//! virtio-gpu in its plain 2D form with nowhere to show pixels: a headless display.
//!
//! The guest's display driver gets a mode, creates its framebuffers and flushes them, and every
//! command is acknowledged without the pixels being read. That is enough for a guest that renders in
//! software to finish booting, on machines with no GPU or window at all.
//! Requests come from the guest; only their fixed-size header is read.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use virtio_bindings::virtio_gpu::{
    VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX, VIRTIO_GPU_MAX_SCANOUTS,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_DISPLAY_INFO as CMD_GET_DISPLAY_INFO,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_2D as CMD_FIRST_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING as CMD_LAST_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_UNSPEC as RESP_ERR_UNSPEC,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_DISPLAY_INFO as RESP_OK_DISPLAY_INFO,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_NODATA as RESP_OK_NODATA,
};
use virtio_bindings::virtio_ids::VIRTIO_ID_GPU;
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;

/// Control queue, then cursor queue.
const QUEUE_SIZES: [u16; 2] = [256, 16];
/// `struct virtio_gpu_ctrl_hdr`: type, flags, fence_id (u64), ctx_id, ring_idx and padding.
const HEADER_LEN: usize = 24;
const HEADER_FLAGS_AT: usize = 4;
/// From the flags to the end of the header: everything a response echoes back for a fenced command.
const HEADER_ECHO: std::ops::Range<usize> = HEADER_FLAGS_AT..HEADER_LEN;
/// One entry of the display info response: a rectangle, an enabled flag and flags, all u32.
const DISPLAY_ENTRY_LEN: usize = 24;
const DISPLAY_WIDTH_AT: usize = 8;
const DISPLAY_HEIGHT_AT: usize = 12;
const DISPLAY_ENABLED_AT: usize = 16;
/// `struct virtio_gpu_config`: events_read, events_clear, num_scanouts, num_capsets.
const CONFIG_NUM_SCANOUTS_AT: usize = 8;
const CONFIG_LEN: usize = 16;
const SCANOUTS: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(
        bytes[at..at + size_of::<u32>()]
            .try_into()
            .expect("four bytes"),
    )
}

/// The device's answer to one control command, from that command's header alone.
fn respond(mode: DisplayMode, request: &[u8; HEADER_LEN]) -> Vec<u8> {
    let (kind, body_len) = match word(request, 0) {
        CMD_GET_DISPLAY_INFO => (
            RESP_OK_DISPLAY_INFO,
            DISPLAY_ENTRY_LEN * VIRTIO_GPU_MAX_SCANOUTS as usize,
        ),
        CMD_FIRST_2D..=CMD_LAST_2D => (RESP_OK_NODATA, 0),
        // 3D, capability sets and EDID are not offered, so a driver has no business asking.
        _ => (RESP_ERR_UNSPEC, 0),
    };
    let mut response = vec![0u8; HEADER_LEN + body_len];
    response[..size_of::<u32>()].copy_from_slice(&kind.to_le_bytes());
    // A fenced command is complete as soon as it is answered: nothing here runs later.
    let flags = word(request, HEADER_FLAGS_AT);
    if flags & VIRTIO_GPU_FLAG_FENCE != 0 {
        response[HEADER_ECHO].copy_from_slice(&request[HEADER_ECHO]);
        let kept = flags & (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX);
        response[HEADER_FLAGS_AT..HEADER_FLAGS_AT + size_of::<u32>()]
            .copy_from_slice(&kept.to_le_bytes());
    }
    if kind == RESP_OK_DISPLAY_INFO {
        let first = &mut response[HEADER_LEN..HEADER_LEN + DISPLAY_ENTRY_LEN];
        first[DISPLAY_WIDTH_AT..DISPLAY_WIDTH_AT + 4].copy_from_slice(&mode.width.to_le_bytes());
        first[DISPLAY_HEIGHT_AT..DISPLAY_HEIGHT_AT + 4].copy_from_slice(&mode.height.to_le_bytes());
        first[DISPLAY_ENABLED_AT..DISPLAY_ENABLED_AT + 4].copy_from_slice(&1u32.to_le_bytes());
    }
    response
}

/// Answers every pending control command. Returns whether anything was used.
fn drain_control(
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
    mode: DisplayMode,
) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    while let Some(chain) = queue.pop_descriptor_chain(mem) {
        let head = chain.head_index();
        let mut request = [0u8; HEADER_LEN];
        // A chain too short to hold a header, or with no room for an answer, is returned unused.
        let written = chain
            .clone()
            .reader(mem)
            .ok()
            .and_then(|mut reader| reader.read_exact(&mut request).ok())
            .and_then(|()| chain.writer(mem).ok())
            .and_then(|mut writer| {
                writer
                    .write_all(&respond(mode, &request))
                    .ok()
                    .map(|()| writer.bytes_written())
            })
            .unwrap_or(0);
        queue.add_used(mem, head, written as u32)?;
        used_any = true;
    }
    Ok(used_any)
}

/// Cursor updates need no answer; their buffers are just handed back.
fn drain_cursor(queue: &mut Queue, mem: &GuestMemoryMmap) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    while let Some(chain) = queue.pop_descriptor_chain(mem) {
        queue.add_used(mem, chain.head_index(), 0)?;
        used_any = true;
    }
    Ok(used_any)
}

enum Wake {
    Queue,
    Stop,
}

fn serve(
    mode: DisplayMode,
    mut control: Queue,
    mut cursor: Queue,
    mem: GuestMemoryMmap,
    irq: Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
) {
    while let Ok(Wake::Queue) = wake.recv() {
        let drained = drain_control(&mut control, &mem, mode)
            .and_then(|a| drain_cursor(&mut cursor, &mem).map(|b| a | b));
        match drained {
            Ok(true) => irq.signal_used(),
            Ok(false) => {}
            Err(err) => log::warn!("{}", copy::virtio_queue_error(&err)),
        }
    }
}

pub struct Gpu {
    mode: DisplayMode,
    worker: Option<(Sender<Wake>, JoinHandle<()>)>,
}

impl Gpu {
    pub fn headless(mode: DisplayMode) -> Self {
        Self { mode, worker: None }
    }
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_GPU
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        0
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        config[CONFIG_NUM_SCANOUTS_AT..CONFIG_NUM_SCANOUTS_AT + 4]
            .copy_from_slice(&SCANOUTS.to_le_bytes());
        for (at, byte) in (offset..).zip(data.iter_mut()) {
            *byte = usize::try_from(at)
                .ok()
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError> {
        if self.worker.is_some() {
            return Err(DeviceError::AlreadyActive);
        }
        let mut queues = ctx.queues.into_iter();
        let (Some(control), Some(cursor)) = (queues.next(), queues.next()) else {
            return Err(DeviceError::MissingQueue);
        };
        let (wake, wakes) = mpsc::channel();
        let mode = self.mode;
        let thread = thread::Builder::new()
            .name(copy::GPU_WORKER_NAME.into())
            .spawn(move || serve(mode, control, cursor, ctx.mem, ctx.irq, wakes))?;
        self.worker = Some((wake, thread));
        Ok(())
    }

    fn queue_notify(&mut self, _queue: u16) {
        if let Some((wake, _)) = &self.worker {
            let _ = wake.send(Wake::Queue);
        }
    }

    fn reset(&mut self) {
        if let Some((wake, thread)) = self.worker.take() {
            let _ = wake.send(Wake::Stop);
            if thread.join().is_err() {
                log::error!("{}", copy::GPU_WORKER_GONE);
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use virtio_bindings::virtio_gpu::{
        virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_FLUSH as CMD_RESOURCE_FLUSH,
        virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SUBMIT_3D as CMD_SUBMIT_3D,
    };
    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::{Address, Bytes, GuestAddress};

    use super::*;

    const MODE: DisplayMode = DisplayMode {
        width: 1280,
        height: 720,
    };
    const MEM_SIZE: usize = 2 << 20;
    const REQUEST_AT: u64 = 0x10_0000;
    const RESPONSE_AT: u64 = 0x11_0000;
    const QUEUE_LEN: u16 = 16;
    const DISPLAY_INFO_LEN: usize = HEADER_LEN + DISPLAY_ENTRY_LEN * 16;

    fn header(kind: u32, flags: u32, fence: u64, ctx: u32) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&kind.to_le_bytes());
        header[4..8].copy_from_slice(&flags.to_le_bytes());
        header[8..16].copy_from_slice(&fence.to_le_bytes());
        header[16..20].copy_from_slice(&ctx.to_le_bytes());
        header
    }

    #[test]
    fn display_info_reports_one_enabled_scanout_of_the_configured_size() {
        let response = respond(MODE, &header(CMD_GET_DISPLAY_INFO, 0, 0, 0));
        assert_eq!(response.len(), DISPLAY_INFO_LEN);
        assert_eq!(word(&response, 0), RESP_OK_DISPLAY_INFO);
        let first = &response[HEADER_LEN..];
        assert_eq!(
            [
                word(first, 0),
                word(first, 4),
                word(first, 8),
                word(first, 12),
                word(first, 16)
            ],
            [0, 0, 1280, 720, 1]
        );
        assert!(
            response[HEADER_LEN + DISPLAY_ENTRY_LEN..]
                .iter()
                .all(|byte| *byte == 0),
            "other scanouts are off"
        );
    }

    #[test]
    fn two_d_commands_are_acknowledged_and_everything_else_is_an_error() {
        for kind in CMD_FIRST_2D..=CMD_LAST_2D {
            let response = respond(MODE, &header(kind, 0, 0, 0));
            assert_eq!(
                (response.len(), word(&response, 0)),
                (HEADER_LEN, RESP_OK_NODATA),
                "command {kind:#x}"
            );
        }
        for kind in [0, CMD_SUBMIT_3D, CMD_LAST_2D + 1, u32::MAX] {
            assert_eq!(
                word(&respond(MODE, &header(kind, 0, 0, 0)), 0),
                RESP_ERR_UNSPEC,
                "command {kind:#x}"
            );
        }
    }

    #[test]
    fn fenced_command_gets_its_fence_back_and_an_unfenced_one_gets_zeros() {
        let fenced = respond(
            MODE,
            &header(
                CMD_RESOURCE_FLUSH,
                VIRTIO_GPU_FLAG_FENCE | 0x80,
                0xfeed_beef,
                7,
            ),
        );
        assert_eq!(
            word(&fenced, 4),
            VIRTIO_GPU_FLAG_FENCE,
            "only defined flags are echoed"
        );
        assert_eq!(
            u64::from_le_bytes(fenced[8..16].try_into().unwrap()),
            0xfeed_beef
        );
        assert_eq!(word(&fenced, 16), 7);
        let plain = respond(MODE, &header(CMD_RESOURCE_FLUSH, 0, 0xfeed_beef, 7));
        assert!(plain[4..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn control_queue_answers_in_the_guests_buffer_and_survives_bad_chains() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
        let mock = MockSplitQueue::new(&mem, QUEUE_LEN);
        mem.write_slice(
            &header(CMD_GET_DISPLAY_INFO, 0, 0, 0),
            GuestAddress(REQUEST_AT),
        )
        .unwrap();
        let write = VRING_DESC_F_WRITE as u16;
        let chains = [
            // A normal request with its response buffer.
            RawDescriptor::from(Descriptor::new(REQUEST_AT, HEADER_LEN as u32, 1, 1)),
            RawDescriptor::from(Descriptor::new(RESPONSE_AT, 1024, write, 0)),
            // A request shorter than a header.
            RawDescriptor::from(Descriptor::new(REQUEST_AT, 8, 1, 3)),
            RawDescriptor::from(Descriptor::new(RESPONSE_AT + 0x1000, 1024, write, 0)),
            // A request with no response buffer.
            RawDescriptor::from(Descriptor::new(REQUEST_AT, HEADER_LEN as u32, 0, 0)),
        ];
        mock.add_desc_chains(&chains, 0).unwrap();
        let mut queue: Queue = mock.create_queue().unwrap();

        assert!(drain_control(&mut queue, &mem, MODE).unwrap());
        let used_len = |index: u64| -> u32 {
            mem.read_obj(mock.used_addr().unchecked_add(4 + index * 8 + 4))
                .unwrap()
        };
        assert_eq!(
            [used_len(0), used_len(1), used_len(2)],
            [DISPLAY_INFO_LEN as u32, 0, 0]
        );
        let mut response = vec![0u8; DISPLAY_INFO_LEN];
        mem.read_slice(&mut response, GuestAddress(RESPONSE_AT))
            .unwrap();
        assert_eq!(
            response,
            respond(MODE, &header(CMD_GET_DISPLAY_INFO, 0, 0, 0))
        );
        assert!(
            !drain_control(&mut queue, &mem, MODE).unwrap(),
            "nothing left"
        );
    }

    #[test]
    fn config_space_reports_one_scanout() {
        let gpu = Gpu::headless(MODE);
        let mut config = [0xffu8; 20];
        gpu.read_config(0, &mut config);
        assert_eq!(word(&config, CONFIG_NUM_SCANOUTS_AT), 1);
        assert!(
            config[..8]
                .iter()
                .chain(&config[12..])
                .all(|byte| *byte == 0)
        );
    }
}
