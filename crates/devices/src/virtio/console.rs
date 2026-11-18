//! virtio-console with several ports, each of which the guest sees as one `/dev/hvcN` terminal.
//!
//! A port carries bytes in both directions. What the guest writes goes to the port's sink, if it has
//! one, and is dropped otherwise; what the host sends through the `ConsoleHandle` is what the guest
//! reads. Android's hardware services expect a fixed set of these terminals to exist, whether or not
//! anything on the host answers.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use virtio_bindings::virtio_ids::VIRTIO_ID_CONSOLE;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;

/// The device has more than one port and a pair of control queues.
const F_MULTIPORT: u64 = 1 << 1;

// Control events, as the specification numbers them.
const DEVICE_READY: u16 = 0;
const PORT_ADD: u16 = 1;
const PORT_READY: u16 = 3;
const CONSOLE_PORT: u16 = 4;
const PORT_OPEN: u16 = 6;
/// A control message: port number (u32), event (u16), value (u16), all little-endian.
const CONTROL_LEN: usize = 8;
/// The value that means "yes" in the control messages used here.
const YES: u16 = 1;

/// Configuration space: columns (u16), rows (u16), number of ports (u32), emergency write (u32).
const CONFIG_LEN: usize = 12;
const CONFIG_PORTS_AT: usize = 4;

const PORT_QUEUE_SIZE: u16 = 16;
const CONTROL_QUEUE_SIZE: u16 = 32;
const CONTROL_RX: usize = 2;
const CONTROL_TX: usize = 3;
/// Most bytes kept for the guest per port while it is not reading; older bytes are dropped first.
const MAX_PENDING_BYTES: usize = 64 << 10;
/// Most bytes taken from one guest buffer in one step.
const CHUNK: usize = 4096;

/// Where a port's bytes from the guest go.
pub type PortSink = Box<dyn Write + Send>;

/// Queue that carries bytes to the guest on `port`. The queue for bytes from the guest follows it.
/// Port 0 has the first pair and the control queues the second; that is how the specification lays
/// them out.
const fn receive_queue(port: usize) -> usize {
    if port == 0 { 0 } else { 2 * port + 2 }
}

fn control(port: u32, event: u16, value: u16) -> [u8; CONTROL_LEN] {
    let mut message = [0u8; CONTROL_LEN];
    message[..4].copy_from_slice(&port.to_le_bytes());
    message[4..6].copy_from_slice(&event.to_le_bytes());
    message[6..].copy_from_slice(&value.to_le_bytes());
    message
}

enum Wake {
    Work,
    Stop,
}

struct Shared {
    /// Bytes waiting for the guest to read, per port.
    to_guest: Mutex<Vec<VecDeque<u8>>>,
    /// Present while the driver is active.
    wake: Mutex<Option<Sender<Wake>>>,
}

impl Shared {
    fn wake(&self) {
        let wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(wake) = wake.as_ref() {
            let _ = wake.send(Wake::Work);
        }
    }
}

/// The host's end of the ports. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct ConsoleHandle(Arc<Shared>);

impl ConsoleHandle {
    /// Queues bytes for the guest to read from `port`. If the guest is not reading, the oldest bytes
    /// make room. Returns false for a port the device does not have.
    pub fn send(&self, port: usize, bytes: &[u8]) -> bool {
        let mut to_guest = self
            .0
            .to_guest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(pending) = to_guest.get_mut(port) else {
            return false;
        };
        pending.extend(bytes);
        let excess = pending.len().saturating_sub(MAX_PENDING_BYTES);
        pending.drain(..excess);
        drop(to_guest);
        self.0.wake();
        true
    }
}

/// The worker's side: the queues, the sinks, and control messages not yet delivered.
struct Ports {
    shared: Arc<Shared>,
    sinks: Vec<Option<PortSink>>,
    queues: Vec<Queue>,
    control: VecDeque<[u8; CONTROL_LEN]>,
}

impl Ports {
    /// Reads the control messages the driver sent and queues the answers.
    fn read_control(&mut self, mem: &GuestMemoryMmap) -> Result<bool, virtio_queue::Error> {
        let mut used_any = false;
        let Some(queue) = self
            .queues
            .get_mut(CONTROL_TX)
            .filter(|queue| queue.ready())
        else {
            return Ok(false);
        };
        while let Some(mut chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let mut message = [0u8; CONTROL_LEN];
            let complete = chain.next().is_some_and(|buffer| {
                !buffer.is_write_only()
                    && buffer.len() as usize >= CONTROL_LEN
                    && mem.read_slice(&mut message, buffer.addr()).is_ok()
            });
            queue.add_used(mem, head, 0)?;
            used_any = true;
            if !complete {
                continue;
            }
            let port = u32::from_le_bytes([message[0], message[1], message[2], message[3]]);
            let event = u16::from_le_bytes([message[4], message[5]]);
            let value = u16::from_le_bytes([message[6], message[7]]);
            match (event, value) {
                (DEVICE_READY, YES) => {
                    let ports = 0..self.sinks.len() as u32;
                    self.control
                        .extend(ports.map(|port| control(port, PORT_ADD, YES)));
                }
                // Every port is a terminal, and its host end is always open.
                (PORT_READY, YES) if (port as usize) < self.sinks.len() => {
                    self.control.push_back(control(port, CONSOLE_PORT, YES));
                    self.control.push_back(control(port, PORT_OPEN, YES));
                }
                _ => {}
            }
        }
        Ok(used_any)
    }

    /// Hands queued control messages to the driver, one per buffer it has posted.
    fn write_control(&mut self, mem: &GuestMemoryMmap) -> Result<bool, virtio_queue::Error> {
        let mut used_any = false;
        let Some(queue) = self
            .queues
            .get_mut(CONTROL_RX)
            .filter(|queue| queue.ready())
        else {
            return Ok(false);
        };
        while let Some(message) = self.control.front() {
            let Some(mut chain) = queue.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            let written = chain.next().is_some_and(|buffer| {
                buffer.is_write_only()
                    && buffer.len() as usize >= CONTROL_LEN
                    && mem.write_slice(message, buffer.addr()).is_ok()
            });
            // An unusable buffer is handed back empty and the message waits for the next one.
            if written {
                self.control.pop_front();
            }
            queue.add_used(mem, head, if written { CONTROL_LEN as u32 } else { 0 })?;
            used_any = true;
        }
        Ok(used_any)
    }

    /// Takes everything the guest wrote to `port` and passes it to the port's sink.
    fn drain_guest(
        &mut self,
        port: usize,
        mem: &GuestMemoryMmap,
    ) -> Result<bool, virtio_queue::Error> {
        let mut used_any = false;
        let queue = self.queues.get_mut(receive_queue(port) + 1);
        let Some(queue) = queue.filter(|queue| queue.ready()) else {
            return Ok(false);
        };
        let sink = &mut self.sinks[port];
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            for buffer in chain.filter(|buffer| !buffer.is_write_only()) {
                let Some(sink) = sink.as_mut() else {
                    continue;
                };
                let mut chunk = [0u8; CHUNK];
                let mut done = 0;
                while done < buffer.len() as usize {
                    let step = (buffer.len() as usize - done).min(CHUNK);
                    let Some(from) = buffer.addr().0.checked_add(done as u64) else {
                        break;
                    };
                    if mem
                        .read_slice(&mut chunk[..step], vm_memory::GuestAddress(from))
                        .is_err()
                    {
                        break;
                    }
                    // A sink that fails (a full disk, a closed pipe) loses output, not the guest.
                    let _ = sink.write_all(&chunk[..step]);
                    done += step;
                }
            }
            if let Some(sink) = sink.as_mut() {
                let _ = sink.flush();
            }
            queue.add_used(mem, head, 0)?;
            used_any = true;
        }
        Ok(used_any)
    }

    /// Moves bytes waiting for `port` into the buffers the guest has posted for it.
    fn feed_guest(
        &mut self,
        port: usize,
        mem: &GuestMemoryMmap,
    ) -> Result<bool, virtio_queue::Error> {
        let mut used_any = false;
        let queue = self.queues.get_mut(receive_queue(port));
        let Some(queue) = queue.filter(|queue| queue.ready()) else {
            return Ok(false);
        };
        let mut to_guest = self
            .shared
            .to_guest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pending = &mut to_guest[port];
        while !pending.is_empty() {
            let Some(mut chain) = queue.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            let mut written = 0;
            if let Some(buffer) = chain.next().filter(|buffer| buffer.is_write_only()) {
                let step = pending.len().min(buffer.len() as usize).min(CHUNK);
                let chunk: Vec<u8> = pending.iter().take(step).copied().collect();
                if mem.write_slice(&chunk, buffer.addr()).is_ok() {
                    pending.drain(..step);
                    written = step as u32;
                }
            }
            queue.add_used(mem, head, written)?;
            used_any = true;
        }
        Ok(used_any)
    }

    /// One pass over every queue. Returns whether the guest has anything new to look at.
    fn work(&mut self, mem: &GuestMemoryMmap) -> Result<bool, virtio_queue::Error> {
        let mut used_any = self.read_control(mem)?;
        used_any |= self.write_control(mem)?;
        for port in 0..self.sinks.len() {
            used_any |= self.drain_guest(port, mem)?;
            used_any |= self.feed_guest(port, mem)?;
        }
        Ok(used_any)
    }
}

fn serve(
    mut ports: Ports,
    mem: GuestMemoryMmap,
    irq: Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
) -> Vec<Option<PortSink>> {
    while let Ok(Wake::Work) = wake.recv() {
        match ports.work(&mem) {
            Ok(true) => irq.signal_used(),
            Ok(false) => {}
            Err(err) => log::warn!("{}", copy::virtio_queue_error(&err)),
        }
    }
    ports.sinks
}

type Worker = (Sender<Wake>, JoinHandle<Vec<Option<PortSink>>>);

pub struct Console {
    /// The sinks, while no worker has them.
    sinks: Vec<Option<PortSink>>,
    queue_sizes: Vec<u16>,
    shared: Arc<Shared>,
    worker: Option<Worker>,
}

impl Console {
    /// A device with one port per entry of `sinks`, and the handle the host sends bytes through.
    /// Port N is the guest's `/dev/hvcN`.
    pub fn new(sinks: Vec<Option<PortSink>>) -> (Self, ConsoleHandle) {
        let ports = sinks.len();
        let mut queue_sizes = vec![PORT_QUEUE_SIZE; 2 * (ports + 1)];
        queue_sizes[CONTROL_RX] = CONTROL_QUEUE_SIZE;
        queue_sizes[CONTROL_TX] = CONTROL_QUEUE_SIZE;
        let shared = Arc::new(Shared {
            to_guest: Mutex::new(vec![VecDeque::new(); ports]),
            wake: Mutex::new(None),
        });
        let device = Self {
            sinks,
            queue_sizes,
            shared: Arc::clone(&shared),
            worker: None,
        };
        (device, ConsoleHandle(shared))
    }
}

impl Console {
    /// Gives `port` a sink for what the guest writes to it. Has no effect on a port the device does
    /// not have, or while the driver is running.
    pub fn set_sink(&mut self, port: usize, sink: PortSink) {
        if let Some(slot) = self.sinks.get_mut(port) {
            *slot = Some(sink);
        }
    }

    /// Bytes waiting for the guest to read from `port`.
    pub fn pending(&self, port: usize) -> usize {
        let to_guest = self
            .shared
            .to_guest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        to_guest.get(port).map_or(0, VecDeque::len)
    }
}

impl VirtioDevice for Console {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_CONSOLE
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn features(&self) -> u64 {
        F_MULTIPORT
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        let ports = (self.queue_sizes.len() / 2 - 1) as u32;
        config[CONFIG_PORTS_AT..CONFIG_PORTS_AT + 4].copy_from_slice(&ports.to_le_bytes());
        for (at, byte) in (offset..).zip(data.iter_mut()) {
            *byte = usize::try_from(at)
                .ok()
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError> {
        if self.worker.is_some() {
            return Err(DeviceError::AlreadyActive);
        }
        if ctx.queues.len() != self.queue_sizes.len() {
            return Err(DeviceError::MissingQueue);
        }
        let ports = Ports {
            shared: Arc::clone(&self.shared),
            sinks: std::mem::take(&mut self.sinks),
            queues: ctx.queues,
            control: VecDeque::new(),
        };
        let (wake, wakes) = mpsc::channel();
        let thread = thread::Builder::new()
            .name(copy::CONSOLE_WORKER_NAME.into())
            .spawn(move || serve(ports, ctx.mem, ctx.irq, wakes))?;
        let mut shared_wake = self
            .shared
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *shared_wake = Some(wake.clone());
        drop(shared_wake);
        self.worker = Some((wake, thread));
        // Bytes sent before the driver was ready are waiting.
        self.shared.wake();
        Ok(())
    }

    fn queue_notify(&mut self, _queue: u16) {
        self.shared.wake();
    }

    fn reset(&mut self) {
        self.shared
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((wake, thread)) = self.worker.take() {
            let _ = wake.send(Wake::Stop);
            match thread.join() {
                // The sinks come back, so that the ports still have them after the driver restarts.
                Ok(sinks) => self.sinks = sinks,
                Err(_) => log::error!("{}", copy::CONSOLE_WORKER_GONE),
            }
        }
    }
}

impl Drop for Console {
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
    use vm_memory::{Address, GuestAddress};

    use super::*;

    const MEM_SIZE: usize = 4 << 20;
    const PORTS: usize = 3;
    const RINGS_AT: u64 = 0;
    const RING_STRIDE: u64 = 0x1_0000;
    const BUFFERS_AT: u64 = 0x20_0000;
    const BUFFER_STRIDE: u64 = 0x1000;
    const QUEUE_LEN: u16 = 16;

    /// A sink the test can read back.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Rig<'a> {
        mem: &'a GuestMemoryMmap,
        mocks: Vec<MockSplitQueue<'a, GuestMemoryMmap>>,
        ports: Ports,
        handle: ConsoleHandle,
        captured: Captured,
        next_buffer: u64,
    }

    impl<'a> Rig<'a> {
        /// Three ports; only port 1 has a sink.
        fn new(mem: &'a GuestMemoryMmap) -> Self {
            let captured = Captured::default();
            let sinks: Vec<Option<PortSink>> = vec![None, Some(Box::new(captured.clone())), None];
            let (mut device, handle) = Console::new(sinks);
            let mocks: Vec<_> = (0..device.queue_sizes.len() as u64)
                .map(|index| {
                    let rings = GuestAddress(RINGS_AT + index * RING_STRIDE);
                    MockSplitQueue::create(mem, rings, QUEUE_LEN)
                })
                .collect();
            let queues = mocks
                .iter()
                .map(|mock| mock.create_queue().unwrap())
                .collect();
            let ports = Ports {
                shared: Arc::clone(&device.shared),
                sinks: std::mem::take(&mut device.sinks),
                queues,
                control: VecDeque::new(),
            };
            Self {
                mem,
                mocks,
                ports,
                handle,
                captured,
                next_buffer: 0,
            }
        }

        /// Posts one buffer on a queue: holding `bytes` if given, else empty for the device to fill.
        fn post(&mut self, queue: usize, bytes: Option<&[u8]>, len: u32) -> u64 {
            let addr = BUFFERS_AT + self.next_buffer * BUFFER_STRIDE;
            self.next_buffer += 1;
            let flags = match bytes {
                Some(bytes) => {
                    self.mem.write_slice(bytes, GuestAddress(addr)).unwrap();
                    0
                }
                None => VRING_DESC_F_WRITE as u16,
            };
            let descriptor = RawDescriptor::from(Descriptor::new(addr, len, flags, 0));
            // Each buffer is its own chain; descriptor indices are per queue.
            let used = self.posted(queue);
            self.mocks[queue]
                .add_desc_chains(&[descriptor], used)
                .unwrap();
            addr
        }

        fn posted(&self, queue: usize) -> u16 {
            self.mem
                .read_obj(self.mocks[queue].avail_addr().unchecked_add(2))
                .unwrap()
        }

        fn used_len(&self, queue: usize, index: u64) -> u32 {
            let at = self.mocks[queue]
                .used_addr()
                .unchecked_add(4 + index * 8 + 4);
            self.mem.read_obj(at).unwrap()
        }

        fn read(&self, addr: u64, len: usize) -> Vec<u8> {
            let mut bytes = vec![0u8; len];
            self.mem.read_slice(&mut bytes, GuestAddress(addr)).unwrap();
            bytes
        }
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap()
    }

    #[test]
    fn queues_follow_the_layout_the_specification_gives() {
        assert_eq!(receive_queue(0), 0);
        assert_eq!(receive_queue(1), 4);
        assert_eq!(receive_queue(2), 6);
        let (device, _handle) = Console::new(vec![None, None, None]);
        assert_eq!(device.queue_max_sizes().len(), 8);
        assert_eq!(device.features(), F_MULTIPORT);
        let mut config = [0xffu8; CONFIG_LEN + 2];
        device.read_config(0, &mut config);
        assert_eq!(config, [0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn ready_driver_is_told_about_every_port_and_each_becomes_a_terminal() {
        let mem = memory();
        let mut rig = Rig::new(&mem);
        const NO_PORT: u32 = u32::MAX;
        rig.post(
            CONTROL_TX,
            Some(&control(NO_PORT, DEVICE_READY, YES)),
            CONTROL_LEN as u32,
        );
        // Room for two answers only: the third has to wait.
        let first = rig.post(CONTROL_RX, None, CONTROL_LEN as u32);
        let second = rig.post(CONTROL_RX, None, CONTROL_LEN as u32);
        assert!(rig.ports.work(&mem).unwrap());
        assert_eq!(rig.read(first, CONTROL_LEN), control(0, PORT_ADD, YES));
        assert_eq!(rig.read(second, CONTROL_LEN), control(1, PORT_ADD, YES));
        assert_eq!(rig.ports.control.len(), 1);

        let third = rig.post(CONTROL_RX, None, CONTROL_LEN as u32);
        rig.post(
            CONTROL_TX,
            Some(&control(1, PORT_READY, YES)),
            CONTROL_LEN as u32,
        );
        // A port the device does not have, and a message cut short, are ignored.
        rig.post(
            CONTROL_TX,
            Some(&control(PORTS as u32, PORT_READY, YES)),
            CONTROL_LEN as u32,
        );
        rig.post(CONTROL_TX, Some(&[0; 4]), 4);
        let fourth = rig.post(CONTROL_RX, None, CONTROL_LEN as u32);
        let fifth = rig.post(CONTROL_RX, None, CONTROL_LEN as u32);
        assert!(rig.ports.work(&mem).unwrap());
        assert_eq!(rig.read(third, CONTROL_LEN), control(2, PORT_ADD, YES));
        assert_eq!(rig.read(fourth, CONTROL_LEN), control(1, CONSOLE_PORT, YES));
        assert_eq!(rig.read(fifth, CONTROL_LEN), control(1, PORT_OPEN, YES));
        assert!(rig.ports.control.is_empty());
        assert_eq!(rig.used_len(CONTROL_RX, 4), CONTROL_LEN as u32);
    }

    #[test]
    fn guest_output_reaches_the_ports_sink_and_is_dropped_without_one() {
        let mem = memory();
        let mut rig = Rig::new(&mem);
        let long = vec![b'x'; CHUNK + 10];
        rig.post(receive_queue(1) + 1, Some(b"hello "), 6);
        rig.post(receive_queue(1) + 1, Some(&long), long.len() as u32);
        rig.post(receive_queue(2) + 1, Some(b"nobody listens"), 14);
        assert!(rig.ports.work(&mem).unwrap());
        let captured = rig.captured.0.lock().unwrap().clone();
        assert_eq!(&captured[..6], b"hello ");
        assert_eq!(captured.len(), 6 + long.len());
        // All three buffers went back to the guest.
        assert_eq!(rig.posted(receive_queue(1) + 1), 2);
        let used: u16 = mem
            .read_obj(rig.mocks[receive_queue(2) + 1].used_addr().unchecked_add(2))
            .unwrap();
        assert_eq!(used, 1);
    }

    #[test]
    fn host_bytes_wait_for_guest_buffers_and_fill_them_in_order() {
        let mem = memory();
        let mut rig = Rig::new(&mem);
        assert!(rig.handle.send(2, b"abcdefgh"));
        assert!(!rig.handle.send(PORTS, b"no such port"));
        assert!(!rig.ports.work(&mem).unwrap());

        let first = rig.post(receive_queue(2), None, 5);
        let second = rig.post(receive_queue(2), None, 5);
        assert!(rig.ports.work(&mem).unwrap());
        assert_eq!(rig.read(first, 5), b"abcde");
        assert_eq!(rig.read(second, 3), b"fgh");
        assert_eq!(rig.used_len(receive_queue(2), 0), 5);
        assert_eq!(rig.used_len(receive_queue(2), 1), 3);
    }

    #[test]
    fn a_guest_that_never_reads_only_costs_a_bounded_backlog() {
        let (device, handle) = Console::new(vec![None]);
        assert!(handle.send(0, &vec![1u8; MAX_PENDING_BYTES]));
        assert!(handle.send(0, &[2u8; 10]));
        let to_guest = device.shared.to_guest.lock().unwrap();
        assert_eq!(to_guest[0].len(), MAX_PENDING_BYTES);
        assert_eq!(to_guest[0].back(), Some(&2));
        assert_eq!(to_guest[0].front(), Some(&1));
    }
}
