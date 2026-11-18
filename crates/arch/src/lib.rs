//! Board description and boot setup. One module per guest architecture; the bootconfig trailer is shared.

use std::fmt;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
pub mod android;
pub mod bootconfig;
mod copy;
pub mod slots;
/// Built on every host: it is plain data layout, so its tests run anywhere.
pub mod x86_64;

#[derive(Debug)]
pub enum ArchError {
    /// The kernel file is not a bootable image for this architecture.
    KernelImage(&'static str),
    /// Something does not fit where the address map puts it.
    Layout(&'static str),
    Bootconfig(&'static str),
    /// An Android boot image the VMM cannot use.
    BootImage(&'static str),
    Fdt(vm_fdt::Error),
    Memory(vm_memory::GuestMemoryError),
    Io(std::io::Error),
}

impl fmt::Display for ArchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KernelImage(why) => write!(f, "{}", copy::kernel_image(why)),
            Self::Layout(why) => write!(f, "{}", copy::layout(why)),
            Self::Bootconfig(why) => write!(f, "{}", copy::bootconfig(why)),
            Self::BootImage(why) => write!(f, "{}", copy::boot_image(why)),
            Self::Fdt(err) => write!(f, "{}", copy::fdt(err)),
            Self::Memory(err) => write!(f, "{}", copy::guest_memory(err)),
            Self::Io(err) => write!(f, "{}", copy::io(err)),
        }
    }
}

impl std::error::Error for ArchError {}

impl From<vm_fdt::Error> for ArchError {
    fn from(err: vm_fdt::Error) -> Self {
        Self::Fdt(err)
    }
}

impl From<vm_memory::GuestMemoryError> for ArchError {
    fn from(err: vm_memory::GuestMemoryError) -> Self {
        Self::Memory(err)
    }
}

impl From<std::io::Error> for ArchError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
