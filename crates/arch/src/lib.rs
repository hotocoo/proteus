//! Board description and boot setup. One module per guest architecture; the bootconfig trailer is shared.

use std::fmt;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
pub mod android;
pub mod bootconfig;
mod copy;
pub mod ramdisk;
pub mod slots;
/// Built on every host: it is plain data layout, so its tests run anywhere.
pub mod x86_64;

/// A kernel or ramdisk file, as the loaders read it. On Unix that is the file itself; vm-memory
/// cannot read a Windows file straight into guest memory, so there it goes through a buffer.
#[cfg(unix)]
pub type BootFile = std::fs::File;
#[cfg(not(unix))]
pub use bounce::BootFile;

#[cfg(not(unix))]
mod bounce {
    use std::io::{Read, Seek, SeekFrom};
    use vm_memory::volatile_memory::{Error as VolatileMemoryError, VolatileSlice};
    use vm_memory::{ReadVolatile, bitmap::BitmapSlice};

    pub struct BootFile(std::fs::File);

    impl BootFile {
        pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
            std::fs::File::open(path).map(Self)
        }
    }

    impl From<std::fs::File> for BootFile {
        fn from(file: std::fs::File) -> Self {
            Self(file)
        }
    }

    impl Read for BootFile {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Seek for BootFile {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    impl ReadVolatile for BootFile {
        fn read_volatile<B: BitmapSlice>(
            &mut self,
            buf: &mut VolatileSlice<B>,
        ) -> Result<usize, VolatileMemoryError> {
            let mut bounce = vec![0; buf.len()];
            let read = self
                .0
                .read(&mut bounce)
                .map_err(VolatileMemoryError::IOError)?;
            buf.copy_from(&bounce[..read]);
            Ok(read)
        }
    }
}

/// `COMMAND_LINE_SIZE` in Linux on arm64 and x86, terminator included: longer lines are cut silently.
pub const KERNEL_CMDLINE_SIZE: usize = 2048;

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
