//! Every message this crate can show. Code refers to these by name.

use std::fmt::Display;

pub const BUS_OVERLAP: &str = "device window overlaps one already on the bus";
pub const BUS_EMPTY_WINDOW: &str = "device window is empty or wraps the address space";

pub fn unmapped_access(direction: &str, gpa: u64, len: usize) -> String {
    format!("guest {direction} of {len} bytes at {gpa:#x} hit no device")
}

pub const ACCESS_READ: &str = "read";
pub const ACCESS_WRITE: &str = "write";

pub fn serial_output_failed(err: &dyn Display) -> String {
    format!("serial output failed: {err}")
}

pub fn virtio_activation_failed(device_type: u32, err: &dyn Display) -> String {
    format!("virtio device type {device_type} failed to start: {err}")
}

pub fn virtio_queue_error(err: &dyn Display) -> String {
    format!("virtio queue error: {err}")
}

pub fn virtio_bad_features(device_type: u32, driver: u64, offered: u64) -> String {
    format!(
        "virtio device type {device_type}: driver chose features {driver:#x} outside the offered {offered:#x}"
    )
}

pub fn virtio_queue_not_ready(device_type: u32) -> String {
    format!("virtio device type {device_type}: driver set DRIVER_OK with an invalid queue")
}

pub fn disk_open_failed(err: &dyn Display) -> String {
    format!("disk image: {err}")
}

pub const DISK_WORKER_GONE: &str = "disk worker thread ended unexpectedly";
pub const DISK_ALREADY_ACTIVE: &str = "device is already active";
pub const DISK_NO_QUEUE: &str = "device was activated without all of its queues";
pub const DISK_WORKER_NAME: &str = "virtio-blk";

pub const DISK_OUT_OF_RANGE: &str = "access runs past the end of the disk";
pub const DISK_BAD_GUEST_ADDRESS: &str = "guest buffer address overflows";
pub const DISK_READ_ONLY_AREA: &str = "write to a read-only part of the disk";
pub const SPARSE_BAD_HEADER: &str = "sparse image header is not one this version understands";
pub const SPARSE_BAD_CHUNK: &str = "sparse image has a malformed chunk";
pub const SPARSE_TRUNCATED: &str = "sparse image is shorter than its chunks say";
pub const SPARSE_SIZE_MISMATCH: &str = "sparse image chunks do not add up to its declared size";
pub const GPT_NAME_TOO_LONG: &str = "partition name is longer than 36 characters";
pub const GPT_TOO_MANY: &str = "more partitions than a GPT holds";
pub const GPT_BAD_SIZE: &str = "partition size is not a multiple of the sector size";
