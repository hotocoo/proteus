//! Hypervisor.framework backend: Apple Silicon, macOS 15 or later (the in-kernel GICv3 needs 15).

mod ffi;

use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crate::aarch64::{INSN_LEN, esr, mpidr_affinity};
use crate::{
    Caps, Exit, Gpa, HvError, HypervisorBackend, IoHandler, Irqchip, IrqchipConfig, Kick, Prot,
    Regs, Vcpu, Vm, VmConfig, copy,
};

/// Hypervisor.framework keeps one VM per process and addresses it implicitly.
static VM_LIVE: AtomicBool = AtomicBool::new(false);

/// Calls a framework function and turns its status into a `Result` that names the call.
macro_rules! hv_call {
    ($name:ident ( $($arg:expr),* $(,)? )) => {{
        // SAFETY: every function invoked through this macro takes only integers, booleans,
        // retained object handles, or pointers to locals that outlive the call.
        let status = unsafe { ffi::$name($($arg),*) };
        match status {
            ffi::HV_SUCCESS => Ok(()),
            ffi::HV_DENIED => Err(HvError::Denied),
            _ => Err(HvError::Backend { call: stringify!($name), code: i64::from(status as u32) }),
        }
    }};
}

fn host_page_size() -> usize {
    // SAFETY: no arguments, no side effects.
    let size = unsafe { ffi::getpagesize() };
    usize::try_from(size).expect("page size is positive")
}

pub struct Hvf;

impl HypervisorBackend for Hvf {
    type Vm = HvfVm;

    fn probe() -> Result<Caps, HvError> {
        let mut gpa_bits = 0u32;
        hv_call!(hv_vm_config_get_max_ipa_size(&mut gpa_bits))?;
        let mut max_vcpus = 0u32;
        hv_call!(hv_vm_get_max_vcpu_count(&mut max_vcpus))?;
        Ok(Caps {
            host_page_size: host_page_size(),
            gpa_bits: u8::try_from(gpa_bits).unwrap_or(u8::MAX),
            max_vcpus,
        })
    }

    fn create_vm(cfg: &VmConfig) -> Result<HvfVm, HvError> {
        let caps = Self::probe()?;
        if cfg.gpa_bits > caps.gpa_bits {
            return Err(HvError::Unsupported(copy::IPA_TOO_WIDE));
        }
        if VM_LIVE.swap(true, Ordering::SeqCst) {
            return Err(HvError::Busy);
        }
        // SAFETY: returns a retained configuration object, released right after use below.
        let config = unsafe { ffi::hv_vm_config_create() };
        let created = hv_call!(hv_vm_config_set_ipa_size(config, u32::from(cfg.gpa_bits)))
            .and_then(|()| hv_call!(hv_vm_create(config)));
        // SAFETY: `config` came from `hv_vm_config_create` and is not used again.
        unsafe { ffi::os_release(config) };
        if let Err(err) = created {
            VM_LIVE.store(false, Ordering::SeqCst);
            return Err(err);
        }
        Ok(HvfVm {
            irqchip_created: AtomicBool::new(false),
            page_size: caps.host_page_size,
        })
    }
}

pub struct HvfVm {
    irqchip_created: AtomicBool,
    page_size: usize,
}

impl Drop for HvfVm {
    fn drop(&mut self) {
        if let Err(err) = hv_call!(hv_vm_destroy()) {
            log::warn!("{err}");
        }
        VM_LIVE.store(false, Ordering::SeqCst);
    }
}

fn memory_flags(prot: Prot) -> ffi::hv_memory_flags_t {
    [
        (Prot::READ, ffi::HV_MEMORY_READ),
        (Prot::WRITE, ffi::HV_MEMORY_WRITE),
        (Prot::EXEC, ffi::HV_MEMORY_EXEC),
    ]
    .into_iter()
    .filter(|(bit, _)| prot.contains(*bit))
    .fold(0, |flags, (_, flag)| flags | flag)
}

impl Vm for HvfVm {
    type Vcpu = HvfVcpu;
    type Irqchip = HvfGic;

    unsafe fn map(
        &self,
        gpa: Gpa,
        host: NonNull<u8>,
        len: usize,
        prot: Prot,
    ) -> Result<(), HvError> {
        let page = self.page_size;
        if !gpa.is_multiple_of(page as u64)
            || !host.addr().get().is_multiple_of(page)
            || !len.is_multiple_of(page)
        {
            return Err(HvError::BadArgument(copy::UNMAPPED_PAGE_ALIGN));
        }
        let status =
            // SAFETY: the caller guarantees `host..host+len` is valid and stays put until `unmap`.
            unsafe { ffi::hv_vm_map(host.as_ptr().cast(), gpa, len, memory_flags(prot)) };
        if status == ffi::HV_SUCCESS {
            Ok(())
        } else {
            Err(HvError::Backend {
                call: "hv_vm_map",
                code: i64::from(status as u32),
            })
        }
    }

    fn unmap(&self, gpa: Gpa, len: usize) -> Result<(), HvError> {
        hv_call!(hv_vm_unmap(gpa, len))
    }

    fn create_irqchip(&self, cfg: &IrqchipConfig) -> Result<Arc<HvfGic>, HvError> {
        if self.irqchip_created.swap(true, Ordering::SeqCst) {
            return Err(HvError::BadArgument(copy::IRQCHIP_EXISTS));
        }
        let geometry = GicGeometry::query()?;
        if !cfg.gicd_base.is_multiple_of(geometry.distributor_alignment)
            || !cfg
                .gicr_base
                .is_multiple_of(geometry.redistributor_alignment)
        {
            return Err(HvError::BadArgument(copy::GIC_LAYOUT_MISMATCH));
        }
        // SAFETY: returns a retained configuration object, released right after use below.
        let config = unsafe { ffi::hv_gic_config_create() };
        let created = hv_call!(hv_gic_config_set_distributor_base(config, cfg.gicd_base))
            .and_then(|()| hv_call!(hv_gic_config_set_redistributor_base(config, cfg.gicr_base)))
            .and_then(|()| hv_call!(hv_gic_create(config)));
        // SAFETY: `config` came from `hv_gic_config_create` and is not used again.
        unsafe { ffi::os_release(config) };
        created?;
        Ok(Arc::new(HvfGic { geometry }))
    }

    fn create_vcpu(&self, index: u32) -> Result<HvfVcpu, HvError> {
        let mut id: ffi::hv_vcpu_t = 0;
        let mut exit: *const ffi::hv_vcpu_exit_t = ptr::null();
        hv_call!(hv_vcpu_create(&mut id, &mut exit, ptr::null_mut()))?;
        let vcpu = HvfVcpu { id, exit };
        vcpu.set_sys_reg(ffi::HV_SYS_REG_MPIDR_EL1, mpidr_affinity(index))?;
        Ok(vcpu)
    }
}

/// Sizes and placement rules of the framework's GICv3, for the board to check its address map against.
#[derive(Clone, Copy, Debug)]
pub struct GicGeometry {
    pub distributor_size: u64,
    pub distributor_alignment: u64,
    /// Size of one vCPU's redistributor frame pair.
    pub redistributor_size: u64,
    pub redistributor_alignment: u64,
    /// First interrupt ID usable as an SPI, and how many follow it.
    pub spi_base: u32,
    pub spi_count: u32,
}

impl GicGeometry {
    pub fn query() -> Result<Self, HvError> {
        let (mut dist_size, mut dist_align, mut redist_size, mut redist_align) =
            (0usize, 0usize, 0usize, 0usize);
        let (mut spi_base, mut spi_count) = (0u32, 0u32);
        hv_call!(hv_gic_get_distributor_size(&mut dist_size))?;
        hv_call!(hv_gic_get_distributor_base_alignment(&mut dist_align))?;
        hv_call!(hv_gic_get_redistributor_size(&mut redist_size))?;
        hv_call!(hv_gic_get_redistributor_base_alignment(&mut redist_align))?;
        hv_call!(hv_gic_get_spi_interrupt_range(
            &mut spi_base,
            &mut spi_count
        ))?;
        Ok(Self {
            distributor_size: dist_size as u64,
            distributor_alignment: dist_align as u64,
            redistributor_size: redist_size as u64,
            redistributor_alignment: redist_align as u64,
            spi_base,
            spi_count,
        })
    }
}

pub struct HvfGic {
    geometry: GicGeometry,
}

impl HvfGic {
    pub fn geometry(&self) -> GicGeometry {
        self.geometry
    }
}

impl Irqchip for HvfGic {
    fn set_irq(&self, line: u32, level: bool) -> Result<(), HvError> {
        if line >= self.geometry.spi_count {
            return Err(HvError::BadArgument(copy::IRQ_OUT_OF_RANGE));
        }
        // A vCPU idling in WFI sleeps inside the framework, which wakes it for this interrupt itself.
        hv_call!(hv_gic_set_spi(self.geometry.spi_base + line, level))
    }
}

pub struct HvfVcpu {
    id: ffi::hv_vcpu_t,
    /// Exit record owned by the framework; valid while the vCPU exists. The raw pointer also keeps
    /// this type `!Send`, which matches the framework's rule that a vCPU stays on its creating thread.
    exit: *const ffi::hv_vcpu_exit_t,
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        if let Err(err) = hv_call!(hv_vcpu_destroy(self.id)) {
            log::warn!("{err}");
        }
    }
}

impl HvfVcpu {
    fn reg(&self, reg: ffi::hv_reg_t) -> Result<u64, HvError> {
        let mut value = 0;
        hv_call!(hv_vcpu_get_reg(self.id, reg, &mut value))?;
        Ok(value)
    }

    fn set_reg(&self, reg: ffi::hv_reg_t, value: u64) -> Result<(), HvError> {
        hv_call!(hv_vcpu_set_reg(self.id, reg, value))
    }

    fn sys_reg(&self, reg: ffi::hv_sys_reg_t) -> Result<u64, HvError> {
        let mut value = 0;
        hv_call!(hv_vcpu_get_sys_reg(self.id, reg, &mut value))?;
        Ok(value)
    }

    fn set_sys_reg(&self, reg: ffi::hv_sys_reg_t, value: u64) -> Result<(), HvError> {
        hv_call!(hv_vcpu_set_sys_reg(self.id, reg, value))
    }

    /// General-purpose register as a load/store sees it: number 31 is the zero register.
    fn gpr(&self, number: u8) -> Result<u64, HvError> {
        if number == esr::REG_ZR {
            Ok(0)
        } else {
            self.reg(ffi::HV_REG_X0 + u32::from(number))
        }
    }

    fn set_gpr(&self, number: u8, value: u64) -> Result<(), HvError> {
        if number == esr::REG_ZR {
            Ok(())
        } else {
            self.set_reg(ffi::HV_REG_X0 + u32::from(number), value)
        }
    }

    fn advance_pc(&self) -> Result<(), HvError> {
        self.set_reg(ffi::HV_REG_PC, self.reg(ffi::HV_REG_PC)? + INSN_LEN)
    }

    fn handle_mmio(&self, io: &dyn IoHandler, syndrome: u64, gpa: Gpa) -> Result<(), HvError> {
        let Some(access) = esr::decode_data_abort(syndrome) else {
            let pc = self.reg(ffi::HV_REG_PC)?;
            return Err(HvError::UnhandledExit(copy::abort_without_syndrome(
                gpa, pc, syndrome,
            )));
        };
        if access.write {
            let data = self.gpr(access.reg)?.to_le_bytes();
            io.mmio_write(gpa, &data[..access.len]);
        } else {
            let mut data = [0u8; size_of::<u64>()];
            io.mmio_read(gpa, &mut data[..access.len]);
            self.set_gpr(access.reg, access.load_result(&data))?;
        }
        self.advance_pc()
    }

    /// Unmodelled system registers read as zero and ignore writes.
    fn handle_sysreg(&self, syndrome: u64) -> Result<(), HvError> {
        let access = esr::decode_sysreg(syndrome);
        if !access.sysreg.is_expected() {
            let pc = self.reg(ffi::HV_REG_PC)?;
            let direction = if access.is_read {
                copy::SYSREG_READ
            } else {
                copy::SYSREG_WRITE
            };
            log::warn!("{}", copy::sysreg_ignored(direction, syndrome, pc));
        }
        if access.is_read {
            self.set_gpr(access.reg, 0)?;
        }
        self.advance_pc()
    }

    fn handle_exception(&self, io: &dyn IoHandler) -> Result<Option<Exit>, HvError> {
        // SAFETY: `exit` points at this vCPU's exit record, which the framework filled before
        // `hv_vcpu_run` returned and which only this thread reads.
        let exception = unsafe { (*self.exit).exception };
        let syndrome = exception.syndrome;
        match esr::class(syndrome) {
            esr::EC_DATA_ABORT_LOWER => self
                .handle_mmio(io, syndrome, exception.physical_address)
                .map(|()| None),
            esr::EC_HVC64 => Ok(Some(Exit::Hypercall)),
            esr::EC_SYSREG => self.handle_sysreg(syndrome).map(|()| None),
            esr::EC_WFX => {
                // With the framework's GIC, WFI sleeps inside the framework and never gets here
                // (measured: no such exit across a Linux boot). Should one arrive anyway, yielding
                // and re-entering costs host CPU but cannot sleep through an interrupt.
                self.advance_pc()?;
                thread::yield_now();
                Ok(None)
            }
            class => {
                let pc = self.reg(ffi::HV_REG_PC)?;
                Err(HvError::UnhandledExit(copy::unknown_exception(
                    class, syndrome, pc,
                )))
            }
        }
    }
}

impl Vcpu for HvfVcpu {
    type Kick = HvfKick;

    fn run(&mut self, io: &dyn IoHandler) -> Result<Exit, HvError> {
        loop {
            hv_call!(hv_vcpu_run(self.id))?;
            // SAFETY: as in `handle_exception`.
            let reason = unsafe { (*self.exit).reason };
            match reason {
                ffi::HV_EXIT_REASON_CANCELED => return Ok(Exit::Kicked),
                ffi::HV_EXIT_REASON_EXCEPTION => {
                    if let Some(exit) = self.handle_exception(io)? {
                        return Ok(exit);
                    }
                }
                other => return Err(HvError::UnhandledExit(copy::unknown_exit_reason(other))),
            }
        }
    }

    fn regs(&self) -> Result<Regs, HvError> {
        let mut regs = Regs::default();
        for (number, slot) in (ffi::HV_REG_X0..).zip(regs.x.iter_mut()) {
            *slot = self.reg(number)?;
        }
        regs.pc = self.reg(ffi::HV_REG_PC)?;
        regs.pstate = self.reg(ffi::HV_REG_CPSR)?;
        regs.sp_el1 = self.sys_reg(ffi::HV_SYS_REG_SP_EL1)?;
        Ok(regs)
    }

    fn set_regs(&mut self, regs: &Regs) -> Result<(), HvError> {
        for (number, value) in (ffi::HV_REG_X0..).zip(regs.x) {
            self.set_reg(number, value)?;
        }
        self.set_reg(ffi::HV_REG_PC, regs.pc)?;
        self.set_reg(ffi::HV_REG_CPSR, regs.pstate)?;
        self.set_sys_reg(ffi::HV_SYS_REG_SP_EL1, regs.sp_el1)
    }

    fn kicker(&self) -> HvfKick {
        HvfKick { id: self.id }
    }
}

#[derive(Clone)]
pub struct HvfKick {
    id: ffi::hv_vcpu_t,
}

impl Kick for HvfKick {
    fn kick(&self) {
        let mut id = self.id;
        // A vCPU that is already gone cannot be kicked; that is not an error for the caller.
        if let Err(err) = hv_call!(hv_vcpus_exit(&mut id, 1)) {
            log::debug!("{err}");
        }
    }
}
