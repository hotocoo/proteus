//! Every message this crate can show. Code refers to these by name.

use std::fmt::Display;

pub fn kernel_image(why: &str) -> String {
    format!("kernel image rejected: {why}")
}

pub fn layout(why: &str) -> String {
    format!("guest memory layout: {why}")
}

pub fn bootconfig(why: &str) -> String {
    format!("bootconfig: {why}")
}

pub fn boot_image(why: &str) -> String {
    format!("Android boot image: {why}")
}

pub fn fdt(err: &dyn Display) -> String {
    format!("device tree: {err}")
}

pub fn guest_memory(err: &dyn Display) -> String {
    format!("guest memory access: {err}")
}

pub fn io(err: &dyn Display) -> String {
    format!("reading boot file: {err}")
}

#[cfg(target_arch = "aarch64")]
pub const KERNEL_TOO_SHORT: &str = "file is shorter than an ARM64 Image header";
#[cfg(target_arch = "aarch64")]
pub const KERNEL_BAD_MAGIC: &str =
    "missing the ARM64 Image magic; an uncompressed Image is required";
#[cfg(target_arch = "aarch64")]
pub const KERNEL_BAD_OFFSET: &str = "load offset is not 2 MiB aligned";
#[cfg(target_arch = "aarch64")]
pub const KERNEL_OVERLAPS_INITRD: &str = "kernel image reaches into the ramdisk area";
#[cfg(target_arch = "aarch64")]
pub const INITRD_OVERLAPS_FDT: &str = "ramdisk and bootconfig reach into the device tree area";
#[cfg(target_arch = "aarch64")]
pub const FDT_TOO_LARGE: &str = "device tree exceeds 2 MiB";
pub const RAM_TOO_SMALL: &str = "RAM is smaller than the fixed boot layout needs";
#[cfg(target_arch = "aarch64")]
pub const RAM_TOO_LARGE: &str = "RAM would run into the GPU shared-memory window";
#[cfg(target_arch = "aarch64")]
pub const TOO_MANY_VCPUS: &str = "vCPU count is zero or exceeds what the redistributor area holds";
#[cfg(target_arch = "aarch64")]
pub const TOO_MANY_VIRTIO: &str = "more virtio devices than the board has slots";
pub const BOOTCONFIG_BAD_KEY: &str = "keys may hold only letters, digits, '.', '-' and '_'";
pub const BOOTCONFIG_BAD_VALUE: &str = "values may not contain a double quote, newline or NUL";
pub const BOOTCONFIG_TOO_LARGE: &str = "exceeds the kernel's 32 KiB limit";
pub const BOOT_IMAGE_BAD_MAGIC: &str = "not a boot image of the expected kind";
pub const BOOT_IMAGE_BAD_VERSION: &str = "only header versions 3 and 4 are supported";
pub const BOOT_IMAGE_BAD_PAGE_SIZE: &str = "page size is not a power of two";
pub const BOOT_IMAGE_TRUNCATED: &str = "a section runs past the end of the file";

pub const X86_NOT_A_KERNEL: &str = "not an x86_64 kernel (neither an ELF vmlinux nor a bzImage)";
pub const X86_NO_64_BIT_ENTRY: &str =
    "this bzImage has no 64-bit entry point (boot protocol 2.12 or later)";
pub const X86_BAD_ELF: &str = "this ELF file is not a 64-bit kernel the loader can read";
pub const X86_CMDLINE_TOO_LONG: &str = "the kernel command line is too long";
pub const MPTABLE_TOO_MANY_CPUS: &str = "too many CPUs for the MP table (at most 32)";
