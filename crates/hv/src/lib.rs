//! Hypervisor abstraction shared by every host.
//!
//! The weakest backend sets each shape: HVF vCPUs are bound to the thread that
//! created them, WHPX serves MMIO from emulator callbacks, and neither has
//! ioeventfd/irqfd. So a vCPU is not `Send`, I/O is a callback passed into
//! `run`, and every device is exit-driven.

use std::fmt;
use std::ptr::NonNull;
use std::sync::Arc;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
mod copy;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod hvf;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod kvm;
/// Plain register types, built on every host so x86_64 boot code can be tested anywhere.
pub mod x86_64;

#[cfg(target_arch = "aarch64")]
pub use aarch64::Regs;
#[cfg(target_arch = "x86_64")]
pub use x86_64::{Regs, SystemRegs};

/// The one backend that exists on this target.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub type Host = hvf::Hvf;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub type Host = kvm::Kvm;

/// Guest physical address.
pub type Gpa = u64;

#[derive(Debug)]
pub enum HvError {
    /// A hypervisor call returned a failure status.
    Backend {
        call: &'static str,
        code: i64,
    },
    /// The host cannot do what was asked.
    Unsupported(&'static str),
    /// The host refused this process access to its hypervisor.
    Denied,
    /// This process already owns a VM and the backend allows only one.
    Busy,
    BadArgument(&'static str),
    /// The guest did something the backend has no handler for.
    UnhandledExit(String),
}

impl fmt::Display for HvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend { call, code } => write!(f, "{}", copy::backend_call_failed(call, *code)),
            Self::Unsupported(what) => write!(f, "{}", copy::unsupported(what)),
            Self::Denied => f.write_str(copy::DENIED),
            Self::Busy => f.write_str(copy::VM_BUSY),
            Self::BadArgument(what) => write!(f, "{}", copy::bad_argument(what)),
            Self::UnhandledExit(what) => write!(f, "{}", copy::unhandled_exit(what)),
        }
    }
}

impl std::error::Error for HvError {}

/// Access rights of a guest memory mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prot(u8);

impl Prot {
    pub const READ: Prot = Prot(1);
    pub const WRITE: Prot = Prot(1 << 1);
    pub const EXEC: Prot = Prot(1 << 2);
    pub const RW: Prot = Prot(Self::READ.0 | Self::WRITE.0);
    pub const RWX: Prot = Prot(Self::RW.0 | Self::EXEC.0);

    pub fn contains(self, other: Prot) -> bool {
        self.0 & other.0 == other.0
    }
}

/// What the host hypervisor can do, read before any VM exists.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Every `Vm::map` argument must be a multiple of this (16 KiB on Apple Silicon).
    pub host_page_size: usize,
    /// Widest guest physical address space the host can give a VM.
    pub gpa_bits: u8,
    pub max_vcpus: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct VmConfig {
    /// Guest physical address width the board's address map needs.
    pub gpa_bits: u8,
}

/// Where the board wants the interrupt controller.
#[derive(Clone, Copy, Debug)]
pub struct IrqchipConfig {
    #[cfg(target_arch = "aarch64")]
    pub gicd_base: Gpa,
    #[cfg(target_arch = "aarch64")]
    pub gicr_base: Gpa,
}

pub trait HypervisorBackend: Sized + 'static {
    type Vm: Vm;

    fn probe() -> Result<Caps, HvError>;
    fn create_vm(cfg: &VmConfig) -> Result<Self::Vm, HvError>;
}

pub trait Vm: Send + Sync + 'static {
    type Vcpu: Vcpu;
    type Irqchip: Irqchip;

    /// Legal while vCPUs are running; HOST_VISIBLE blobs depend on that.
    ///
    /// # Safety
    /// `host..host+len` stays valid and unmoved until `unmap`.
    /// `gpa`, `host` and `len` are multiples of `Caps::host_page_size`.
    unsafe fn map(
        &self,
        gpa: Gpa,
        host: NonNull<u8>,
        len: usize,
        prot: Prot,
    ) -> Result<(), HvError>;
    fn unmap(&self, gpa: Gpa, len: usize) -> Result<(), HvError>;

    /// Once, before the first `create_vcpu` (HVF GIC rule).
    fn create_irqchip(&self, cfg: &IrqchipConfig) -> Result<Arc<Self::Irqchip>, HvError>;

    /// Must be called on the thread that will run this vCPU.
    fn create_vcpu(&self, index: u32) -> Result<Self::Vcpu, HvError>;
}

/// Deliberately not `Send`.
pub trait Vcpu {
    type Kick: Kick;

    /// Runs until something the VMM must handle. MMIO and port I/O are served
    /// through `io` from inside the call. Idle (WFI/HLT) blocks inside the backend.
    fn run(&mut self, io: &dyn IoHandler) -> Result<Exit, HvError>;
    fn regs(&self) -> Result<Regs, HvError>;
    fn set_regs(&mut self, regs: &Regs) -> Result<(), HvError>;
    /// Control registers, segments and descriptor tables.
    #[cfg(target_arch = "x86_64")]
    fn system_regs(&self) -> Result<SystemRegs, HvError>;
    #[cfg(target_arch = "x86_64")]
    fn set_system_regs(&mut self, regs: &SystemRegs) -> Result<(), HvError>;
    fn kicker(&self) -> Self::Kick;
}

/// Makes a running `Vcpu::run` return `Exit::Kicked`. Callable from any thread.
pub trait Kick: Clone + Send + Sync + 'static {
    fn kick(&self);
}

pub trait IoHandler: Sync {
    fn mmio_read(&self, gpa: Gpa, data: &mut [u8]);
    fn mmio_write(&self, gpa: Gpa, data: &[u8]);
    #[cfg(target_arch = "x86_64")]
    fn pio_read(&self, port: u16, data: &mut [u8]);
    #[cfg(target_arch = "x86_64")]
    fn pio_write(&self, port: u16, data: &[u8]);
}

#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Exit {
    Kicked,
    Shutdown,
    Reset,
    /// PSCI call; arguments are read with `Vcpu::regs`, the result written with `set_regs`.
    #[cfg(target_arch = "aarch64")]
    Hypercall,
}

pub trait Irqchip: Send + Sync + 'static {
    /// Drive one wired, level-triggered line.
    /// `line` is a GICv3 SPI number on ARM64 and an IOAPIC GSI on x86_64.
    fn set_irq(&self, line: u32, level: bool) -> Result<(), HvError>;
}
