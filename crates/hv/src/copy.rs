//! Every message this crate can show. Code refers to these by name.

#[cfg(target_os = "macos")]
pub const DENIED: &str = "the hypervisor refused this process; the binary must be signed with the \
com.apple.security.hypervisor entitlement (packaging/macos/hv.entitlements)";
#[cfg(not(target_os = "macos"))]
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
