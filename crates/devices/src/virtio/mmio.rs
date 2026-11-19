//! virtio-mmio transport, version 2 (virtio 1.x), including the shared-memory region registers.
//!
//! Register offsets come from `virtio_bindings::virtio_mmio`, which is generated from the Linux UAPI header.

use std::sync::{Arc, Mutex, PoisonError};

use virtio_bindings::virtio_config::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_CONFIG_S_NEEDS_RESET,
    VIRTIO_F_VERSION_1,
};
use virtio_bindings::virtio_mmio::*;
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{Activate, VirtioDevice, VirtioInterrupt};
use crate::{InterruptLine, MmioDevice, copy};

/// "virt" in little-endian ASCII.
const MAGIC: u32 = u32::from_le_bytes(*b"virt");
const VERSION: u32 = 2;
/// Identifies this implementation to the driver; carries no meaning.
const VENDOR_ID: u32 = u32::from_le_bytes(*b"EMU0");
/// What the SHM length registers read when the selected region does not exist.
const SHM_ABSENT: u64 = u64::MAX;
/// Bits in one half of a 64-bit register pair.
const HALF: u32 = u32::BITS;

fn low(value: u64) -> u32 {
    value as u32
}

fn high(value: u64) -> u32 {
    (value >> HALF) as u32
}

/// The half of `value` a feature selector picks; selectors above 1 read as zero.
fn feature_half(value: u64, selector: u32) -> u32 {
    match selector {
        0 => low(value),
        1 => high(value),
        _ => 0,
    }
}

struct State {
    device: Box<dyn VirtioDevice>,
    mem: GuestMemoryMmap,
    interrupt: Arc<VirtioInterrupt>,
    /// Driver-visible queue configuration. Moved into the device on activation.
    queues: Vec<Queue>,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    shm_sel: u32,
    status: u32,
    activated: bool,
}

fn fresh_queues(device: &dyn VirtioDevice) -> Result<Vec<Queue>, virtio_queue::Error> {
    device
        .queue_max_sizes()
        .iter()
        .map(|max| Queue::new(*max))
        .collect()
}

impl State {
    fn offered_features(&self) -> u64 {
        self.device.features() | 1 << VIRTIO_F_VERSION_1
    }

    fn selected_queue(&mut self) -> Option<&mut Queue> {
        if self.activated {
            return None;
        }
        self.queues.get_mut(usize::try_from(self.queue_sel).ok()?)
    }

    fn selected_shm(&self) -> Option<(u64, u64)> {
        let region = self
            .device
            .shm_regions()
            .iter()
            .find(|region| u32::from(region.id) == self.shm_sel)?;
        Some((region.gpa, region.len))
    }

    fn reset(&mut self) {
        self.device.reset();
        self.interrupt.clear();
        // Queue sizes were accepted in `new`, so rebuilding cannot fail; fall back to none if it somehow does.
        self.queues = fresh_queues(&*self.device).unwrap_or_default();
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.shm_sel = 0;
        self.status = 0;
        self.activated = false;
    }

    fn activate(&mut self) -> bool {
        let device_type = self.device.device_type();
        if self
            .queues
            .iter()
            .any(|queue| queue.ready() && !queue.is_valid(&self.mem))
        {
            log::warn!("{}", copy::virtio_queue_not_ready(device_type));
            return false;
        }
        let ctx = Activate {
            mem: self.mem.clone(),
            queues: std::mem::take(&mut self.queues),
            irq: Arc::clone(&self.interrupt),
            features: self.driver_features,
        };
        match self.device.activate(ctx) {
            Ok(()) => true,
            Err(err) => {
                log::warn!("{}", copy::virtio_activation_failed(device_type, &err));
                false
            }
        }
    }

    fn write_status(&mut self, value: u32) {
        if value == 0 {
            self.reset();
            return;
        }
        let newly_set = value & !self.status;
        let mut status = value;
        if newly_set & VIRTIO_CONFIG_S_FEATURES_OK != 0 {
            let offered = self.offered_features();
            let chosen = self.driver_features;
            // A driver that does not speak virtio 1.x cannot use this transport version.
            if chosen & !offered != 0 || chosen & 1 << VIRTIO_F_VERSION_1 == 0 {
                log::warn!(
                    "{}",
                    copy::virtio_bad_features(self.device.device_type(), chosen, offered)
                );
                status &= !VIRTIO_CONFIG_S_FEATURES_OK;
            }
        }
        if newly_set & VIRTIO_CONFIG_S_DRIVER_OK != 0 && !self.activated {
            if status & VIRTIO_CONFIG_S_FEATURES_OK != 0 && self.activate() {
                self.activated = true;
            } else {
                status = status & !VIRTIO_CONFIG_S_DRIVER_OK | VIRTIO_CONFIG_S_NEEDS_RESET;
            }
        }
        self.status = status;
    }

    fn read_register(&mut self, register: u32) -> u32 {
        match register {
            VIRTIO_MMIO_MAGIC_VALUE => MAGIC,
            VIRTIO_MMIO_VERSION => VERSION,
            VIRTIO_MMIO_DEVICE_ID => self.device.device_type(),
            VIRTIO_MMIO_VENDOR_ID => VENDOR_ID,
            VIRTIO_MMIO_DEVICE_FEATURES => {
                feature_half(self.offered_features(), self.device_features_sel)
            }
            VIRTIO_MMIO_QUEUE_NUM_MAX => {
                let sizes = self.device.queue_max_sizes();
                usize::try_from(self.queue_sel)
                    .ok()
                    .and_then(|at| sizes.get(at))
                    .map_or(0, |max| u32::from(*max))
            }
            VIRTIO_MMIO_QUEUE_READY => {
                let configured = usize::try_from(self.queue_sel)
                    .ok()
                    .and_then(|at| self.queues.get(at));
                u32::from(configured.is_some_and(|queue| queue.ready()))
            }
            VIRTIO_MMIO_INTERRUPT_STATUS => self.interrupt.status(),
            VIRTIO_MMIO_STATUS => self.status,
            VIRTIO_MMIO_SHM_LEN_LOW => low(self.selected_shm().map_or(SHM_ABSENT, |(_, len)| len)),
            VIRTIO_MMIO_SHM_LEN_HIGH => {
                high(self.selected_shm().map_or(SHM_ABSENT, |(_, len)| len))
            }
            VIRTIO_MMIO_SHM_BASE_LOW => low(self.selected_shm().map_or(SHM_ABSENT, |(gpa, _)| gpa)),
            VIRTIO_MMIO_SHM_BASE_HIGH => {
                high(self.selected_shm().map_or(SHM_ABSENT, |(gpa, _)| gpa))
            }
            // Configuration never changes underneath a running driver in the devices built so far.
            VIRTIO_MMIO_CONFIG_GENERATION => 0,
            _ => 0,
        }
    }

    fn write_register(&mut self, register: u32, value: u32) {
        match register {
            VIRTIO_MMIO_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            VIRTIO_MMIO_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            // Once FEATURES_OK is set the negotiation is over; later writes must not change what
            // the device is activated with.
            VIRTIO_MMIO_DRIVER_FEATURES if self.status & VIRTIO_CONFIG_S_FEATURES_OK != 0 => {}
            VIRTIO_MMIO_DRIVER_FEATURES => {
                let keep_low = u64::from(low(self.driver_features));
                let keep_high = u64::from(high(self.driver_features)) << HALF;
                match self.driver_features_sel {
                    0 => self.driver_features = keep_high | u64::from(value),
                    1 => self.driver_features = keep_low | u64::from(value) << HALF,
                    _ => {}
                }
            }
            VIRTIO_MMIO_QUEUE_SEL => self.queue_sel = value,
            VIRTIO_MMIO_QUEUE_NUM => {
                if let (Some(queue), Ok(size)) = (self.selected_queue(), u16::try_from(value)) {
                    queue.set_size(size);
                }
            }
            VIRTIO_MMIO_QUEUE_READY => {
                if let Some(queue) = self.selected_queue() {
                    queue.set_ready(value == 1);
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                self.with_queue(|queue| queue.set_desc_table_address(Some(value), None))
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                self.with_queue(|queue| queue.set_desc_table_address(None, Some(value)))
            }
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                self.with_queue(|queue| queue.set_avail_ring_address(Some(value), None))
            }
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                self.with_queue(|queue| queue.set_avail_ring_address(None, Some(value)))
            }
            VIRTIO_MMIO_QUEUE_USED_LOW => {
                self.with_queue(|queue| queue.set_used_ring_address(Some(value), None))
            }
            VIRTIO_MMIO_QUEUE_USED_HIGH => {
                self.with_queue(|queue| queue.set_used_ring_address(None, Some(value)))
            }
            VIRTIO_MMIO_QUEUE_NOTIFY => {
                if let (true, Ok(queue)) = (self.activated, u16::try_from(value)) {
                    self.device.queue_notify(queue);
                }
            }
            VIRTIO_MMIO_INTERRUPT_ACK => self.interrupt.acknowledge(value),
            VIRTIO_MMIO_STATUS => self.write_status(value),
            VIRTIO_MMIO_SHM_SEL => self.shm_sel = value,
            _ => {}
        }
    }

    fn with_queue(&mut self, configure: impl FnOnce(&mut Queue)) {
        if let Some(queue) = self.selected_queue() {
            configure(queue);
        }
    }
}

pub struct MmioTransport {
    state: Mutex<State>,
}

impl MmioTransport {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        mem: GuestMemoryMmap,
        line: Arc<dyn InterruptLine>,
    ) -> Result<Self, virtio_queue::Error> {
        let queues = fresh_queues(&*device)?;
        Ok(Self {
            state: Mutex::new(State {
                device,
                mem,
                interrupt: VirtioInterrupt::new(line),
                queues,
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                queue_sel: 0,
                shm_sel: 0,
                status: 0,
                activated: false,
            }),
        })
    }
}

impl MmioDevice for MmioTransport {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(config_offset) = offset.checked_sub(u64::from(VIRTIO_MMIO_CONFIG)) {
            state.device.read_config(config_offset, data);
            return;
        }
        // Every transport register is 32 bits wide; other access sizes read as zero.
        match (u32::try_from(offset), <&mut [u8; 4]>::try_from(&mut *data)) {
            (Ok(register), Ok(word)) => *word = state.read_register(register).to_le_bytes(),
            _ => data.fill(0),
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(config_offset) = offset.checked_sub(u64::from(VIRTIO_MMIO_CONFIG)) {
            state.device.write_config(config_offset, data);
            return;
        }
        if let (Ok(register), Ok(word)) = (u32::try_from(offset), <[u8; 4]>::try_from(data)) {
            state.write_register(register, u32::from_le_bytes(word));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use virtio_bindings::virtio_config::{VIRTIO_CONFIG_S_ACKNOWLEDGE, VIRTIO_CONFIG_S_DRIVER};
    use vm_memory::GuestAddress;

    use super::super::testing::LineLog;
    use super::super::{DeviceError, ShmRegion};
    use super::*;

    const DEVICE_TYPE: u32 = 42;
    const DEVICE_FEATURE: u64 = 1 << 5;
    const QUEUE_MAX: u16 = 16;
    const SHM: ShmRegion = ShmRegion {
        id: 1,
        gpa: 0x8_0000_0000,
        len: 0x2_0000_0000,
    };
    const MEM_SIZE: usize = 1 << 20;
    const DESC_ADDR: u32 = 0x1000;
    const AVAIL_ADDR: u32 = 0x2000;
    const USED_ADDR: u32 = 0x3000;

    #[derive(Default)]
    struct Seen {
        activated_with: Option<(u64, usize, bool)>,
        notified: Vec<u16>,
        resets: u32,
        config_writes: Vec<(u64, Vec<u8>)>,
        fail_activation: bool,
    }

    struct FakeDevice(Arc<Mutex<Seen>>);

    impl VirtioDevice for FakeDevice {
        fn device_type(&self) -> u32 {
            DEVICE_TYPE
        }

        fn queue_max_sizes(&self) -> &[u16] {
            &[QUEUE_MAX]
        }

        fn features(&self) -> u64 {
            DEVICE_FEATURE
        }

        fn read_config(&self, offset: u64, data: &mut [u8]) {
            data.fill(0xc0 + offset as u8);
        }

        fn write_config(&mut self, offset: u64, data: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .config_writes
                .push((offset, data.to_vec()));
        }

        fn shm_regions(&self) -> &[ShmRegion] {
            &[SHM]
        }

        fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError> {
            let mut seen = self.0.lock().unwrap();
            if seen.fail_activation {
                return Err(DeviceError::AlreadyActive);
            }
            seen.activated_with = Some((ctx.features, ctx.queues.len(), ctx.queues[0].ready()));
            ctx.irq.signal_used();
            Ok(())
        }

        fn queue_notify(&mut self, queue: u16) {
            self.0.lock().unwrap().notified.push(queue);
        }

        fn reset(&mut self) {
            self.0.lock().unwrap().resets += 1;
        }
    }

    struct Harness {
        transport: MmioTransport,
        seen: Arc<Mutex<Seen>>,
        line: Arc<LineLog>,
    }

    impl Harness {
        fn new() -> Self {
            let seen = Arc::new(Mutex::new(Seen::default()));
            let line = Arc::new(LineLog::default());
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
            let transport =
                MmioTransport::new(Box::new(FakeDevice(seen.clone())), mem, line.clone()).unwrap();
            Self {
                transport,
                seen,
                line,
            }
        }

        fn read(&self, register: u32) -> u32 {
            let mut word = [0u8; 4];
            self.transport.read(u64::from(register), &mut word);
            u32::from_le_bytes(word)
        }

        fn write(&self, register: u32, value: u32) {
            self.transport
                .write(u64::from(register), &value.to_le_bytes());
        }

        fn negotiate(&self, driver_features: u64) {
            self.write(VIRTIO_MMIO_STATUS, VIRTIO_CONFIG_S_ACKNOWLEDGE);
            self.write(
                VIRTIO_MMIO_STATUS,
                VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER,
            );
            self.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
            self.write(VIRTIO_MMIO_DRIVER_FEATURES, low(driver_features));
            self.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, 1);
            self.write(VIRTIO_MMIO_DRIVER_FEATURES, high(driver_features));
            self.write(
                VIRTIO_MMIO_STATUS,
                self.read(VIRTIO_MMIO_STATUS) | VIRTIO_CONFIG_S_FEATURES_OK,
            );
        }

        fn configure_queue(&self) {
            self.write(VIRTIO_MMIO_QUEUE_SEL, 0);
            self.write(VIRTIO_MMIO_QUEUE_NUM, u32::from(QUEUE_MAX));
            self.write(VIRTIO_MMIO_QUEUE_DESC_LOW, DESC_ADDR);
            self.write(VIRTIO_MMIO_QUEUE_DESC_HIGH, 0);
            self.write(VIRTIO_MMIO_QUEUE_AVAIL_LOW, AVAIL_ADDR);
            self.write(VIRTIO_MMIO_QUEUE_AVAIL_HIGH, 0);
            self.write(VIRTIO_MMIO_QUEUE_USED_LOW, USED_ADDR);
            self.write(VIRTIO_MMIO_QUEUE_USED_HIGH, 0);
            self.write(VIRTIO_MMIO_QUEUE_READY, 1);
        }

        fn driver_ok(&self) {
            self.write(
                VIRTIO_MMIO_STATUS,
                self.read(VIRTIO_MMIO_STATUS) | VIRTIO_CONFIG_S_DRIVER_OK,
            );
        }
    }

    const ACCEPTED: u64 = DEVICE_FEATURE | 1 << VIRTIO_F_VERSION_1;

    #[test]
    fn identification_registers() {
        let h = Harness::new();
        assert_eq!(h.read(VIRTIO_MMIO_MAGIC_VALUE), 0x7472_6976);
        assert_eq!(h.read(VIRTIO_MMIO_VERSION), 2);
        assert_eq!(h.read(VIRTIO_MMIO_DEVICE_ID), DEVICE_TYPE);
        assert_eq!(h.read(VIRTIO_MMIO_QUEUE_NUM_MAX), u32::from(QUEUE_MAX));
        h.write(VIRTIO_MMIO_QUEUE_SEL, 1);
        assert_eq!(
            h.read(VIRTIO_MMIO_QUEUE_NUM_MAX),
            0,
            "a queue the device does not have"
        );
    }

    #[test]
    fn offered_features_include_version_1_in_the_high_half() {
        let h = Harness::new();
        h.write(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0);
        assert_eq!(h.read(VIRTIO_MMIO_DEVICE_FEATURES), low(DEVICE_FEATURE));
        h.write(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 1);
        assert_eq!(
            h.read(VIRTIO_MMIO_DEVICE_FEATURES),
            1 << (VIRTIO_F_VERSION_1 - HALF)
        );
        h.write(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 2);
        assert_eq!(h.read(VIRTIO_MMIO_DEVICE_FEATURES), 0);
    }

    #[test]
    fn full_driver_handshake_activates_the_device_with_its_queue() {
        let h = Harness::new();
        h.negotiate(ACCEPTED);
        assert_ne!(h.read(VIRTIO_MMIO_STATUS) & VIRTIO_CONFIG_S_FEATURES_OK, 0);
        h.configure_queue();
        assert_eq!(h.read(VIRTIO_MMIO_QUEUE_READY), 1);
        h.driver_ok();

        assert_ne!(h.read(VIRTIO_MMIO_STATUS) & VIRTIO_CONFIG_S_DRIVER_OK, 0);
        assert_eq!(
            h.seen.lock().unwrap().activated_with,
            Some((ACCEPTED, 1, true))
        );

        // The fake device signalled a used buffer during activation.
        assert_eq!(h.read(VIRTIO_MMIO_INTERRUPT_STATUS), VIRTIO_MMIO_INT_VRING);
        assert_eq!(h.line.level(), Some(true));
        h.write(VIRTIO_MMIO_INTERRUPT_ACK, VIRTIO_MMIO_INT_VRING);
        assert_eq!(h.read(VIRTIO_MMIO_INTERRUPT_STATUS), 0);
        assert_eq!(h.line.level(), Some(false));

        h.write(VIRTIO_MMIO_QUEUE_NOTIFY, 0);
        assert_eq!(h.seen.lock().unwrap().notified, [0]);
    }

    #[test]
    fn notify_before_activation_is_dropped() {
        let h = Harness::new();
        h.write(VIRTIO_MMIO_QUEUE_NOTIFY, 0);
        assert!(h.seen.lock().unwrap().notified.is_empty());
    }

    #[test]
    fn features_the_device_never_offered_are_refused() {
        let h = Harness::new();
        h.negotiate(ACCEPTED | 1 << 7);
        assert_eq!(h.read(VIRTIO_MMIO_STATUS) & VIRTIO_CONFIG_S_FEATURES_OK, 0);
        // And so is a legacy driver that does not accept VERSION_1.
        let h = Harness::new();
        h.negotiate(DEVICE_FEATURE);
        assert_eq!(h.read(VIRTIO_MMIO_STATUS) & VIRTIO_CONFIG_S_FEATURES_OK, 0);
    }

    #[test]
    fn features_written_after_features_ok_change_nothing() {
        let h = Harness::new();
        h.negotiate(ACCEPTED);
        h.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
        h.write(VIRTIO_MMIO_DRIVER_FEATURES, low(ACCEPTED | 1 << 7));
        h.configure_queue();
        h.driver_ok();
        let seen = h.seen.lock().unwrap().activated_with;
        assert_eq!(seen.map(|(features, ..)| features), Some(ACCEPTED));
    }

    #[test]
    fn driver_ok_without_features_ok_asks_for_a_reset() {
        let h = Harness::new();
        h.write(
            VIRTIO_MMIO_STATUS,
            VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER,
        );
        h.configure_queue();
        h.driver_ok();
        let status = h.read(VIRTIO_MMIO_STATUS);
        assert_ne!(status & VIRTIO_CONFIG_S_NEEDS_RESET, 0);
        assert_eq!(status & VIRTIO_CONFIG_S_DRIVER_OK, 0);
        assert_eq!(h.seen.lock().unwrap().activated_with, None);
    }

    #[test]
    fn queue_pointing_outside_guest_memory_is_refused() {
        let h = Harness::new();
        h.negotiate(ACCEPTED);
        h.configure_queue();
        h.write(VIRTIO_MMIO_QUEUE_READY, 0);
        h.write(VIRTIO_MMIO_QUEUE_DESC_HIGH, 1);
        h.write(VIRTIO_MMIO_QUEUE_READY, 1);
        h.driver_ok();
        assert_ne!(h.read(VIRTIO_MMIO_STATUS) & VIRTIO_CONFIG_S_NEEDS_RESET, 0);
        assert_eq!(h.seen.lock().unwrap().activated_with, None);
    }

    #[test]
    fn failed_device_activation_asks_for_a_reset() {
        let h = Harness::new();
        h.seen.lock().unwrap().fail_activation = true;
        h.negotiate(ACCEPTED);
        h.configure_queue();
        h.driver_ok();
        assert_ne!(h.read(VIRTIO_MMIO_STATUS) & VIRTIO_CONFIG_S_NEEDS_RESET, 0);
    }

    #[test]
    fn writing_zero_status_resets_device_queues_and_interrupt() {
        let h = Harness::new();
        h.negotiate(ACCEPTED);
        h.configure_queue();
        h.driver_ok();
        h.write(VIRTIO_MMIO_STATUS, 0);

        assert_eq!(h.seen.lock().unwrap().resets, 1);
        assert_eq!(h.read(VIRTIO_MMIO_STATUS), 0);
        assert_eq!(h.read(VIRTIO_MMIO_INTERRUPT_STATUS), 0);
        assert_eq!(h.line.level(), Some(false));
        assert_eq!(h.read(VIRTIO_MMIO_QUEUE_READY), 0);

        // The same driver can come up again.
        h.negotiate(ACCEPTED);
        h.configure_queue();
        h.driver_ok();
        assert_ne!(h.read(VIRTIO_MMIO_STATUS) & VIRTIO_CONFIG_S_DRIVER_OK, 0);
    }

    #[test]
    fn shared_memory_registers_describe_the_region_or_report_absence() {
        let h = Harness::new();
        h.write(VIRTIO_MMIO_SHM_SEL, u32::from(SHM.id));
        assert_eq!(h.read(VIRTIO_MMIO_SHM_LEN_LOW), low(SHM.len));
        assert_eq!(h.read(VIRTIO_MMIO_SHM_LEN_HIGH), high(SHM.len));
        assert_eq!(h.read(VIRTIO_MMIO_SHM_BASE_LOW), low(SHM.gpa));
        assert_eq!(h.read(VIRTIO_MMIO_SHM_BASE_HIGH), high(SHM.gpa));
        h.write(VIRTIO_MMIO_SHM_SEL, 0);
        assert_eq!(h.read(VIRTIO_MMIO_SHM_LEN_LOW), u32::MAX);
        assert_eq!(h.read(VIRTIO_MMIO_SHM_LEN_HIGH), u32::MAX);
    }

    #[test]
    fn config_space_is_forwarded_with_device_relative_offsets() {
        let h = Harness::new();
        let mut byte = [0u8];
        h.transport
            .read(u64::from(VIRTIO_MMIO_CONFIG) + 3, &mut byte);
        assert_eq!(byte, [0xc3]);
        h.transport
            .write(u64::from(VIRTIO_MMIO_CONFIG) + 8, &[1, 2]);
        assert_eq!(h.seen.lock().unwrap().config_writes, [(8, vec![1, 2])]);
    }

    #[test]
    fn register_accesses_of_the_wrong_width_do_nothing() {
        let h = Harness::new();
        let mut half = [0xffu8; 2];
        h.transport
            .read(u64::from(VIRTIO_MMIO_MAGIC_VALUE), &mut half);
        assert_eq!(half, [0, 0]);
        h.transport.write(
            u64::from(VIRTIO_MMIO_STATUS),
            &[VIRTIO_CONFIG_S_ACKNOWLEDGE as u8],
        );
        assert_eq!(h.read(VIRTIO_MMIO_STATUS), 0);
    }
}
