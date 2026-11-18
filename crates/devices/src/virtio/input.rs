//! virtio-input: a keyboard, touchscreen or gamepad as the guest's evdev layer sees one.
//!
//! What kind of device it is comes entirely from its description: a name, the event codes it can
//! send, and the range of each absolute axis. The host side holds an `InputHandle` and sends events
//! through it from any thread.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use virtio_bindings::virtio_ids::VIRTIO_ID_INPUT;
use virtio_bindings::virtio_input::{
    virtio_input_config_select_VIRTIO_INPUT_CFG_ABS_INFO as CFG_ABS_INFO,
    virtio_input_config_select_VIRTIO_INPUT_CFG_EV_BITS as CFG_EV_BITS,
    virtio_input_config_select_VIRTIO_INPUT_CFG_ID_DEVIDS as CFG_ID_DEVIDS,
    virtio_input_config_select_VIRTIO_INPUT_CFG_ID_NAME as CFG_ID_NAME,
    virtio_input_config_select_VIRTIO_INPUT_CFG_ID_SERIAL as CFG_ID_SERIAL,
    virtio_input_config_select_VIRTIO_INPUT_CFG_PROP_BITS as CFG_PROP_BITS,
};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;

/// Event queue (device to guest), then status queue (guest to device: LEDs and the like).
const QUEUE_SIZES: [u16; 2] = [64, 64];
/// `struct virtio_input_config`: select, subsel, size, five reserved bytes, then up to 128 bytes of payload.
const CONFIG_SELECT_AT: u64 = 0;
const CONFIG_SUBSEL_AT: u64 = 1;
const CONFIG_SIZE_AT: usize = 2;
const CONFIG_PAYLOAD_AT: usize = 8;
const CONFIG_PAYLOAD_MAX: usize = 128;
const CONFIG_LEN: usize = CONFIG_PAYLOAD_AT + CONFIG_PAYLOAD_MAX;
/// `struct virtio_input_event`: type and code (u16 each), then a 32-bit value.
const EVENT_LEN: usize = 8;
/// Events waiting for the guest to post buffers. Beyond this the newest batch is dropped whole,
/// so the guest never sees half of a multi-event report.
const MAX_PENDING_EVENTS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// Range of one absolute axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub min: u32,
    pub max: u32,
    pub fuzz: u32,
    pub flat: u32,
    pub resolution: u32,
}

/// Bus, vendor, product and version, as evdev reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceIds {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// Everything the guest can ask about the device.
#[derive(Clone, Debug, Default)]
pub struct InputDescription {
    pub name: String,
    pub serial: String,
    pub ids: DeviceIds,
    /// `INPUT_PROP_*` numbers the device has.
    pub properties: Vec<u16>,
    /// For each event type, the codes the device can send.
    pub events: Vec<(u16, Vec<u16>)>,
    pub axes: Vec<(u16, AbsInfo)>,
}

/// A bitmap with one bit per number, least significant bit first, as long as the highest number needs.
fn bitmap(numbers: &[u16]) -> Vec<u8> {
    let mut bits = vec![
        0u8;
        numbers
            .iter()
            .max()
            .map_or(0, |highest| usize::from(*highest) / 8 + 1)
    ];
    for number in numbers {
        bits[usize::from(*number) / 8] |= 1 << (number % 8);
    }
    bits.truncate(CONFIG_PAYLOAD_MAX);
    bits
}

impl InputDescription {
    /// The payload for one (select, subsel) pair; empty if the device has nothing to say about it.
    fn answer(&self, select: u8, subsel: u8) -> Vec<u8> {
        let mut payload = match u32::from(select) {
            CFG_ID_NAME => self.name.clone().into_bytes(),
            CFG_ID_SERIAL => self.serial.clone().into_bytes(),
            CFG_ID_DEVIDS => {
                let ids = [
                    self.ids.bustype,
                    self.ids.vendor,
                    self.ids.product,
                    self.ids.version,
                ];
                ids.iter().flat_map(|id| id.to_le_bytes()).collect()
            }
            CFG_PROP_BITS => bitmap(&self.properties),
            CFG_EV_BITS => {
                let codes = self
                    .events
                    .iter()
                    .find(|(kind, _)| *kind == u16::from(subsel));
                codes.map_or_else(Vec::new, |(_, codes)| bitmap(codes))
            }
            CFG_ABS_INFO => {
                let axis = self
                    .axes
                    .iter()
                    .find(|(axis, _)| *axis == u16::from(subsel));
                axis.map_or_else(Vec::new, |(_, info)| {
                    let fields = [info.min, info.max, info.fuzz, info.flat, info.resolution];
                    fields
                        .iter()
                        .flat_map(|field| field.to_le_bytes())
                        .collect()
                })
            }
            _ => Vec::new(),
        };
        payload.truncate(CONFIG_PAYLOAD_MAX);
        payload
    }
}

enum Wake {
    Work,
    Stop,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<VecDeque<InputEvent>>,
    /// Present while the driver is active.
    wake: Mutex<Option<Sender<Wake>>>,
}

impl Shared {
    fn wake(&self) {
        if let Some(wake) = self
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = wake.send(Wake::Work);
        }
    }
}

/// The host's end of an input device. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct InputHandle(Arc<Shared>);

impl InputHandle {
    /// Queues a batch of events for the guest, normally ending in a `SYN_REPORT`.
    /// Returns false, queuing nothing, if the guest has fallen too far behind.
    pub fn send(&self, events: &[InputEvent]) -> bool {
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pending.len() + events.len() > MAX_PENDING_EVENTS {
            return false;
        }
        pending.extend(events);
        drop(pending);
        self.0.wake();
        true
    }
}

/// Hands queued events to the guest, one per posted buffer. Returns whether any buffer was used.
fn deliver(
    shared: &Shared,
    events: &mut Queue,
    status: &mut Queue,
    mem: &GuestMemoryMmap,
) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    // Whatever the guest reports (LED state, for instance) is acknowledged and ignored.
    while let Some(chain) = status.pop_descriptor_chain(mem) {
        status.add_used(mem, chain.head_index(), 0)?;
        used_any = true;
    }
    let mut pending = shared
        .pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    while !pending.is_empty() {
        let Some(mut chain) = events.pop_descriptor_chain(mem) else {
            break;
        };
        let head = chain.head_index();
        let buffer = chain
            .next()
            .filter(|buffer| buffer.is_write_only() && buffer.len() as usize >= EVENT_LEN);
        let written = match (buffer, pending.front()) {
            (Some(buffer), Some(event)) => {
                let mut raw = [0u8; EVENT_LEN];
                raw[..2].copy_from_slice(&event.kind.to_le_bytes());
                raw[2..4].copy_from_slice(&event.code.to_le_bytes());
                raw[4..].copy_from_slice(&event.value.to_le_bytes());
                if mem.write_slice(&raw, buffer.addr()).is_ok() {
                    EVENT_LEN as u32
                } else {
                    0
                }
            }
            _ => 0,
        };
        // An unusable buffer is handed back empty and the event waits for the next one.
        if written != 0 {
            pending.pop_front();
        }
        events.add_used(mem, head, written)?;
        used_any = true;
    }
    Ok(used_any)
}

fn serve(
    shared: Arc<Shared>,
    mut events: Queue,
    mut status: Queue,
    mem: GuestMemoryMmap,
    irq: Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
) {
    while let Ok(Wake::Work) = wake.recv() {
        match deliver(&shared, &mut events, &mut status, &mem) {
            Ok(true) => irq.signal_used(),
            Ok(false) => {}
            Err(err) => log::warn!("{}", copy::virtio_queue_error(&err)),
        }
    }
}

pub struct Input {
    description: InputDescription,
    select: u8,
    subsel: u8,
    shared: Arc<Shared>,
    worker: Option<(Sender<Wake>, JoinHandle<()>)>,
}

impl Input {
    /// The device, and the handle the host sends its events through.
    pub fn new(description: InputDescription) -> (Self, InputHandle) {
        let shared = Arc::new(Shared::default());
        let device = Self {
            description,
            select: 0,
            subsel: 0,
            shared: Arc::clone(&shared),
            worker: None,
        };
        (device, InputHandle(shared))
    }
}

impl VirtioDevice for Input {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_INPUT
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        0
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let payload = self.description.answer(self.select, self.subsel);
        let mut config = [0u8; CONFIG_LEN];
        config[CONFIG_SELECT_AT as usize] = self.select;
        config[CONFIG_SUBSEL_AT as usize] = self.subsel;
        config[CONFIG_SIZE_AT] = payload.len() as u8;
        config[CONFIG_PAYLOAD_AT..CONFIG_PAYLOAD_AT + payload.len()].copy_from_slice(&payload);
        for (at, byte) in (offset..).zip(data.iter_mut()) {
            *byte = usize::try_from(at)
                .ok()
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (at, byte) in (offset..).zip(data) {
            match at {
                CONFIG_SELECT_AT => self.select = *byte,
                CONFIG_SUBSEL_AT => self.subsel = *byte,
                _ => {}
            }
        }
    }

    fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError> {
        if self.worker.is_some() {
            return Err(DeviceError::AlreadyActive);
        }
        let mut queues = ctx.queues.into_iter();
        let (Some(events), Some(status)) = (queues.next(), queues.next()) else {
            return Err(DeviceError::MissingQueue);
        };
        let (wake, wakes) = mpsc::channel();
        let shared = Arc::clone(&self.shared);
        let thread = thread::Builder::new()
            .name(copy::INPUT_WORKER_NAME.into())
            .spawn(move || serve(shared, events, status, ctx.mem, ctx.irq, wakes))?;
        *self
            .shared
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(wake.clone());
        self.worker = Some((wake, thread));
        // Events sent before the driver was ready are waiting.
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
            if thread.join().is_err() {
                log::error!("{}", copy::INPUT_WORKER_GONE);
            }
        }
        self.select = 0;
        self.subsel = 0;
    }
}

impl Drop for Input {
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

    // evdev numbers used by the tests.
    const EV_SYN: u16 = 0;
    const EV_KEY: u16 = 1;
    const EV_ABS: u16 = 3;
    const BTN_TOUCH: u16 = 0x14a;
    const ABS_MT_POSITION_X: u16 = 0x35;
    const INPUT_PROP_DIRECT: u16 = 1;

    const MEM_SIZE: usize = 2 << 20;
    const EVENT_RINGS_AT: u64 = 0;
    const STATUS_RINGS_AT: u64 = 0x1_0000;
    const BUFFERS_AT: u64 = 0x10_0000;
    const QUEUE_LEN: u16 = 16;

    fn touchscreen() -> InputDescription {
        InputDescription {
            name: "emu touchscreen".into(),
            serial: "emu-input-0".into(),
            ids: DeviceIds {
                bustype: 6,
                vendor: 0x1234,
                product: 0x5678,
                version: 1,
            },
            properties: vec![INPUT_PROP_DIRECT],
            events: vec![(EV_KEY, vec![BTN_TOUCH]), (EV_ABS, vec![ABS_MT_POSITION_X])],
            axes: vec![(
                ABS_MT_POSITION_X,
                AbsInfo {
                    min: 0,
                    max: 1279,
                    ..AbsInfo::default()
                },
            )],
        }
    }

    /// What a driver does: write select and subsel, read the size, read that many payload bytes.
    fn query(device: &mut Input, select: u32, subsel: u16) -> Vec<u8> {
        device.write_config(CONFIG_SELECT_AT, &[select as u8]);
        device.write_config(CONFIG_SUBSEL_AT, &[subsel as u8]);
        let mut size = [0u8];
        device.read_config(CONFIG_SIZE_AT as u64, &mut size);
        let mut payload = vec![0u8; usize::from(size[0])];
        device.read_config(CONFIG_PAYLOAD_AT as u64, &mut payload);
        payload
    }

    #[test]
    fn identity_is_reported_as_the_driver_asks_for_it() {
        let (mut device, _handle) = Input::new(touchscreen());
        assert_eq!(query(&mut device, CFG_ID_NAME, 0), b"emu touchscreen");
        assert_eq!(query(&mut device, CFG_ID_SERIAL, 0), b"emu-input-0");
        assert_eq!(
            query(&mut device, CFG_ID_DEVIDS, 0),
            [6, 0, 0x34, 0x12, 0x78, 0x56, 1, 0]
        );
    }

    #[test]
    fn capability_bitmaps_have_one_bit_per_code() {
        let (mut device, _handle) = Input::new(touchscreen());
        assert_eq!(query(&mut device, CFG_PROP_BITS, 0), [0b10]);
        let keys = query(&mut device, CFG_EV_BITS, EV_KEY);
        assert_eq!(keys.len(), usize::from(BTN_TOUCH) / 8 + 1);
        assert_eq!(keys[usize::from(BTN_TOUCH) / 8], 1 << (BTN_TOUCH % 8));
        assert_eq!(keys.iter().map(|byte| byte.count_ones()).sum::<u32>(), 1);
        // An event type the device does not have, and an unknown selector, both answer "nothing".
        assert!(query(&mut device, CFG_EV_BITS, 2).is_empty());
        assert!(query(&mut device, 0x7f, 0).is_empty());
    }

    #[test]
    fn axis_range_is_reported_only_for_axes_the_device_has() {
        let (mut device, _handle) = Input::new(touchscreen());
        let info = query(&mut device, CFG_ABS_INFO, ABS_MT_POSITION_X);
        assert_eq!(info.len(), 20);
        assert_eq!(u32::from_le_bytes(info[4..8].try_into().unwrap()), 1279);
        assert!(query(&mut device, CFG_ABS_INFO, 0).is_empty());
    }

    #[test]
    fn config_reads_beyond_the_payload_are_zero() {
        let (mut device, _handle) = Input::new(touchscreen());
        device.write_config(CONFIG_SELECT_AT, &[CFG_ID_NAME as u8]);
        let mut tail = [0xffu8; 8];
        device.read_config(CONFIG_LEN as u64 - 4, &mut tail);
        assert_eq!(tail, [0; 8]);
    }

    #[test]
    fn events_fill_guest_buffers_in_order_and_wait_when_there_are_none() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
        let event_mock = MockSplitQueue::create(&mem, GuestAddress(EVENT_RINGS_AT), QUEUE_LEN);
        let status_mock = MockSplitQueue::create(&mem, GuestAddress(STATUS_RINGS_AT), QUEUE_LEN);
        let write = VRING_DESC_F_WRITE as u16;
        let buffer = |index: u64| {
            RawDescriptor::from(Descriptor::new(
                BUFFERS_AT + index * 0x100,
                EVENT_LEN as u32,
                write,
                0,
            ))
        };
        // Two good buffers, then one too small to hold an event.
        event_mock
            .add_desc_chains(
                &[
                    buffer(0),
                    buffer(1),
                    RawDescriptor::from(Descriptor::new(BUFFERS_AT + 0x800, 4, write, 0)),
                ],
                0,
            )
            .unwrap();
        let mut events: Queue = event_mock.create_queue().unwrap();
        let mut status: Queue = status_mock.create_queue().unwrap();

        let (device, handle) = Input::new(touchscreen());
        let batch = [
            InputEvent {
                kind: EV_ABS,
                code: ABS_MT_POSITION_X,
                value: 640,
            },
            InputEvent {
                kind: EV_KEY,
                code: BTN_TOUCH,
                value: 1,
            },
            InputEvent {
                kind: EV_SYN,
                code: 0,
                value: 0,
            },
        ];
        assert!(handle.send(&batch));
        assert!(deliver(&device.shared, &mut events, &mut status, &mem).unwrap());

        let read_event = |index: u64| {
            let mut raw = [0u8; EVENT_LEN];
            mem.read_slice(&mut raw, GuestAddress(BUFFERS_AT + index * 0x100))
                .unwrap();
            raw
        };
        assert_eq!(read_event(0), [3, 0, 0x35, 0, 0x80, 0x02, 0, 0]);
        assert_eq!(read_event(1), [1, 0, 0x4a, 0x01, 1, 0, 0, 0]);
        let used_len = |index: u64| -> u32 {
            mem.read_obj(event_mock.used_addr().unchecked_add(4 + index * 8 + 4))
                .unwrap()
        };
        assert_eq!(
            [used_len(0), used_len(1), used_len(2)],
            [8, 8, 0],
            "the short buffer comes back empty"
        );
        // The third event is still waiting for a usable buffer.
        assert_eq!(device.shared.pending.lock().unwrap().len(), 1);
        assert!(!deliver(&device.shared, &mut events, &mut status, &mem).unwrap());
    }

    #[test]
    fn a_batch_that_would_overflow_the_backlog_is_refused_whole() {
        let (_device, handle) = Input::new(touchscreen());
        let event = InputEvent {
            kind: EV_SYN,
            code: 0,
            value: 0,
        };
        assert!(handle.send(&vec![event; MAX_PENDING_EVENTS - 1]));
        assert!(!handle.send(&[event, event]));
        assert!(handle.send(&[event]));
        assert!(!handle.send(&[event]));
    }
}
