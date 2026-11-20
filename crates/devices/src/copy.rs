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
pub const GPU_WORKER_NAME: &str = "virtio-gpu";
pub const GPU_WORKER_GONE: &str = "display worker thread ended unexpectedly";
pub const INPUT_WORKER_NAME: &str = "virtio-input";
pub const INPUT_WORKER_GONE: &str = "input worker thread ended unexpectedly";
pub const CONSOLE_WORKER_NAME: &str = "virtio-console";
pub const SOUND_WORKER_NAME: &str = "virtio-snd";
pub const SOUND_WORKER_GONE: &str = "sound worker thread ended unexpectedly";
#[cfg(unix)]
pub const BALLOON_WORKER_NAME: &str = "virtio-balloon";
#[cfg(unix)]
pub const BALLOON_WORKER_GONE: &str = "memory-return worker thread ended unexpectedly";
#[cfg(unix)]
pub fn balloon_discard_failed(err: &std::io::Error) -> String {
    format!("could not give freed guest memory back to the host: {err}")
}
pub const VSOCK_WORKER_NAME: &str = "virtio-vsock";
pub const VSOCK_WORKER_GONE: &str = "vsock worker thread ended unexpectedly";
pub const CONSOLE_WORKER_GONE: &str = "console worker thread ended unexpectedly";

pub fn gpu_renderer_refused(what: &str, why: &dyn std::fmt::Display) -> String {
    format!("virtio-gpu: the renderer refused a {what} command: {why}")
}

pub fn gpu_blob_misaligned(resource: u32, offset: u64, len: u64) -> String {
    format!(
        "virtio-gpu: blob {resource} at {offset:#x}, {len:#x} bytes, is not whole host pages inside the window"
    )
}

pub fn gpu_renderer_failed(why: &dyn std::fmt::Display) -> String {
    format!("virtio-gpu: the host renderer did not start, so the display is unaccelerated: {why}")
}

pub fn gpu_readback_failed(code: u32) -> String {
    format!("virtio-gpu: reading back a frame failed ({code:#x})")
}
pub fn gpu_readback(frames: u32, took: std::time::Duration, over: std::time::Duration) -> String {
    let average = took.as_secs_f64() * 1000.0 / f64::from(frames.max(1));
    format!(
        "virtio-gpu: {:.1} frames read back per second, {average:.2} ms each",
        f64::from(frames) / over.as_secs_f64()
    )
}
