//! Memory a frame is read into that the GPU then samples where it lies.
//!
//! Reading a frame into a `Vec` and uploading it makes the renderer's copy and a second one, of
//! the whole frame, on the window's thread. Where the GPU and the CPU share memory (Apple
//! silicon), a frame can instead be read straight into a Metal buffer the window wraps as a
//! texture, so the renderer's copy is the only one. Other hosts, and any failure, use the `Vec`.

use std::sync::{Arc, Mutex, PoisonError};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use metal::{SharedBuffer, SharedPool};
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub use none::{SharedBuffer, SharedPool};

/// Slots handed out in turn, so a slot is reused as late as it can be.
pub struct Ring<T> {
    slots: Vec<Arc<T>>,
    /// Where the next search starts.
    next: usize,
}

impl<T> Default for Ring<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            next: 0,
        }
    }
}

impl<T> Ring<T> {
    /// A slot nobody holds but the ring that `fits`. When every free one is the wrong kind it is
    /// replaced with one `make` builds, and a ring below `capacity` grows by one. `None` when
    /// every slot is held, or `make` fails.
    pub fn take(
        &mut self,
        capacity: usize,
        fits: impl Fn(&T) -> bool,
        make: impl FnOnce() -> Option<T>,
    ) -> Option<Arc<T>> {
        let count = self.slots.len();
        let mut stale = None;
        for step in 0..count {
            let at = (self.next + step) % count;
            let slot = &self.slots[at];
            if Arc::strong_count(slot) != 1 || Arc::weak_count(slot) != 0 {
                continue;
            }
            if fits(slot) {
                self.next = at + 1;
                return Some(Arc::clone(slot));
            }
            stale.get_or_insert(at);
        }
        let made = Arc::new(make()?);
        match stale {
            _ if count < capacity => self.slots.push(Arc::clone(&made)),
            Some(at) => self.slots[at] = Arc::clone(&made),
            None => return None,
        }
        Some(made)
    }
}

/// Locks `mutex` even if a thread panicked holding it: its data is a list of slots, whole either way.
pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod metal {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use devices::virtio::gpu::BYTES_PER_PIXEL;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{
        MTLBuffer, MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLResourceOptions,
        MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    };

    use super::{Ring, lock};
    use crate::config::SHARED_FRAME_SLOTS;

    /// Tells slots apart, so a window can keep what it made from one.
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    /// One frame's memory, a shared-storage Metal buffer.
    pub struct SharedBuffer {
        id: u64,
        buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
        len: usize,
    }

    // SAFETY: a Metal buffer may be used from any thread. Its bytes are written by one holder at a
    // time: the pool lends a slot only while nobody else holds it, and the holder that filled it
    // shares it, read-only, once it is shown.
    unsafe impl Send for SharedBuffer {}
    // SAFETY: as above; shared references only read.
    unsafe impl Sync for SharedBuffer {}

    impl SharedBuffer {
        pub fn id(&self) -> u64 {
            self.id
        }

        pub fn bytes(&self) -> &[u8] {
            // SAFETY: `contents` points at `len` bytes that live as long as the buffer, and no
            // one writes them while this shared reference exists (see the `Send` impl above).
            unsafe { std::slice::from_raw_parts(self.buffer.contents().as_ptr().cast(), self.len) }
        }

        /// The bytes, to be filled.
        ///
        /// # Safety
        /// The caller is the only holder of this buffer, besides the pool that lent it.
        #[allow(clippy::mut_from_ref)] // The buffer is shared memory; the caller's promise is the lock.
        pub unsafe fn bytes_mut(&self) -> &mut [u8] {
            // SAFETY: as `bytes`; exclusivity is the caller's.
            unsafe {
                std::slice::from_raw_parts_mut(self.buffer.contents().as_ptr().cast(), self.len)
            }
        }

        /// A texture over the buffer's bytes as they lie, `width` pixels across: rows are
        /// tightly packed, as the renderer writes them. `None` if Metal refuses that row length.
        pub fn texture(
            &self,
            format: MTLPixelFormat,
            width: u32,
            height: u32,
        ) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
            // SAFETY: plain descriptor and texture construction; the size is nonzero (`acquire`
            // refuses an empty frame), the format is a colour one, and the row is the buffer's
            // own packing.
            unsafe {
                let descriptor =
                    MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                        format,
                        width as usize,
                        height as usize,
                        false,
                    );
                descriptor.setStorageMode(MTLStorageMode::Shared);
                descriptor.setUsage(MTLTextureUsage::ShaderRead);
                let row = width as usize * BYTES_PER_PIXEL;
                self.buffer
                    .newTextureWithDescriptor_offset_bytesPerRow(&descriptor, 0, row)
            }
        }
    }

    /// The buffers frames are read into.
    pub struct SharedPool {
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        ring: std::sync::Mutex<Ring<SharedBuffer>>,
    }

    // SAFETY: Metal devices are thread-safe.
    unsafe impl Send for SharedPool {}
    // SAFETY: as above; the ring is behind its own lock.
    unsafe impl Sync for SharedPool {}

    impl SharedPool {
        /// A pool on the system's GPU, which the window's renderer draws with too (Apple silicon
        /// has one). `None` if there is none.
        pub fn new() -> Option<Self> {
            Some(Self {
                device: MTLCreateSystemDefaultDevice()?,
                ring: std::sync::Mutex::new(Ring::default()),
            })
        }

        /// A free buffer for a `width` by `height` frame. `None` when every one is in use or Metal
        /// cannot make a texture of that row length (the row must be a multiple of the GPU's
        /// alignment), and the frame goes the ordinary way.
        pub fn acquire(&self, width: u32, height: u32) -> Option<Arc<SharedBuffer>> {
            let row = (width as usize).checked_mul(BYTES_PER_PIXEL)?;
            let len = row.checked_mul(height as usize)?;
            let align = self
                .device
                .minimumLinearTextureAlignmentForPixelFormat(MTLPixelFormat::BGRA8Unorm);
            if len == 0 || row % align != 0 {
                return None;
            }
            lock(&self.ring).take(
                SHARED_FRAME_SLOTS,
                |slot| slot.len == len,
                || {
                    let buffer = self
                        .device
                        .newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared)?;
                    Some(SharedBuffer {
                        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                        buffer,
                        len,
                    })
                },
            )
        }
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod none {
    use std::sync::Arc;

    /// No host has shared memory but Apple silicon; this one is never made.
    pub enum SharedBuffer {}

    impl SharedBuffer {
        pub fn id(&self) -> u64 {
            match *self {}
        }

        pub fn bytes(&self) -> &[u8] {
            match *self {}
        }

        /// # Safety
        /// Never callable: there is no value to call it on.
        #[allow(clippy::mut_from_ref)]
        pub unsafe fn bytes_mut(&self) -> &mut [u8] {
            match *self {}
        }
    }

    pub struct SharedPool;

    impl SharedPool {
        pub fn new() -> Option<Self> {
            None
        }

        pub fn acquire(&self, _width: u32, _height: u32) -> Option<Arc<SharedBuffer>> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(ring: &mut Ring<u32>, want: u32) -> Option<Arc<u32>> {
        ring.take(3, |slot| *slot == want, || Some(want))
    }

    #[test]
    fn a_ring_grows_to_its_capacity_then_reuses_what_nobody_holds() {
        let mut ring = Ring::default();
        let held: Vec<_> = (0..3).map(|_| take(&mut ring, 1).unwrap()).collect();
        assert!(take(&mut ring, 1).is_none(), "every slot is held");
        let order: Vec<_> = held.iter().map(Arc::as_ptr).collect();
        drop(held);
        // Slots come round in turn, each one after the last handed out.
        let again: Vec<_> = (0..3)
            .map(|_| Arc::as_ptr(&take(&mut ring, 1).unwrap()))
            .collect();
        assert_eq!(again, order);
        assert_eq!(ring.slots.len(), 3);
    }

    #[test]
    fn a_held_slot_is_never_handed_out_twice() {
        let mut ring = Ring::default();
        let first = take(&mut ring, 1).unwrap();
        let second = take(&mut ring, 1).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn a_free_slot_of_the_wrong_kind_is_replaced() {
        let mut ring = Ring::default();
        drop(
            (0..3)
                .map(|_| take(&mut ring, 1).unwrap())
                .collect::<Vec<_>>(),
        );
        let other = take(&mut ring, 2).unwrap();
        assert_eq!(*other, 2);
        assert_eq!(ring.slots.len(), 3);
    }

    #[test]
    fn a_failed_make_hands_out_nothing() {
        let mut ring: Ring<u32> = Ring::default();
        assert!(ring.take(3, |_| true, || None).is_none());
        assert!(ring.slots.is_empty());
    }
}
