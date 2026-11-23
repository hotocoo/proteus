//! virtio-balloon used only for free page reporting: the guest tells the host which of its pages it
//! has freed, and the host gives the memory behind them back to the system. Without it, every page
//! the guest ever touched stays held by this process until it exits, so the emulator's memory only
//! grows. The balloon itself is never inflated (its target stays zero).

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use virtio_bindings::virtio_ids::VIRTIO_ID_BALLOON;
use virtio_queue::{Queue, QueueT};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;

/// The guest reports the pages it frees on a queue of its own.
const F_REPORTING: u64 = 1 << 5;

/// Inflate, deflate, then the reporting queue (the optional queues before it are not offered, and
/// the driver numbers only the queues the device has).
const QUEUE_SIZES: [u16; 3] = [32, 32, 64];
const REPORTING_QUEUE: usize = 2;

/// Configuration space: target pages (u32), pages the guest has given (u32), free page hint
/// command id (u32), poison value (u32). All zero: the balloon is never asked to grow.
const CONFIG_LEN: usize = 16;

enum Wake {
    Work,
    Stop,
}

/// Gives guest RAM back to the host where `madvise` alone cannot: Hypervisor.framework keeps the
/// pages it mapped into the guest charged to this process whatever `madvise` says about them.
pub trait RamReleaser: Send + Sync {
    /// `host..host + len` is the guest RAM at `gpa`, whole host pages the guest has freed and does
    /// not touch until the report is handed back.
    fn release(&self, gpa: u64, host: NonNull<u8>, len: usize);
}

pub struct Balloon {
    worker: Option<(Sender<Wake>, JoinHandle<()>)>,
    releaser: Option<Arc<dyn RamReleaser>>,
}

impl Balloon {
    pub fn new() -> Self {
        Self {
            worker: None,
            releaser: None,
        }
    }

    /// Hands freed pages to `releaser` instead of `madvise`.
    pub fn releasing(releaser: Arc<dyn RamReleaser>) -> Self {
        Self {
            worker: None,
            releaser: Some(releaser),
        }
    }
}

impl Default for Balloon {
    fn default() -> Self {
        Self::new()
    }
}

/// Gives the host memory behind `len` bytes of guest RAM at `addr` back to the system. The guest
/// sees zeros or its old bytes there next time it reads, which is fine: the pages are free.
fn discard(
    mem: &GuestMemoryMmap,
    releaser: Option<&dyn RamReleaser>,
    addr: GuestAddress,
    len: usize,
) {
    // Both ends have to be in guest memory, and contiguous on the host: one region.
    let Some(last) = len
        .checked_sub(1)
        .and_then(|span| addr.0.checked_add(span as u64))
    else {
        return;
    };
    let (Ok(first), Ok(end)) = (
        mem.get_host_address(addr),
        mem.get_host_address(GuestAddress(last)),
    ) else {
        return;
    };
    let start = first as usize;
    if (end as usize).wrapping_sub(start) != len - 1 {
        return;
    }
    // SAFETY: sysconf only reads a system setting.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(0);
    if page == 0 {
        return;
    }
    // Only whole host pages inside the range: a guest page smaller than a host page shares it.
    let first = start.next_multiple_of(page);
    let end = (start + len) / page * page;
    if end <= first {
        return;
    }
    if let Some((releaser, host)) = releaser.zip(NonNull::new(first as *mut u8)) {
        let gpa = addr.0 + (first - start) as u64;
        releaser.release(gpa, host, end - first);
        return;
    }
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE_REUSABLE;
    #[cfg(not(target_os = "macos"))]
    let advice = libc::MADV_DONTNEED;
    // SAFETY: the range is whole pages of guest RAM this process mapped and keeps mapped; dropping
    // their contents is what the guest asked for, and the guest does not use them meanwhile.
    let done = unsafe { libc::madvise(first as *mut libc::c_void, end - first, advice) };
    if done != 0 {
        log::debug!(
            "{}",
            copy::balloon_discard_failed(&std::io::Error::last_os_error())
        );
    }
}

/// Hands back every buffer on the inflate, deflate and reporting queues; for the reporting queue
/// first gives the host memory behind each one back. Returns whether any buffer was used.
fn drain(
    queues: &mut [Queue],
    mem: &GuestMemoryMmap,
    releaser: Option<&dyn RamReleaser>,
) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    for (index, queue) in queues.iter_mut().enumerate() {
        if !queue.ready() {
            continue;
        }
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            if index == REPORTING_QUEUE {
                for buffer in chain {
                    if let Ok(len) = usize::try_from(buffer.len()) {
                        discard(mem, releaser, buffer.addr(), len);
                    }
                }
            }
            queue.add_used(mem, head, 0)?;
            used_any = true;
        }
    }
    Ok(used_any)
}

fn serve(
    mut queues: Vec<Queue>,
    mem: GuestMemoryMmap,
    irq: std::sync::Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
    releaser: Option<Arc<dyn RamReleaser>>,
) {
    while let Ok(Wake::Work) = wake.recv() {
        match drain(&mut queues, &mem, releaser.as_deref()) {
            Ok(true) => irq.signal_used(),
            Ok(false) => {}
            Err(err) => log::warn!("{}", copy::virtio_queue_error(&err)),
        }
    }
}

impl VirtioDevice for Balloon {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_BALLOON
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        F_REPORTING
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let in_range = usize::try_from(offset).is_ok_and(|at| at < CONFIG_LEN);
        if in_range {
            data.fill(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError> {
        if self.worker.is_some() {
            return Err(DeviceError::AlreadyActive);
        }
        if ctx.queues.len() != QUEUE_SIZES.len() {
            return Err(DeviceError::MissingQueue);
        }
        let (wake, wakes) = mpsc::channel();
        let releaser = self.releaser.clone();
        let thread = thread::Builder::new()
            .name(copy::BALLOON_WORKER_NAME.into())
            .spawn(move || serve(ctx.queues, ctx.mem, ctx.irq, wakes, releaser))?;
        self.worker = Some((wake, thread));
        Ok(())
    }

    fn queue_notify(&mut self, _queue: u16) {
        if let Some((wake, _)) = &self.worker {
            let _ = wake.send(Wake::Work);
        }
    }

    fn reset(&mut self) {
        if let Some((wake, thread)) = self.worker.take() {
            let _ = wake.send(Wake::Stop);
            if thread.join().is_err() {
                log::error!("{}", copy::BALLOON_WORKER_GONE);
            }
        }
    }
}

impl Drop for Balloon {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::{Bytes, GuestAddress};

    use super::*;

    const MEM_SIZE: usize = 8 << 20;
    const FREED_AT: u64 = 4 << 20;
    const FREED_LEN: u32 = 2 << 20;

    #[test]
    fn reported_pages_are_handed_back_and_read_as_free() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
        mem.write_slice(&[0xAB; 64], GuestAddress(FREED_AT))
            .unwrap();
        let mock = MockSplitQueue::new(&mem, QUEUE_SIZES[REPORTING_QUEUE]);
        let report = [RawDescriptor::from(Descriptor::new(
            FREED_AT,
            FREED_LEN,
            VRING_DESC_F_WRITE as u16,
            0,
        ))];
        mock.build_desc_chain(&report).unwrap();
        let reporting = mock.create_queue::<Queue>().unwrap();
        let idle = || Queue::new(QUEUE_SIZES[0]).unwrap();
        let mut queues = vec![idle(), idle(), reporting];

        assert!(drain(&mut queues, &mem, None).unwrap());
        assert_eq!(mock.used().idx().load(), 1);
        // Whatever the host gave back, the memory is still there for the guest to use again.
        mem.write_slice(&[1; 64], GuestAddress(FREED_AT)).unwrap();
        let mut back = [0u8; 64];
        mem.read_slice(&mut back, GuestAddress(FREED_AT)).unwrap();
        assert_eq!(back, [1; 64]);
    }

    #[test]
    fn a_releaser_gets_the_whole_host_pages_of_a_report() {
        struct Record(std::sync::Mutex<Vec<(u64, usize)>>);
        impl RamReleaser for Record {
            fn release(&self, gpa: u64, _host: NonNull<u8>, len: usize) {
                self.0.lock().unwrap().push((gpa, len));
            }
        }
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
        let record = Record(std::sync::Mutex::new(Vec::new()));
        discard(
            &mem,
            Some(&record),
            GuestAddress(FREED_AT),
            FREED_LEN as usize,
        );
        // A range smaller than a host page is left alone.
        discard(&mem, Some(&record), GuestAddress(FREED_AT), 1);
        assert_eq!(*record.0.lock().unwrap(), [(FREED_AT, FREED_LEN as usize)]);
    }

    #[test]
    fn config_space_reads_as_an_empty_balloon() {
        let device = Balloon::new();
        let mut config = [0xFF; CONFIG_LEN];
        device.read_config(0, &mut config);
        assert_eq!(config, [0; CONFIG_LEN]);
        assert_eq!(device.features(), F_REPORTING);
        assert_eq!(device.device_type(), VIRTIO_ID_BALLOON);
    }
}
