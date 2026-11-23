//! macOS: keeps what guest RAM costs in Activity Monitor equal to what the guest really uses.
//!
//! Hypervisor.framework charges a page of guest RAM to this process once for the guest's mapping
//! and once more for this process's own mapping as soon as the emulator touches it (a disk read
//! into a guest buffer, a GPU transfer): 1 GB touched by both shows as 2 GB, though it is one
//! copy in memory. And it keeps every page it ever mapped charged, freed by the guest or not.
//!
//! So guest RAM gets a second, hidden mapping (the alias) that nothing reads or writes. Every
//! `config::HOST_RAM_REFRESH_PERIOD` the program's own view is mapped afresh from the alias, in
//! place and atomically: the same memory, but none of the program's page table entries, so each
//! page counts once again (the emulator's next touch maps it back, cheaply). Pages the guest frees
//! are taken out of the guest, replaced with fresh memory in both mappings, and put back.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use hv::{Prot, Vm};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::config::{HOST_RAM_REFRESH_CHUNK, HOST_RAM_REFRESH_PERIOD};
use crate::copy;

type KernReturn = libc::c_int;
const KERN_SUCCESS: KernReturn = 0;
const VM_FLAGS_FIXED: libc::c_int = 0;
const VM_FLAGS_ANYWHERE: libc::c_int = 1;
const VM_FLAGS_OVERWRITE: libc::c_int = 0x4000;
const VM_INHERIT_SHARE: libc::c_uint = 0;

unsafe extern "C" {
    fn mach_vm_remap(
        target_task: libc::mach_port_t,
        target_address: *mut u64,
        size: u64,
        mask: u64,
        flags: libc::c_int,
        src_task: libc::mach_port_t,
        src_address: u64,
        copy: libc::c_int,
        cur_protection: *mut libc::c_int,
        max_protection: *mut libc::c_int,
        inheritance: libc::c_uint,
    ) -> KernReturn;
    fn mach_vm_deallocate(target: libc::mach_port_t, address: u64, size: u64) -> KernReturn;
    static mach_task_self_: libc::mach_port_t;
}

/// This process's task port (what `mach_task_self()` returns in C).
fn task() -> libc::mach_port_t {
    // SAFETY: set up by the system before `main` and never changed.
    unsafe { mach_task_self_ }
}

/// Maps `len` bytes at `from` again at `to` (anywhere when `to` is `None`), sharing the memory.
/// Mapping over an existing range replaces it in one step: a thread touching it meanwhile waits
/// for the new mapping instead of finding a hole.
fn share(from: usize, to: Option<usize>, len: usize) -> Result<usize, KernReturn> {
    let mut target = to.unwrap_or(0) as u64;
    let flags = match to {
        Some(_) => VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
        None => VM_FLAGS_ANYWHERE,
    };
    let (mut current, mut max) = (0, 0);
    // SAFETY: both ranges are mappings of this process that the caller owns; the call only
    // changes which pages back `target`, and the pages are the same ones `from` maps.
    let kr = unsafe {
        mach_vm_remap(
            task(),
            &mut target,
            len as u64,
            0,
            flags,
            task(),
            from as u64,
            0,
            &mut current,
            &mut max,
            VM_INHERIT_SHARE,
        )
    };
    if kr == KERN_SUCCESS {
        Ok(target as usize)
    } else {
        Err(kr)
    }
}

struct Region {
    host: usize,
    alias: usize,
    len: usize,
}

pub struct HostRam<V: Vm> {
    vm: Arc<V>,
    regions: Vec<Region>,
    /// Releases and refreshes never overlap: a refresh between a release's two steps would map
    /// the old pages back.
    lock: Mutex<()>,
    /// Off for good once the alias stops matching guest RAM; refreshing would then map stale pages.
    in_step: AtomicBool,
    /// Keeps guest RAM mapped for as long as this can replace pages in it; dropped after `vm`.
    _mem: GuestMemoryMmap,
}

impl<V: Vm> HostRam<V> {
    /// An alias for each range of guest RAM; `None` if one cannot be made.
    pub fn new(vm: Arc<V>, mem: GuestMemoryMmap, ranges: &[(GuestAddress, usize)]) -> Option<Self> {
        let mut regions = Vec::new();
        for &(start, len) in ranges {
            let host = mem.get_host_address(start).ok()? as usize;
            match share(host, None, len) {
                Ok(alias) => regions.push(Region { host, alias, len }),
                Err(kr) => {
                    log::warn!("{}", copy::ram_alias_failed(kr));
                    return None;
                }
            }
        }
        Some(Self {
            vm,
            regions,
            lock: Mutex::new(()),
            in_step: AtomicBool::new(true),
            _mem: mem,
        })
    }

    /// Maps the program's view of guest RAM afresh from the alias, a chunk at a time.
    fn refresh(&self) {
        for region in &self.regions {
            for offset in (0..region.len).step_by(HOST_RAM_REFRESH_CHUNK) {
                let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
                if !self.in_step.load(Ordering::SeqCst) {
                    return;
                }
                let len = HOST_RAM_REFRESH_CHUNK.min(region.len - offset);
                if let Err(kr) = share(region.alias + offset, Some(region.host + offset), len) {
                    log::debug!("{}", copy::ram_refresh_failed(kr));
                }
            }
        }
    }

    /// Refreshes every `HOST_RAM_REFRESH_PERIOD` on a thread of its own, until `ram` is dropped.
    pub fn keep_refreshing(ram: &Arc<Self>) -> std::io::Result<()>
    where
        V: 'static,
    {
        let weak: Weak<Self> = Arc::downgrade(ram);
        std::thread::Builder::new()
            .name(copy::HOST_RAM_THREAD.into())
            .spawn(move || {
                loop {
                    std::thread::sleep(HOST_RAM_REFRESH_PERIOD);
                    match weak.upgrade() {
                        Some(ram) => ram.refresh(),
                        None => return,
                    }
                }
            })
            .map(drop)
    }
}

impl<V: Vm> devices::virtio::balloon::RamReleaser for HostRam<V> {
    fn release(&self, gpa: u64, host: NonNull<u8>, len: usize) {
        let at = host.as_ptr() as usize;
        let Some(region) = self
            .regions
            .iter()
            .find(|region| at >= region.host && at + len <= region.host + region.len)
        else {
            return;
        };
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = self.vm.unmap(gpa, len) {
            log::warn!("{}", copy::ram_release_failed(&err));
            return;
        }
        // SAFETY: `host..host+len` is whole pages of guest RAM that `mem` mapped and keeps mapped;
        // the guest has freed them and the VM no longer maps them, so nothing reads them while
        // they are replaced in place by fresh anonymous memory.
        let fresh = unsafe {
            libc::mmap(
                host.as_ptr().cast(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if fresh == libc::MAP_FAILED {
            log::warn!(
                "{}",
                copy::ram_release_failed(&std::io::Error::last_os_error())
            );
        } else if let Err(kr) = share(at, Some(region.alias + (at - region.host)), len) {
            // The alias still holds the old pages: refreshing from it would undo this release.
            self.in_step.store(false, Ordering::SeqCst);
            log::warn!("{}", copy::ram_alias_failed(kr));
        }
        // SAFETY: as when the VM was built: `mem` owns this range, never moves it, outlives `vm`.
        if let Err(err) = unsafe { self.vm.map(gpa, host, len, Prot::RWX) } {
            log::error!("{}", copy::ram_release_failed(&err));
        }
    }
}

impl<V: Vm> Drop for HostRam<V> {
    fn drop(&mut self) {
        for region in &self.regions {
            // SAFETY: the alias is this struct's own mapping and nothing uses it any more.
            unsafe { mach_vm_deallocate(task(), region.alias as u64, region.len as u64) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: usize = 4 << 20;

    #[test]
    fn a_refreshed_view_keeps_the_memory_and_shares_it_with_the_alias() {
        // SAFETY: a fresh private anonymous mapping, owned by this test.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        } as usize;
        assert_ne!(base as *mut libc::c_void, libc::MAP_FAILED);
        let view = base as *mut u8;
        // SAFETY: inside the mapping made above.
        unsafe { view.add(100).write(7) };
        let alias = share(base, None, LEN).unwrap();
        // Remapping the view from the alias, as a refresh does, keeps what was written.
        assert_eq!(share(alias, Some(base), LEN), Ok(base));
        // SAFETY: both addresses are inside mappings this test owns.
        unsafe {
            assert_eq!(view.add(100).read(), 7);
            view.add(200).write(9);
            assert_eq!((alias as *const u8).add(200).read(), 9);
            mach_vm_deallocate(task(), alias as u64, LEN as u64);
            libc::munmap(base as *mut libc::c_void, LEN);
        }
    }
}
