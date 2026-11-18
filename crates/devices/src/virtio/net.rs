//! virtio-net: moves Ethernet frames between the guest's two queues and a backend that owns the host side.
//!
//! No offloads are offered, so every frame is a plain Ethernet frame preceded by a 12-byte header.
//! Frames from the guest are untrusted: their layout is checked and oversized ones are dropped.

use std::sync::Arc;

use virtio_bindings::virtio_ids::VIRTIO_ID_NET;
use virtio_bindings::virtio_net::VIRTIO_NET_F_MAC;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Address, Bytes, GuestMemoryMmap};

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;

/// Receive queue (host to guest), then transmit queue (guest to host), this many descriptors each.
const QUEUE_SIZES: [u16; 2] = [256, 256];
/// `struct virtio_net_hdr_v1`. With no offload negotiated every field is zero except `num_buffers`.
const HEADER_LEN: usize = 12;
const NUM_BUFFERS_AT: usize = 10;
/// A frame delivered to the guest always occupies one buffer chain.
const ONE_BUFFER: u16 = 1;
/// Largest Ethernet frame without a VLAN tag: 1500 bytes of payload and a 14-byte header.
pub const MAX_FRAME_LEN: usize = 1514;
pub const MAC_LEN: usize = 6;

/// The guest side of the link, handed to the backend once the driver is ready.
pub struct NetLink {
    mem: GuestMemoryMmap,
    rx: Queue,
    tx: Queue,
    irq: Arc<VirtioInterrupt>,
    /// Buffers returned to the guest since the last `flush`.
    used_pending: bool,
}

impl NetLink {
    fn used(&mut self, queue: fn(&mut Self) -> &mut Queue, head: u16, len: u32) {
        let mem = self.mem.clone();
        match queue(self).add_used(&mem, head, len) {
            Ok(()) => self.used_pending = true,
            Err(err) => log::warn!("{}", copy::virtio_queue_error(&err)),
        }
    }

    /// Copies the next frame the guest sent into `frame`, replacing its contents.
    /// Returns false when the guest has nothing more to send. Malformed frames are skipped.
    pub fn next_from_guest(&mut self, frame: &mut Vec<u8>) -> bool {
        let mem = self.mem.clone();
        while let Some(chain) = self.tx.pop_descriptor_chain(&mem) {
            let head = chain.head_index();
            frame.clear();
            let mut header_left = HEADER_LEN;
            let mut well_formed = true;
            for desc in chain {
                let len = desc.len() as usize;
                let skip = header_left.min(len);
                header_left -= skip;
                let start = frame.len();
                let end = start + (len - skip);
                let source = desc.addr().checked_add(skip as u64);
                // A transmit buffer is read by the device, never written.
                if desc.is_write_only() || end > MAX_FRAME_LEN {
                    well_formed = false;
                    break;
                }
                frame.resize(end, 0);
                if source.is_none_or(|addr| mem.read_slice(&mut frame[start..], addr).is_err()) {
                    well_formed = false;
                    break;
                }
            }
            self.used(|link| &mut link.tx, head, 0);
            if well_formed && header_left == 0 && !frame.is_empty() {
                return true;
            }
        }
        false
    }

    /// Delivers one frame to the guest. Returns false, leaving the frame undelivered, when the guest
    /// has posted no receive buffer. A buffer too small for the frame is returned empty and the
    /// frame is dropped, which the guest sees as loss.
    pub fn send_to_guest(&mut self, frame: &[u8]) -> bool {
        let mem = self.mem.clone();
        let Some(chain) = self.rx.pop_descriptor_chain(&mem) else {
            return false;
        };
        let head = chain.head_index();
        let mut header = [0u8; HEADER_LEN];
        header[NUM_BUFFERS_AT..].copy_from_slice(&ONE_BUFFER.to_le_bytes());
        let mut packet = Vec::with_capacity(HEADER_LEN + frame.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(frame);

        let mut left = packet.as_slice();
        for desc in chain {
            if left.is_empty() {
                break;
            }
            let take = left.len().min(desc.len() as usize);
            // A receive buffer must be writable by the device.
            if !desc.is_write_only() || mem.write_slice(&left[..take], desc.addr()).is_err() {
                break;
            }
            left = &left[take..];
        }
        let delivered = if left.is_empty() {
            packet.len() as u32
        } else {
            0
        };
        self.used(|link| &mut link.rx, head, delivered);
        true
    }

    /// Tells the guest about buffers returned since the last call, if it asked to be told.
    pub fn flush(&mut self) {
        if !std::mem::take(&mut self.used_pending) {
            return;
        }
        let mem = self.mem.clone();
        let wants = |queue: &mut Queue| queue.needs_notification(&mem).unwrap_or(true);
        if wants(&mut self.rx) | wants(&mut self.tx) {
            self.irq.signal_used();
        }
    }
}

/// The host side of a virtio-net device.
pub trait NetBackend: Send {
    /// The driver is ready; take over the link.
    fn start(&mut self, link: NetLink) -> Result<(), DeviceError>;
    /// The guest queued frames or receive buffers. Runs on a vCPU thread and must not block.
    fn notify(&mut self);
    /// The driver reset the device; give the link up.
    fn stop(&mut self);
}

pub struct Net<B: NetBackend> {
    backend: B,
    mac: [u8; MAC_LEN],
}

impl<B: NetBackend> Net<B> {
    pub fn new(backend: B, mac: [u8; MAC_LEN]) -> Self {
        Self { backend, mac }
    }
}

impl<B: NetBackend> VirtioDevice for Net<B> {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_NET
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_NET_F_MAC
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // `struct virtio_net_config` starts with the MAC address; the fields after it are unused.
        for (at, byte) in (offset..).zip(data.iter_mut()) {
            *byte = usize::try_from(at)
                .ok()
                .and_then(|at| self.mac.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError> {
        let mut queues = ctx.queues.into_iter();
        let (Some(rx), Some(tx)) = (queues.next(), queues.next()) else {
            return Err(DeviceError::MissingQueue);
        };
        self.backend.start(NetLink {
            mem: ctx.mem,
            rx,
            tx,
            irq: ctx.irq,
            used_pending: false,
        })
    }

    fn queue_notify(&mut self, _queue: u16) {
        self.backend.notify();
    }

    fn reset(&mut self) {
        self.backend.stop();
    }
}

#[cfg(test)]
mod tests {
    use virtio_bindings::virtio_mmio::VIRTIO_MMIO_INT_VRING;
    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::GuestAddress;

    use super::super::testing::LineLog;
    use super::*;

    const MEM_SIZE: usize = 4 << 20;
    const QUEUE_LEN: u16 = 16;
    /// The two mock queues keep their rings here; buffers live above `BUFFERS_AT`.
    const RX_RINGS_AT: u64 = 0;
    const TX_RINGS_AT: u64 = 0x1_0000;
    const BUFFERS_AT: u64 = 0x10_0000;
    const MAC: [u8; MAC_LEN] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const NEXT: u16 = 1;
    const WRITE: u16 = VRING_DESC_F_WRITE as u16;

    struct Rig {
        mem: GuestMemoryMmap,
        line: Arc<LineLog>,
    }

    impl Rig {
        fn new() -> Self {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
            Self {
                mem,
                line: Arc::new(LineLog::default()),
            }
        }

        fn queues(
            &self,
        ) -> (
            MockSplitQueue<'_, GuestMemoryMmap>,
            MockSplitQueue<'_, GuestMemoryMmap>,
        ) {
            (
                MockSplitQueue::create(&self.mem, GuestAddress(RX_RINGS_AT), QUEUE_LEN),
                MockSplitQueue::create(&self.mem, GuestAddress(TX_RINGS_AT), QUEUE_LEN),
            )
        }

        fn link(
            &self,
            rx: &MockSplitQueue<'_, GuestMemoryMmap>,
            tx: &MockSplitQueue<'_, GuestMemoryMmap>,
        ) -> NetLink {
            NetLink {
                mem: self.mem.clone(),
                rx: rx.create_queue().unwrap(),
                tx: tx.create_queue().unwrap(),
                irq: VirtioInterrupt::new(self.line.clone()),
                used_pending: false,
            }
        }

        fn put(&self, addr: u64, bytes: &[u8]) {
            self.mem.write_slice(bytes, GuestAddress(addr)).unwrap();
        }

        fn get(&self, addr: u64, len: usize) -> Vec<u8> {
            let mut bytes = vec![0; len];
            self.mem.read_slice(&mut bytes, GuestAddress(addr)).unwrap();
            bytes
        }
    }

    fn desc(addr: u64, len: usize, flags: u16, next: u16) -> RawDescriptor {
        RawDescriptor::from(Descriptor::new(addr, len as u32, flags, next))
    }

    fn used_len(
        queue: &MockSplitQueue<'_, GuestMemoryMmap>,
        mem: &GuestMemoryMmap,
        index: u64,
    ) -> u32 {
        // Used ring: flags (u16), idx (u16), then {id: u32, len: u32} entries.
        mem.read_obj(queue.used_addr().unchecked_add(4 + index * 8 + 4))
            .unwrap()
    }

    #[test]
    fn frame_from_the_guest_is_reassembled_without_its_header() {
        let rig = Rig::new();
        let (rx, tx) = rig.queues();
        let frame: Vec<u8> = (0..100u8).collect();
        // Header split across two buffers, with the frame continuing in a third.
        rig.put(BUFFERS_AT, &[0; 8]);
        rig.put(
            BUFFERS_AT + 0x100,
            &[[0u8; 4].as_slice(), &frame[..10]].concat(),
        );
        rig.put(BUFFERS_AT + 0x200, &frame[10..]);
        tx.add_desc_chains(
            &[
                desc(BUFFERS_AT, 8, NEXT, 1),
                desc(BUFFERS_AT + 0x100, 14, NEXT, 2),
                desc(BUFFERS_AT + 0x200, 90, 0, 0),
            ],
            0,
        )
        .unwrap();
        let mut link = rig.link(&rx, &tx);

        let mut got = vec![0xff; 3];
        assert!(link.next_from_guest(&mut got));
        assert_eq!(got, frame);
        assert!(!link.next_from_guest(&mut got));
    }

    #[test]
    fn malformed_guest_frames_are_skipped_and_the_next_good_one_is_returned() {
        let rig = Rig::new();
        let (rx, tx) = rig.queues();
        let good = [0xabu8; 60];
        rig.put(
            BUFFERS_AT + 0x3000,
            &[[0u8; HEADER_LEN].as_slice(), &good].concat(),
        );
        tx.add_desc_chains(
            &[
                // Oversized: more than a full Ethernet frame after the header.
                desc(BUFFERS_AT, HEADER_LEN + MAX_FRAME_LEN + 1, 0, 0),
                // Shorter than the header.
                desc(BUFFERS_AT, HEADER_LEN - 1, 0, 0),
                // Header only, no frame.
                desc(BUFFERS_AT, HEADER_LEN, 0, 0),
                // A buffer the device is supposed to write, on the transmit queue.
                desc(BUFFERS_AT, 64, WRITE, 0),
                // Outside guest memory.
                desc(MEM_SIZE as u64, 64, 0, 0),
                desc(BUFFERS_AT + 0x3000, HEADER_LEN + good.len(), 0, 0),
            ],
            0,
        )
        .unwrap();
        let mut link = rig.link(&rx, &tx);

        let mut got = Vec::new();
        assert!(link.next_from_guest(&mut got));
        assert_eq!(got, good);
        assert!(!link.next_from_guest(&mut got));
    }

    #[test]
    fn frame_to_the_guest_gets_a_header_and_spans_buffers() {
        let rig = Rig::new();
        let (rx, tx) = rig.queues();
        rx.add_desc_chains(
            &[
                desc(BUFFERS_AT, 16, WRITE | NEXT, 1),
                desc(BUFFERS_AT + 0x100, 200, WRITE, 0),
            ],
            0,
        )
        .unwrap();
        let mut link = rig.link(&rx, &tx);
        let frame: Vec<u8> = (1..=80u8).collect();

        assert!(link.send_to_guest(&frame));
        let mut header = [0u8; HEADER_LEN];
        header[NUM_BUFFERS_AT] = 1;
        assert_eq!(
            rig.get(BUFFERS_AT, 16),
            [header.as_slice(), &frame[..4]].concat()
        );
        assert_eq!(rig.get(BUFFERS_AT + 0x100, 76), frame[4..]);
        assert_eq!(
            used_len(&rx, &rig.mem, 0) as usize,
            HEADER_LEN + frame.len()
        );

        // No second buffer was posted.
        assert!(!link.send_to_guest(&frame));
    }

    #[test]
    fn frame_too_large_for_the_guest_buffer_is_dropped_not_truncated() {
        let rig = Rig::new();
        let (rx, tx) = rig.queues();
        rx.add_desc_chains(&[desc(BUFFERS_AT, 32, WRITE, 0)], 0)
            .unwrap();
        let mut link = rig.link(&rx, &tx);

        assert!(link.send_to_guest(&[7u8; 100]));
        assert_eq!(used_len(&rx, &rig.mem, 0), 0);
    }

    #[test]
    fn interrupt_is_raised_once_per_flush_and_only_after_work() {
        let rig = Rig::new();
        let (rx, tx) = rig.queues();
        rx.add_desc_chains(&[desc(BUFFERS_AT, 128, WRITE, 0)], 0)
            .unwrap();
        let mut link = rig.link(&rx, &tx);

        link.flush();
        assert_eq!(rig.line.level(), None, "nothing was used yet");
        assert!(link.send_to_guest(&[1u8; 60]));
        link.flush();
        assert_eq!(rig.line.level(), Some(true));
        assert_eq!(link.irq.status(), VIRTIO_MMIO_INT_VRING);
        link.flush();
        assert_eq!(rig.line.0.lock().unwrap().len(), 1);
    }

    struct Recorder(Arc<std::sync::Mutex<Vec<&'static str>>>);

    impl NetBackend for Recorder {
        fn start(&mut self, _link: NetLink) -> Result<(), DeviceError> {
            self.0.lock().unwrap().push("start");
            Ok(())
        }

        fn notify(&mut self) {
            self.0.lock().unwrap().push("notify");
        }

        fn stop(&mut self) {
            self.0.lock().unwrap().push("stop");
        }
    }

    #[test]
    fn device_forwards_lifecycle_to_the_backend_and_reports_its_mac() {
        let rig = Rig::new();
        let (rx, tx) = rig.queues();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut net = Net::new(Recorder(calls.clone()), MAC);

        let mut config = [0xffu8; 8];
        net.read_config(0, &mut config);
        assert_eq!(config, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56, 0, 0]);
        assert_ne!(net.features() & 1 << VIRTIO_NET_F_MAC, 0);

        let ctx = |queues| Activate {
            mem: rig.mem.clone(),
            queues,
            irq: VirtioInterrupt::new(rig.line.clone()),
            features: 0,
        };
        assert!(matches!(
            net.activate(ctx(vec![rx.create_queue().unwrap()])),
            Err(DeviceError::MissingQueue)
        ));
        net.activate(ctx(vec![
            rx.create_queue().unwrap(),
            tx.create_queue().unwrap(),
        ]))
        .unwrap();
        net.queue_notify(1);
        net.reset();
        assert_eq!(*calls.lock().unwrap(), ["start", "notify", "stop"]);
    }
}
