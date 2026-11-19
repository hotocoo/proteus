//! Windows Hypervisor Platform backend (Windows, x86_64).
//!
//! The hypervisor emulates each local APIC; the IOAPIC is emulated here, in front of the VMM's
//! MMIO bus, and turns a raised pin into `WHvRequestInterrupt`. MMIO and port I/O exits are decoded
//! by WinHvEmulation, which calls back into the VMM's `IoHandler`. There is no PIT and no kvmclock:
//! the guest is shown Hyper-V's enlightenments instead (reference TSC page and the frequency MSRs),
//! which the guest kernel's `CONFIG_HYPERV` uses to know its TSC and APIC timer rates. A vCPU is
//! kicked with `WHvCancelRunVirtualProcessor`, which also stops the next run if none is in progress.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, PoisonError};

use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::core::HRESULT;

use crate::x86_64::{GPR_COUNT, Regs, Segment, SystemRegs, Table, apic_id};
use crate::{
    Caps, Exit, Gpa, HvError, HypervisorBackend, IoHandler, Irqchip, IrqchipConfig, Prot, Vcpu, Vm,
    VmConfig, copy,
};

const PAGE_SIZE: usize = 4096;
/// Where the board puts the IOAPIC (the architectural address) and how big its window is.
const IOAPIC_BASE: Gpa = 0xfec0_0000;
const IOAPIC_SIZE: Gpa = 0x1000;
const IOAPIC_PINS: usize = 24;
/// vCPUs a partition is created for: one per host thread, as WHP does not overcommit well.
fn max_vcpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

/// Hyper-V enlightenments shown to the guest (bank 0 of the synthetic processor features, bit
/// numbers from WinHvPlatformDefs.h; bit 7 is reserved): hypervisor present, Hv#1, partition
/// reference counter, APIC access MSRs, hypercall MSRs, VP index, reference TSC page and the
/// TSC/APIC frequency MSRs. SynIC and synthetic timers stay off, so the guest keeps time with its
/// local APIC timer at the rate the frequency MSR gives.
const SYNTHETIC_FEATURES: u64 =
    1 << 0 | 1 << 1 | 1 << 3 | 1 << 6 | 1 << 8 | 1 << 9 | 1 << 10 | 1 << 12;

fn check(call: &'static str, result: HRESULT) -> Result<(), HvError> {
    if result >= 0 {
        Ok(())
    } else {
        Err(HvError::Backend {
            call,
            code: i64::from(result as u32),
        })
    }
}

pub struct Whp;

impl HypervisorBackend for Whp {
    type Vm = WhpVm;

    fn probe() -> Result<Caps, HvError> {
        let mut present: i32 = 0;
        let mut written = 0u32;
        // SAFETY: the buffer is a BOOL the call fills in, and lives for the call.
        let result = unsafe {
            WHvGetCapability(
                WHvCapabilityCodeHypervisorPresent,
                (&raw mut present).cast(),
                std::mem::size_of::<i32>() as u32,
                &mut written,
            )
        };
        // A missing winhvplatform.dll or a disabled feature both mean no hypervisor to use.
        if result < 0 || present == 0 {
            return Err(HvError::Unsupported(copy::NO_WHP));
        }
        Ok(Caps {
            host_page_size: PAGE_SIZE,
            gpa_bits: crate::x86_64::GPA_BITS,
            max_vcpus: max_vcpus(),
        })
    }

    fn create_vm(_cfg: &VmConfig) -> Result<WhpVm, HvError> {
        Self::probe()?;
        let mut handle: WHV_PARTITION_HANDLE = 0;
        // SAFETY: the call writes the new handle into `handle`.
        check("WHvCreatePartition", unsafe {
            WHvCreatePartition(&mut handle)
        })?;
        let partition = Partition(handle);
        let set = |code, property: &WHV_PARTITION_PROPERTY, call| {
            // SAFETY: the property is a valid union for `code` and lives for the call.
            check(call, unsafe {
                WHvSetPartitionProperty(
                    partition.0,
                    code,
                    std::ptr::from_ref(property).cast(),
                    std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
                )
            })
        };
        // SAFETY: an all-zero property is a valid starting point for every member.
        let mut property: WHV_PARTITION_PROPERTY = unsafe { std::mem::zeroed() };
        property.ProcessorCount = max_vcpus();
        set(
            WHvPartitionPropertyCodeProcessorCount,
            &property,
            "WHvSetPartitionProperty(ProcessorCount)",
        )?;
        property = unsafe { std::mem::zeroed() };
        property.LocalApicEmulationMode = WHvX64LocalApicEmulationModeXApic;
        set(
            WHvPartitionPropertyCodeLocalApicEmulationMode,
            &property,
            "WHvSetPartitionProperty(LocalApicEmulationMode)",
        )?;
        let offered = synthetic_features_offered();
        let wanted = SYNTHETIC_FEATURES & offered;
        // Without the frequency MSRs and the reference TSC the guest has no way to calibrate its
        // timers on this board (no PIT, no HPET), and would hang without a word.
        if wanted != SYNTHETIC_FEATURES {
            log::warn!(
                "WHP offers synthetic features {offered:#x}; the guest gets {wanted:#x} of the                  {SYNTHETIC_FEATURES:#x} it needs to keep time"
            );
        }
        property = unsafe { std::mem::zeroed() };
        property.SyntheticProcessorFeaturesBanks = WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
            BanksCount: 1,
            Reserved0: 0,
            Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 { AsUINT64: [wanted] },
        };
        set(
            WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
            &property,
            "WHvSetPartitionProperty(SyntheticProcessorFeaturesBanks)",
        )?;
        // SAFETY: the partition exists and is configured.
        check("WHvSetupPartition", unsafe {
            WHvSetupPartition(partition.0)
        })?;
        let partition = Arc::new(partition);
        Ok(WhpVm {
            ioapic: Arc::new(Ioapic::new(Arc::clone(&partition))),
            partition,
            slots: Mutex::new(Vec::new()),
        })
    }
}

/// Bank 0 of the synthetic processor features this host can give a partition; none if it says nothing.
fn synthetic_features_offered() -> u64 {
    // SAFETY: an all-zero capability is a valid buffer for the call to fill.
    let mut capability: WHV_CAPABILITY = unsafe { std::mem::zeroed() };
    let mut written = 0u32;
    // SAFETY: the buffer is a whole capability union and lives for the call.
    let result = unsafe {
        WHvGetCapability(
            WHvCapabilityCodeSyntheticProcessorFeaturesBanks,
            (&raw mut capability).cast(),
            std::mem::size_of::<WHV_CAPABILITY>() as u32,
            &mut written,
        )
    };
    if result < 0 {
        return 0;
    }
    // SAFETY: the call filled in the banks member.
    unsafe {
        capability
            .SyntheticProcessorFeaturesBanks
            .Anonymous
            .AsUINT64[0]
    }
}

/// Deleted when the last VM, vCPU or irqchip reference is gone.
struct Partition(WHV_PARTITION_HANDLE);

impl Drop for Partition {
    fn drop(&mut self) {
        // SAFETY: nothing uses the handle any more.
        unsafe { WHvDeletePartition(self.0) };
    }
}

pub struct WhpVm {
    partition: Arc<Partition>,
    ioapic: Arc<Ioapic>,
    slots: Mutex<Vec<(Gpa, usize)>>,
}

impl Vm for WhpVm {
    type Vcpu = WhpVcpu;
    type Irqchip = Ioapic;

    unsafe fn map(
        &self,
        gpa: Gpa,
        host: NonNull<u8>,
        len: usize,
        prot: Prot,
    ) -> Result<(), HvError> {
        let mut flags = WHvMapGpaRangeFlagNone;
        for (bit, flag) in [
            (Prot::READ, WHvMapGpaRangeFlagRead),
            (Prot::WRITE, WHvMapGpaRangeFlagWrite),
            (Prot::EXEC, WHvMapGpaRangeFlagExecute),
        ] {
            if prot.contains(bit) {
                flags |= flag;
            }
        }
        // SAFETY: the caller keeps `host..host+len` valid and unmoved until `unmap`.
        check("WHvMapGpaRange", unsafe {
            WHvMapGpaRange(
                self.partition.0,
                host.as_ptr().cast(),
                gpa,
                len as u64,
                flags,
            )
        })?;
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((gpa, len));
        Ok(())
    }

    fn unmap(&self, gpa: Gpa, len: usize) -> Result<(), HvError> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let index = slots
            .iter()
            .position(|slot| *slot == (gpa, len))
            .ok_or(HvError::BadArgument(copy::NOT_MAPPED))?;
        // SAFETY: the range was mapped by `map`.
        check("WHvUnmapGpaRange", unsafe {
            WHvUnmapGpaRange(self.partition.0, gpa, len as u64)
        })?;
        slots.remove(index);
        Ok(())
    }

    fn create_irqchip(&self, _cfg: &IrqchipConfig) -> Result<Arc<Ioapic>, HvError> {
        Ok(Arc::clone(&self.ioapic))
    }

    fn create_vcpu(&self, index: u32) -> Result<WhpVcpu, HvError> {
        let partition = self.partition.0;
        // SAFETY: the partition is set up; `index` is below its processor count.
        check("WHvCreateVirtualProcessor", unsafe {
            WHvCreateVirtualProcessor(partition, index, 0)
        })?;
        let vcpu = WhpVcpu {
            partition: Arc::clone(&self.partition),
            index,
            ioapic: Arc::clone(&self.ioapic),
            emulator: Emulator::new()?,
            kick: WhpKick(Arc::new(Mutex::new(Some((partition, index))))),
        };
        // Only the boot CPU runs at once; the others wait for the INIT and SIPI the guest sends,
        // which the hypervisor's local APIC handles.
        if apic_id(index) != 0 {
            let mut activity = WHV_REGISTER_VALUE { Reg64: 0 };
            // SAFETY: writing the union's 64-bit view of the activity register.
            unsafe {
                let mut state = WHV_INTERNAL_ACTIVITY_REGISTER::default();
                state.AsUINT64 = 1; // StartupSuspend
                activity.Reg64 = state.AsUINT64;
            }
            vcpu.set(&[WHvRegisterInternalActivityState], &[activity])?;
        }
        Ok(vcpu)
    }
}

/// The IOAPIC: 24 redirection entries behind an index and a data register. Every pin is
/// delivered as an edge, as the board's MP table declares them.
pub struct Ioapic {
    partition: Arc<Partition>,
    state: Mutex<IoapicState>,
}

struct IoapicState {
    select: u32,
    id: u32,
    /// Redirection entries, masked until the guest programs them.
    entries: [u64; IOAPIC_PINS],
    levels: [bool; IOAPIC_PINS],
}

const IOAPIC_SELECT: u64 = 0x00;
const IOAPIC_WINDOW: u64 = 0x10;
const IOAPIC_REG_ID: u32 = 0;
const IOAPIC_REG_VERSION: u32 = 1;
const IOAPIC_REG_ARBITRATION: u32 = 2;
const IOAPIC_REG_TABLE: u32 = 0x10;
const IOAPIC_VERSION: u32 = 0x11 | ((IOAPIC_PINS as u32 - 1) << 16);
const RTE_MASKED: u64 = 1 << 16;
/// Bits a guest may not change: delivery status and remote IRR.
const RTE_READ_ONLY: u64 = 1 << 12 | 1 << 14;

impl Ioapic {
    fn new(partition: Arc<Partition>) -> Self {
        Self {
            partition,
            state: Mutex::new(IoapicState {
                select: 0,
                id: 0,
                entries: [RTE_MASKED; IOAPIC_PINS],
                levels: [false; IOAPIC_PINS],
            }),
        }
    }

    fn contains(gpa: Gpa) -> bool {
        (IOAPIC_BASE..IOAPIC_BASE + IOAPIC_SIZE).contains(&gpa)
    }

    fn read(&self, offset: u64) -> u32 {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match offset {
            IOAPIC_SELECT => state.select,
            IOAPIC_WINDOW => match state.select {
                IOAPIC_REG_ID => state.id,
                IOAPIC_REG_VERSION => IOAPIC_VERSION,
                IOAPIC_REG_ARBITRATION => state.id,
                reg if reg >= IOAPIC_REG_TABLE => {
                    let pin = ((reg - IOAPIC_REG_TABLE) / 2) as usize;
                    let entry = state.entries.get(pin).copied().unwrap_or(0);
                    if reg % 2 == 0 {
                        entry as u32
                    } else {
                        (entry >> 32) as u32
                    }
                }
                _ => 0,
            },
            _ => 0,
        }
    }

    fn write(&self, offset: u64, value: u32) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match offset {
            IOAPIC_SELECT => state.select = value & 0xff,
            IOAPIC_WINDOW => match state.select {
                IOAPIC_REG_ID => state.id = value & 0x0f00_0000,
                reg if reg >= IOAPIC_REG_TABLE => {
                    let pin = ((reg - IOAPIC_REG_TABLE) / 2) as usize;
                    let Some(entry) = state.entries.get_mut(pin) else {
                        return;
                    };
                    let (mask, shift) = if reg % 2 == 0 {
                        (0xffff_ffff & !RTE_READ_ONLY, 0)
                    } else {
                        (0xffff_ffff, 32)
                    };
                    *entry = (*entry & !(mask << shift)) | ((u64::from(value) & mask) << shift);
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Sends the interrupt a redirection entry describes.
    fn deliver(&self, entry: u64) -> Result<(), HvError> {
        let vector = (entry & 0xff) as u32;
        let kind = match (entry >> 8) & 0b111 {
            0b000 => WHvX64InterruptTypeFixed,
            0b001 => WHvX64InterruptTypeLowestPriority,
            0b100 => WHvX64InterruptTypeNmi,
            0b101 => WHvX64InterruptTypeInit,
            // SMI and ExtINT have nothing behind them on this board.
            _ => return Ok(()),
        };
        let logical = (entry >> 11) & 1;
        let control = WHV_INTERRUPT_CONTROL {
            // Type, then destination mode; trigger mode edge (0).
            _bitfield: (kind as u64 & 0xff) | (logical << 8),
            Destination: (entry >> 56) as u32,
            Vector: vector,
        };
        // SAFETY: the control block lives for the call.
        check("WHvRequestInterrupt", unsafe {
            WHvRequestInterrupt(
                self.partition.0,
                &control,
                std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32,
            )
        })
    }
}

impl Irqchip for Ioapic {
    fn set_irq(&self, line: u32, level: bool) -> Result<(), HvError> {
        let pin = line as usize;
        let entry = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(previous) = state.levels.get_mut(pin) else {
                return Err(HvError::BadArgument(copy::IRQ_OUT_OF_RANGE));
            };
            let rising = level && !*previous;
            *previous = level;
            if !rising || state.entries[pin] & RTE_MASKED != 0 {
                return Ok(());
            }
            state.entries[pin]
        };
        self.deliver(entry)
    }
}

/// Kicks the vCPU out of `WHvRunVirtualProcessor`. Cleared when the vCPU is dropped.
#[derive(Clone)]
pub struct WhpKick(Arc<Mutex<Option<(WHV_PARTITION_HANDLE, u32)>>>);

impl crate::Kick for WhpKick {
    fn kick(&self) {
        if let Some((partition, index)) = *self.0.lock().unwrap_or_else(PoisonError::into_inner) {
            // SAFETY: while the target is set, the vCPU and its partition exist.
            unsafe { WHvCancelRunVirtualProcessor(partition, index, 0) };
        }
    }
}

/// WinHvEmulation's instruction emulator for one vCPU.
struct Emulator(*mut c_void);

impl Emulator {
    fn new() -> Result<Self, HvError> {
        let callbacks = WHV_EMULATOR_CALLBACKS {
            Size: std::mem::size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(emulator_io_port),
            WHvEmulatorMemoryCallback: Some(emulator_memory),
            WHvEmulatorGetVirtualProcessorRegisters: Some(emulator_get_registers),
            WHvEmulatorSetVirtualProcessorRegisters: Some(emulator_set_registers),
            WHvEmulatorTranslateGvaPage: Some(emulator_translate),
        };
        let mut handle = std::ptr::null_mut();
        // SAFETY: the callbacks are copied by the call; the handle is written.
        check("WHvEmulatorCreateEmulator", unsafe {
            WHvEmulatorCreateEmulator(&callbacks, &mut handle)
        })?;
        Ok(Self(handle))
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        // SAFETY: the emulator is not in use any more.
        unsafe { WHvEmulatorDestroyEmulator(self.0) };
    }
}

/// What the emulator callbacks reach, through the `context` pointer, while one exit is emulated.
struct EmulationContext<'a> {
    partition: WHV_PARTITION_HANDLE,
    index: u32,
    io: &'a dyn IoHandler,
    ioapic: &'a Ioapic,
}

const S_OK: HRESULT = 0;

unsafe extern "system" fn emulator_io_port(
    context: *const c_void,
    access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: the emulator passes back the context given to it, and a valid access record.
    let (context, access) = unsafe { (&*context.cast::<EmulationContext<'_>>(), &mut *access) };
    let size = usize::from(access.AccessSize).min(4);
    let mut data = access.Data.to_le_bytes();
    if access.Direction == 0 {
        context.io.pio_read(access.Port, &mut data[..size]);
        access.Data = u32::from_le_bytes(data);
    } else {
        context.io.pio_write(access.Port, &data[..size]);
    }
    S_OK
}

unsafe extern "system" fn emulator_memory(
    context: *const c_void,
    access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: as for the port callback.
    let (context, access) = unsafe { (&*context.cast::<EmulationContext<'_>>(), &mut *access) };
    let size = usize::from(access.AccessSize).min(8);
    let gpa = access.GpaAddress;
    if Ioapic::contains(gpa) {
        let offset = gpa - IOAPIC_BASE;
        if access.Direction == 0 {
            let value = context.ioapic.read(offset).to_le_bytes();
            access.Data = [0; 8];
            access.Data[..size.min(4)].copy_from_slice(&value[..size.min(4)]);
        } else {
            let mut value = [0u8; 4];
            value[..size.min(4)].copy_from_slice(&access.Data[..size.min(4)]);
            context.ioapic.write(offset, u32::from_le_bytes(value));
        }
    } else if access.Direction == 0 {
        context.io.mmio_read(gpa, &mut access.Data[..size]);
    } else {
        context.io.mmio_write(gpa, &access.Data[..size]);
    }
    S_OK
}

unsafe extern "system" fn emulator_get_registers(
    context: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *mut WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: the emulator passes back our context and arrays of `count` entries.
    unsafe {
        let context = &*context.cast::<EmulationContext<'_>>();
        WHvGetVirtualProcessorRegisters(context.partition, context.index, names, count, values)
    }
}

unsafe extern "system" fn emulator_set_registers(
    context: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *const WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let context = &*context.cast::<EmulationContext<'_>>();
        WHvSetVirtualProcessorRegisters(context.partition, context.index, names, count, values)
    }
}

unsafe extern "system" fn emulator_translate(
    context: *const c_void,
    gva: u64,
    flags: WHV_TRANSLATE_GVA_FLAGS,
    result_code: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> HRESULT {
    // SAFETY: as above; `result_code` and `gpa` are written by us only.
    unsafe {
        let context = &*context.cast::<EmulationContext<'_>>();
        let mut result = WHV_TRANSLATE_GVA_RESULT::default();
        let status = WHvTranslateGva(
            context.partition,
            context.index,
            gva,
            flags,
            &mut result,
            gpa,
        );
        *result_code = result.ResultCode;
        status
    }
}

pub struct WhpVcpu {
    partition: Arc<Partition>,
    index: u32,
    ioapic: Arc<Ioapic>,
    emulator: Emulator,
    kick: WhpKick,
}

impl Drop for WhpVcpu {
    fn drop(&mut self) {
        *self.kick.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
        // SAFETY: the vCPU is not running; nothing refers to it any more.
        unsafe { WHvDeleteVirtualProcessor(self.partition.0, self.index) };
    }
}

const GPR_NAMES: [WHV_REGISTER_NAME; GPR_COUNT] = [
    WHvX64RegisterRax,
    WHvX64RegisterRcx,
    WHvX64RegisterRdx,
    WHvX64RegisterRbx,
    WHvX64RegisterRsp,
    WHvX64RegisterRbp,
    WHvX64RegisterRsi,
    WHvX64RegisterRdi,
    WHvX64RegisterR8,
    WHvX64RegisterR9,
    WHvX64RegisterR10,
    WHvX64RegisterR11,
    WHvX64RegisterR12,
    WHvX64RegisterR13,
    WHvX64RegisterR14,
    WHvX64RegisterR15,
];

const SYSTEM_NAMES: [WHV_REGISTER_NAME; 14] = [
    WHvX64RegisterCs,
    WHvX64RegisterDs,
    WHvX64RegisterEs,
    WHvX64RegisterFs,
    WHvX64RegisterGs,
    WHvX64RegisterSs,
    WHvX64RegisterTr,
    WHvX64RegisterLdtr,
    WHvX64RegisterGdtr,
    WHvX64RegisterIdtr,
    WHvX64RegisterCr0,
    WHvX64RegisterCr3,
    WHvX64RegisterCr4,
    WHvX64RegisterEfer,
];

fn segment_from(value: &WHV_REGISTER_VALUE) -> Segment {
    // SAFETY: the value was read as a segment register.
    let segment = unsafe { value.Segment };
    // SAFETY: both views of the attribute union are plain bits.
    let attributes = unsafe { segment.Anonymous.Attributes };
    let bit = |n: u16| attributes >> n & 1 != 0;
    Segment {
        base: segment.Base,
        limit: segment.Limit,
        selector: segment.Selector,
        kind: (attributes & 0xf) as u8,
        s: bit(4),
        dpl: (attributes >> 5 & 0b11) as u8,
        present: bit(7),
        l: bit(13),
        db: bit(14),
        g: bit(15),
        unusable: !bit(7),
    }
}

fn segment_to(segment: &Segment) -> WHV_REGISTER_VALUE {
    let present = segment.present && !segment.unusable;
    let attributes = u16::from(segment.kind & 0xf)
        | u16::from(segment.s) << 4
        | u16::from(segment.dpl & 0b11) << 5
        | u16::from(present) << 7
        | u16::from(segment.l) << 13
        | u16::from(segment.db) << 14
        | u16::from(segment.g) << 15;
    WHV_REGISTER_VALUE {
        Segment: WHV_X64_SEGMENT_REGISTER {
            Base: segment.base,
            Limit: segment.limit,
            Selector: segment.selector,
            Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
                Attributes: attributes,
            },
        },
    }
}

fn table_to(table: &Table) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE {
        Table: WHV_X64_TABLE_REGISTER {
            Pad: [0; 3],
            Limit: table.limit,
            Base: table.base,
        },
    }
}

impl WhpVcpu {
    fn get<const N: usize>(
        &self,
        names: &[WHV_REGISTER_NAME; N],
    ) -> Result<[WHV_REGISTER_VALUE; N], HvError> {
        let mut values = [WHV_REGISTER_VALUE { Reg64: 0 }; N];
        // SAFETY: both arrays hold N entries.
        check("WHvGetVirtualProcessorRegisters", unsafe {
            WHvGetVirtualProcessorRegisters(
                self.partition.0,
                self.index,
                names.as_ptr(),
                N as u32,
                values.as_mut_ptr(),
            )
        })?;
        Ok(values)
    }

    fn set(
        &self,
        names: &[WHV_REGISTER_NAME],
        values: &[WHV_REGISTER_VALUE],
    ) -> Result<(), HvError> {
        assert_eq!(names.len(), values.len());
        // SAFETY: both slices hold the same number of entries.
        check("WHvSetVirtualProcessorRegisters", unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition.0,
                self.index,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr(),
            )
        })
    }
}

impl Vcpu for WhpVcpu {
    type Kick = WhpKick;

    // The exit reasons are windows-sys constants, named as Windows names them.
    #[allow(non_upper_case_globals)]
    fn run(&mut self, io: &dyn IoHandler) -> Result<Exit, HvError> {
        let context = EmulationContext {
            partition: self.partition.0,
            index: self.index,
            io,
            ioapic: &self.ioapic,
        };
        loop {
            // SAFETY: an all-zero exit context is valid; the run call fills it in.
            let mut exit: WHV_RUN_VP_EXIT_CONTEXT = unsafe { std::mem::zeroed() };
            // SAFETY: the buffer is a whole exit context and lives for the call.
            check("WHvRunVirtualProcessor", unsafe {
                WHvRunVirtualProcessor(
                    self.partition.0,
                    self.index,
                    (&raw mut exit).cast(),
                    std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
                )
            })?;
            let mut status = WHV_EMULATOR_STATUS { AsUINT32: 0 };
            let context_ptr = (&raw const context).cast::<c_void>();
            match exit.ExitReason {
                WHvRunVpExitReasonMemoryAccess => {
                    // SAFETY: a memory-access exit fills in that member; the context outlives the call.
                    check("WHvEmulatorTryMmioEmulation", unsafe {
                        WHvEmulatorTryMmioEmulation(
                            self.emulator.0,
                            context_ptr,
                            &exit.VpContext,
                            &exit.Anonymous.MemoryAccess,
                            &mut status,
                        )
                    })?;
                }
                WHvRunVpExitReasonX64IoPortAccess => {
                    // SAFETY: as above, for the port-access member.
                    check("WHvEmulatorTryIoEmulation", unsafe {
                        WHvEmulatorTryIoEmulation(
                            self.emulator.0,
                            context_ptr,
                            &exit.VpContext,
                            &exit.Anonymous.IoPortAccess,
                            &mut status,
                        )
                    })?;
                }
                WHvRunVpExitReasonCanceled => return Ok(Exit::Kicked),
                // With the hypervisor's local APIC, a halted vCPU waits inside the hypervisor
                // for its next interrupt when run again.
                WHvRunVpExitReasonX64Halt => {
                    std::thread::yield_now();
                    continue;
                }
                // A triple fault: what a PC does to reset itself.
                WHvRunVpExitReasonUnrecoverableException => return Ok(Exit::Reset),
                other => {
                    let rip = exit.VpContext.Rip;
                    return Err(HvError::UnhandledExit(format!(
                        "WHP exit reason {other:#x} at rip {rip:#x}"
                    )));
                }
            }
            // SAFETY: plain bits.
            if unsafe { status.AsUINT32 } & 1 == 0 {
                let rip = exit.VpContext.Rip;
                return Err(HvError::UnhandledExit(format!(
                    "instruction emulation failed (status {:#x}) at rip {rip:#x}",
                    unsafe { status.AsUINT32 }
                )));
            }
        }
    }

    fn regs(&self) -> Result<Regs, HvError> {
        let gpr = self.get(&GPR_NAMES)?;
        let [rip, rflags] = self.get(&[WHvX64RegisterRip, WHvX64RegisterRflags])?;
        // SAFETY: every value was read as a 64-bit register.
        unsafe {
            Ok(Regs {
                gpr: gpr.map(|value| value.Reg64),
                rip: rip.Reg64,
                rflags: rflags.Reg64,
            })
        }
    }

    fn set_regs(&mut self, regs: &Regs) -> Result<(), HvError> {
        let mut names = GPR_NAMES.to_vec();
        names.extend([WHvX64RegisterRip, WHvX64RegisterRflags]);
        let values: Vec<WHV_REGISTER_VALUE> = regs
            .gpr
            .iter()
            .chain([&regs.rip, &regs.rflags])
            .map(|&value| WHV_REGISTER_VALUE { Reg64: value })
            .collect();
        self.set(&names, &values)
    }

    fn system_regs(&self) -> Result<SystemRegs, HvError> {
        let [
            cs,
            ds,
            es,
            fs,
            gs,
            ss,
            tr,
            ldt,
            gdt,
            idt,
            cr0,
            cr3,
            cr4,
            efer,
        ] = self.get(&SYSTEM_NAMES)?;
        // SAFETY: tables were read as tables, the rest as 64-bit registers.
        let table = |value: WHV_REGISTER_VALUE| unsafe {
            Table {
                base: value.Table.Base,
                limit: value.Table.Limit,
            }
        };
        unsafe {
            Ok(SystemRegs {
                cs: segment_from(&cs),
                ds: segment_from(&ds),
                es: segment_from(&es),
                fs: segment_from(&fs),
                gs: segment_from(&gs),
                ss: segment_from(&ss),
                tr: segment_from(&tr),
                ldt: segment_from(&ldt),
                gdt: table(gdt),
                idt: table(idt),
                cr0: cr0.Reg64,
                cr3: cr3.Reg64,
                cr4: cr4.Reg64,
                efer: efer.Reg64,
            })
        }
    }

    fn set_system_regs(&mut self, regs: &SystemRegs) -> Result<(), HvError> {
        let values = [
            segment_to(&regs.cs),
            segment_to(&regs.ds),
            segment_to(&regs.es),
            segment_to(&regs.fs),
            segment_to(&regs.gs),
            segment_to(&regs.ss),
            segment_to(&regs.tr),
            segment_to(&regs.ldt),
            table_to(&regs.gdt),
            table_to(&regs.idt),
            WHV_REGISTER_VALUE { Reg64: regs.cr0 },
            WHV_REGISTER_VALUE { Reg64: regs.cr3 },
            WHV_REGISTER_VALUE { Reg64: regs.cr4 },
            WHV_REGISTER_VALUE { Reg64: regs.efer },
        ];
        self.set(&SYSTEM_NAMES, &values)
    }

    fn kicker(&self) -> WhpKick {
        self.kick.clone()
    }
}

// SAFETY: the partition handle may be used from any thread.
unsafe impl Send for Partition {}
unsafe impl Sync for Partition {}
// SAFETY: as above; the kick only cancels a run by handle and index.
unsafe impl Send for WhpKick {}
unsafe impl Sync for WhpKick {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioapic_keeps_read_only_bits_and_reads_back_entries() {
        let ioapic = Ioapic::new(Arc::new(Partition(0)));
        ioapic.write(IOAPIC_SELECT, IOAPIC_REG_TABLE + 2 * 9);
        ioapic.write(IOAPIC_WINDOW, 0x31 | 1 << 12 | 1 << 14);
        assert_eq!(ioapic.read(IOAPIC_WINDOW), 0x31);
        ioapic.write(IOAPIC_SELECT, IOAPIC_REG_TABLE + 2 * 9 + 1);
        ioapic.write(IOAPIC_WINDOW, 0x0200_0000);
        assert_eq!(ioapic.read(IOAPIC_WINDOW), 0x0200_0000);
        ioapic.write(IOAPIC_SELECT, IOAPIC_REG_VERSION);
        assert_eq!(ioapic.read(IOAPIC_WINDOW) >> 16, 23);
    }

    #[test]
    fn masked_pins_and_held_levels_send_nothing() {
        let ioapic = Ioapic::new(Arc::new(Partition(0)));
        // Masked from reset: no WHv call is made, so a null partition is fine.
        assert!(ioapic.set_irq(4, true).is_ok());
        assert!(ioapic.set_irq(4, true).is_ok());
        assert!(ioapic.set_irq(99, true).is_err());
    }

    #[test]
    fn segments_survive_the_round_trip() {
        let code = Segment {
            base: 0,
            limit: 0xffff_ffff,
            selector: 0x10,
            kind: 0xb,
            present: true,
            dpl: 0,
            db: false,
            s: true,
            l: true,
            g: true,
            unusable: false,
        };
        assert_eq!(segment_from(&segment_to(&code)), code);
    }
}
