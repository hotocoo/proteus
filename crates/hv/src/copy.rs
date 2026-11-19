//! Every message this crate can show. Code refers to these by name.

#[cfg(target_os = "linux")]
pub const DENIED: &str = "this user may not open /dev/kvm; add it to the kvm group";
#[cfg(target_os = "macos")]
pub const DENIED: &str = "the hypervisor refused this process; the binary must be signed with the \
com.apple.security.hypervisor entitlement (packaging/macos/hv.entitlements)";
#[cfg(target_os = "windows")]
pub const NO_WHP: &str = "Windows Hypervisor Platform (turn on the HypervisorPlatform feature in \"Turn Windows features on or off\", enable virtualization in the firmware, and restart)";
#[cfg(target_os = "windows")]
pub const NOT_MAPPED: &str = "no mapping at that address with that length";
#[cfg(target_os = "windows")]
pub const IRQ_OUT_OF_RANGE: &str = "interrupt line is outside the IOAPIC's pins";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub const DENIED: &str = "the hypervisor refused this process";
pub const VM_BUSY: &str = "this process already runs a virtual machine";

pub fn backend_call_failed(call: &str, code: i64) -> String {
    format!("hypervisor call {call} failed with status {code:#x}")
}

pub fn unsupported(what: &str) -> String {
    format!("not supported on this host: {what}")
}

pub fn bad_argument(what: &str) -> String {
    format!("invalid argument: {what}")
}

pub fn unhandled_exit(what: &str) -> String {
    format!("guest exit with no handler: {what}")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn abort_without_syndrome(gpa: u64, pc: u64, syndrome: u64) -> String {
    format!("data abort at {gpa:#x} (pc {pc:#x}) has no decodable syndrome {syndrome:#x}")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn unknown_exception(class: u8, syndrome: u64, pc: u64) -> String {
    format!("exception class {class:#x}, syndrome {syndrome:#x}, pc {pc:#x}")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn unknown_exit_reason(reason: u32) -> String {
    format!("exit reason {reason}")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn sysreg_ignored(direction: &str, syndrome: u64, pc: u64) -> String {
    format!("system register {direction} ignored (syndrome {syndrome:#x}, pc {pc:#x})")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const SYSREG_READ: &str = "read";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const SYSREG_WRITE: &str = "write";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const UNMAPPED_PAGE_ALIGN: &str =
    "gpa, host address and length must be multiples of the host page size";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const IPA_TOO_WIDE: &str = "guest physical address width exceeds what the host offers";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const IRQ_OUT_OF_RANGE: &str = "interrupt line is outside the controller's SPI range";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const IRQCHIP_EXISTS: &str = "the interrupt controller was already created";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const GIC_LAYOUT_MISMATCH: &str =
    "the board's GIC addresses do not satisfy the host's size or alignment";

#[cfg(target_os = "linux")]
pub const NO_KVM: &str = "KVM (/dev/kvm is missing; enable virtualization and load the kvm module)";
#[cfg(target_os = "linux")]
pub const KVM_API_VERSION: &str = "this kernel's KVM API version";
#[cfg(target_os = "linux")]
pub const KVM_CAPABILITY: &str =
    "a KVM without an in-kernel interrupt controller, user memory, TSS address or immediate exit";
#[cfg(target_os = "linux")]
pub const KVM_UNREADABLE: &str = "guest memory that cannot be read";
#[cfg(target_os = "linux")]
pub const KVM_NOT_MAPPED: &str = "no mapping at that address with that length";
