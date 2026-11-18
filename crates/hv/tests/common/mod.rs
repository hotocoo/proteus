//! Shared by the on-hardware guest tests.
#![allow(dead_code)]

use std::alloc::{self, Layout};
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

/// Runs `scenario` on its own thread so that a guest that hangs fails the test instead of stalling it.
pub fn within_deadline(scenario: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
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
        // SAFETY: `layout` has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc::alloc_zeroed(layout) }).unwrap();
        Self { ptr, layout }
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
        // SAFETY: allocated in `new` with this layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}
