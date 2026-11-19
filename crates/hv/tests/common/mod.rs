//! Shared by the on-hardware guest tests.
#![allow(dead_code)]

use std::alloc::Layout;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use hv::Gpa;

pub const GPA_BITS: u8 = 36;
pub const RAM_BASE: Gpa = 0x8000_0000;
/// An address with nothing mapped, so every access exits to the VMM.
pub const MMIO_BASE: Gpa = 0x0900_0000;
pub const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
/// These scenarios finish in milliseconds; anything near this long is a guest that never came back.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// Hosted CI runners are themselves virtual machines without nested virtualization, so they set this
/// to say that no hypervisor is expected there. Everywhere else the tests must run.
const SKIP_ENV: &str = "EMU_SKIP_HV_TESTS";

/// False (after saying so) when the environment declares that this machine has no usable hypervisor.
pub fn hypervisor_expected() -> bool {
    let expected = std::env::var_os(SKIP_ENV).is_none_or(|value| value.is_empty());
    if !expected {
        eprintln!("skipped: {SKIP_ENV} is set");
    }
    expected
}

/// Runs `scenario` on its own thread so that a guest that hangs fails the test instead of stalling it.
pub fn within_deadline(scenario: impl FnOnce() + Send + 'static) {
    if !hypervisor_expected() {
        return;
    }
    let (done, finished) = mpsc::channel();
    // WHP denies mapping memory into a second partition of the same process while one is live
    // (the VMM runs one VM per process), so the scenarios take turns.
    static ONE_VM: std::sync::Mutex<()> = std::sync::Mutex::new(());
    std::thread::spawn(move || {
        let _turn = ONE_VM
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scenario();
        let _ = done.send(());
    });
    match finished.recv_timeout(DEADLINE) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("the guest did not finish in time"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the scenario failed; see its panic above")
        }
    }
}

/// Bytes of an assembly block bounded by two linker symbols.
///
/// # Safety
/// `start` and `end` must be the first and one-past-last bytes of one contiguous block in this binary.
pub unsafe fn blob(start: *const u8, end: *const u8) -> &'static [u8] {
    // SAFETY: guaranteed by the caller.
    unsafe { std::slice::from_raw_parts(start, end.offset_from_unsigned(start)) }
}

/// One host page of zeroed, page-aligned memory to back guest RAM.
pub struct GuestPage {
    pub ptr: NonNull<u8>,
    layout: Layout,
}

impl GuestPage {
    pub fn new(page_size: usize) -> Self {
        let layout = Layout::from_size_align(page_size, page_size).unwrap();
        // WHP refuses to map heap pages whose allocation another partition already maps from,
        // so on Windows each guest gets its own allocation, as the VMM's guest memory does.
        #[cfg(windows)]
        // SAFETY: a fresh, zeroed, committed region of `page_size` bytes; freed in `drop`.
        let ptr = unsafe {
            use windows_sys::Win32::System::Memory::*;
            VirtualAlloc(
                std::ptr::null(),
                page_size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        }
        .cast::<u8>();
        #[cfg(not(windows))]
        // SAFETY: `layout` has a non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        Self {
            ptr: NonNull::new(ptr).unwrap(),
            layout,
        }
    }

    pub fn load(&self, code: &[u8]) {
        assert!(code.len() <= self.layout.size());
        // SAFETY: the page is at least `code.len()` bytes and nothing else references it yet.
        unsafe {
            self.ptr
                .copy_from_nonoverlapping(NonNull::from(code).cast(), code.len())
        };
    }
}

impl Drop for GuestPage {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: reserved by `VirtualAlloc` in `new`.
        unsafe {
            windows_sys::Win32::System::Memory::VirtualFree(
                self.ptr.as_ptr().cast(),
                0,
                windows_sys::Win32::System::Memory::MEM_RELEASE,
            );
        }
        #[cfg(not(windows))]
        // SAFETY: allocated in `new` with this layout.
        unsafe {
            std::alloc::dealloc(self.ptr.as_ptr(), self.layout)
        };
    }
}
