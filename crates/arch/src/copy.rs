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

pub fn fdt(err: &dyn Display) -> String {
    format!("device tree: {err}")
}

pub fn guest_memory(err: &dyn Display) -> String {
    format!("guest memory access: {err}")
}

pub fn io(err: &dyn Display) -> String {
    format!("reading boot file: {err}")
}

pub const KERNEL_TOO_SHORT: &str = "file is shorter than an ARM64 Image header";
pub const KERNEL_BAD_MAGIC: &str =
    "missing the ARM64 Image magic; an uncompressed Image is required";
pub const KERNEL_BAD_OFFSET: &str = "load offset is not 2 MiB aligned";
pub const KERNEL_OVERLAPS_INITRD: &str = "kernel image reaches into the ramdisk area";
pub const INITRD_OVERLAPS_FDT: &str = "ramdisk and bootconfig reach into the device tree area";
pub const FDT_TOO_LARGE: &str = "device tree exceeds 2 MiB";
pub const RAM_TOO_SMALL: &str = "RAM is smaller than the fixed boot layout needs";
pub const RAM_TOO_LARGE: &str = "RAM would run into the GPU shared-memory window";
pub const TOO_MANY_VCPUS: &str = "vCPU count is zero or exceeds what the redistributor area holds";
pub const TOO_MANY_VIRTIO: &str = "more virtio devices than the board has slots";
pub const BOOTCONFIG_BAD_KEY: &str = "keys may hold only letters, digits, '.', '-' and '_'";
pub const BOOTCONFIG_BAD_VALUE: &str = "values may not contain a double quote, newline or NUL";
pub const BOOTCONFIG_TOO_LARGE: &str = "exceeds the kernel's 32 KiB limit";
