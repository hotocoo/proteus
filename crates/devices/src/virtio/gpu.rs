//! virtio-gpu in its plain 2D form: the guest renders in software and hands over finished frames.
//!
//! With nowhere to show them (headless), every command is acknowledged without the pixels being read.
//! That is enough for a guest to finish booting on a machine with no GPU or window at all.
//! With a `FrameSink`, the device keeps the guest's 2D resources, copies what the guest transfers out
//! of guest memory, and presents the scanout each time the guest flushes it.
//!
//! With a host renderer (`gpu3d`), the device offers 3D, blob resources and context types instead,
//! and hands those commands to it.
//!
//! Every field of every command comes from the guest and is checked before it sizes or indexes anything.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use virtio_bindings::virtio_gpu::{
    VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX, VIRTIO_GPU_MAX_SCANOUTS,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_DISPLAY_INFO as CMD_GET_DISPLAY_INFO,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_EDID as CMD_GET_EDID,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING as CMD_ATTACH_BACKING,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_2D as CMD_FIRST_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_2D as CMD_CREATE_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING as CMD_DETACH_BACKING,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING as CMD_LAST_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_FLUSH as CMD_FLUSH,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_UNREF as CMD_UNREF,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SET_SCANOUT as CMD_SET_SCANOUT,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D as CMD_TRANSFER_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER as RESP_ERR_INVALID_PARAMETER,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID as RESP_ERR_INVALID_RESOURCE_ID,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID as RESP_ERR_INVALID_SCANOUT_ID,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY as RESP_ERR_OUT_OF_MEMORY,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_UNSPEC as RESP_ERR_UNSPEC,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_DISPLAY_INFO as RESP_OK_DISPLAY_INFO,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_EDID as RESP_OK_EDID,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_NODATA as RESP_OK_NODATA,
};
use virtio_bindings::virtio_ids::VIRTIO_ID_GPU;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use virtio_bindings::virtio_gpu::{
    VIRTIO_GPU_F_CONTEXT_INIT, VIRTIO_GPU_F_EDID, VIRTIO_GPU_F_RESOURCE_BLOB, VIRTIO_GPU_F_VIRGL,
};

use super::gpu3d::{BlobMapper, Fence, Gpu3d, Header, PendingFences, RendererFactory};
use super::{Activate, DeviceError, ShmRegion, VirtioDevice, VirtioInterrupt};
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
const CONFIG_NUM_CAPSETS_AT: usize = 12;
const CONFIG_LEN: usize = 16;
const SCANOUTS: u32 = 1;
/// Every pixel format virtio-gpu defines for 2D resources is four bytes wide.
pub const BYTES_PER_PIXEL: usize = 4;
/// Limits on what a guest can make the device allocate.
const MAX_DIMENSION: u32 = 8192;
const MAX_RESOURCES: usize = 256;
/// Most pixel memory all 2D resources together may hold (1 GiB); the per-resource limit alone allows 64 GiB.
const MAX_RESOURCE_BYTES: usize = 1 << 30;
const MAX_BACKING_ENTRIES: u32 = 16_384;
/// `struct virtio_gpu_resp_edid` after the header: size, padding, then room for the EDID.
const EDID_ROOM: usize = 1024;
/// Pixels per inch the display claims in its EDID: the density Android is told to use
/// (`androidboot.lcd_density`), so the composer's DPI and the system's agree.
pub const EDID_DPI: u32 = 320;
/// How long a stopping display worker gets to tear the renderer down. gfxstream's teardown waits
/// for every render thread to leave, and a render thread whose guest was switched off mid-frame
/// waits for guest commands forever: the window then never closed.
const WORKER_STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the stopping worker is checked on.
const WORKER_STOP_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// A finished frame: `width * height` pixels of four bytes each, rows top to bottom.
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// One of the `VIRTIO_GPU_FORMAT_*` values; it says which byte is which channel.
    pub format: u32,
    pub pixels: &'a [u8],
}

/// Where finished frames go. Called on the display's worker thread, once per guest flush.
pub trait FrameSink: Send + Sync {
    fn present(&self, frame: &Frame<'_>);

    /// As `present`, for a frame whose buffer the sink may keep instead of copying. Returns a
    /// buffer the caller can fill with a later frame: this one, or one the sink let go of.
    fn present_owned(&self, width: u32, height: u32, format: u32, pixels: Vec<u8>) -> Vec<u8> {
        self.present(&Frame {
            width,
            height,
            format,
            pixels: &pixels,
        });
        pixels
    }

    /// Memory of the sink's own that the next `width` by `height` frame can be read into, so the
    /// renderer's copy is the only one. `None`: use `present_owned`. The slot comes back through
    /// `present_slot` once filled, or is dropped, unshown, if the read failed.
    fn slot(&self, _width: u32, _height: u32) -> Option<Box<dyn FrameSlot>> {
        None
    }

    /// Shows a slot from `slot`, filled with a frame of `format`.
    fn present_slot(&self, _slot: Box<dyn FrameSlot>, _format: u32) {}
}

/// Memory a sink lends the device to read one frame into, `width * height * BYTES_PER_PIXEL`
/// bytes, rows tightly packed.
pub trait FrameSlot: std::any::Any + Send {
    fn bytes(&mut self) -> &mut [u8];
}

/// A 2D image the guest created, with the host's copy of its pixels.
struct Resource {
    width: u32,
    height: u32,
    format: u32,
    /// Where in guest memory the guest keeps its copy.
    backing: Vec<(GuestAddress, u32)>,
    pixels: Vec<u8>,
}

/// A rectangle as commands carry it: x, y, width, height.
#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Reads little-endian fields off a command body. A short body reads as an error.
struct Body<'a, R: Read>(&'a mut R);

impl<R: Read> Body<'_, R> {
    fn u32(&mut self) -> Result<u32, u32> {
        let mut raw = [0u8; size_of::<u32>()];
        self.0
            .read_exact(&mut raw)
            .map_err(|_| RESP_ERR_INVALID_PARAMETER)?;
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, u32> {
        let mut raw = [0u8; size_of::<u64>()];
        self.0
            .read_exact(&mut raw)
            .map_err(|_| RESP_ERR_INVALID_PARAMETER)?;
        Ok(u64::from_le_bytes(raw))
    }

    fn rect(&mut self) -> Result<Rect, u32> {
        Ok(Rect {
            x: self.u32()?,
            y: self.u32()?,
            width: self.u32()?,
            height: self.u32()?,
        })
    }
}

/// Copies `out.len()` bytes starting `offset` bytes into the guest's backing store.
fn read_backing(
    mem: &GuestMemoryMmap,
    backing: &[(GuestAddress, u32)],
    mut offset: u64,
    mut out: &mut [u8],
) -> Option<()> {
    for (addr, len) in backing {
        let len = u64::from(*len);
        if offset >= len {
            offset -= len;
            continue;
        }
        let take = usize::try_from(len - offset).ok()?.min(out.len());
        let (now, later) = out.split_at_mut(take);
        mem.read_slice(now, addr.checked_add(offset)?).ok()?;
        out = later;
        offset = 0;
        if out.is_empty() {
            return Some(());
        }
    }
    out.is_empty().then_some(())
}

/// The state behind the control queue.
struct Display {
    mode: DisplayMode,
    sink: Option<Arc<dyn FrameSink>>,
    resources: HashMap<u32, Resource>,
    /// The resource the guest attached to the one scanout, and the part of it that is shown.
    scanout: Option<(u32, Rect)>,
    /// The shown part's pixels, when it is not the whole resource. Kept to reuse its memory.
    cropped: Vec<u8>,
}

impl Display {
    fn resource(&mut self, id: u32) -> Result<&mut Resource, u32> {
        self.resources
            .get_mut(&id)
            .ok_or(RESP_ERR_INVALID_RESOURCE_ID)
    }

    /// Carries out one 2D command. Returns the error response code if the guest got something wrong.
    fn execute(
        &mut self,
        mem: &GuestMemoryMmap,
        kind: u32,
        body: &mut impl Read,
    ) -> Result<(), u32> {
        let mut body = Body(body);
        match kind {
            CMD_CREATE_2D => {
                let (id, format, width, height) =
                    (body.u32()?, body.u32()?, body.u32()?, body.u32()?);
                let sane = id != 0
                    && (1..=MAX_DIMENSION).contains(&width)
                    && (1..=MAX_DIMENSION).contains(&height);
                if !sane {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                // Replacing a resource would leave the scanout pointing at the old one's geometry.
                if self.resources.contains_key(&id) {
                    return Err(RESP_ERR_INVALID_RESOURCE_ID);
                }
                let bytes = width as usize * height as usize * BYTES_PER_PIXEL;
                let held: usize = self.resources.values().map(|r| r.pixels.len()).sum();
                if self.resources.len() >= MAX_RESOURCES
                    || held.saturating_add(bytes) > MAX_RESOURCE_BYTES
                {
                    return Err(RESP_ERR_OUT_OF_MEMORY);
                }
                let pixels = vec![0u8; bytes];
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        backing: Vec::new(),
                        pixels,
                    },
                );
            }
            CMD_UNREF => {
                let id = body.u32()?;
                self.resources
                    .remove(&id)
                    .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                if self.scanout.is_some_and(|(shown, _)| shown == id) {
                    self.scanout = None;
                }
            }
            CMD_SET_SCANOUT => {
                let rect = body.rect()?;
                let (scanout, id) = (body.u32()?, body.u32()?);
                if scanout >= SCANOUTS {
                    return Err(RESP_ERR_INVALID_SCANOUT_ID);
                }
                // Resource zero turns the scanout off.
                self.scanout = match id {
                    0 => None,
                    id => {
                        let resource = self.resource(id)?;
                        // The shown part has to lie inside the resource and have some area.
                        let right = rect.x.checked_add(rect.width);
                        let bottom = rect.y.checked_add(rect.height);
                        let inside = right.is_some_and(|right| right <= resource.width)
                            && bottom.is_some_and(|bottom| bottom <= resource.height);
                        if !inside || rect.width == 0 || rect.height == 0 {
                            return Err(RESP_ERR_INVALID_PARAMETER);
                        }
                        Some((id, rect))
                    }
                };
            }
            CMD_ATTACH_BACKING => {
                let (id, entries) = (body.u32()?, body.u32()?);
                if entries > MAX_BACKING_ENTRIES {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                let mut backing = Vec::with_capacity(entries as usize);
                for _ in 0..entries {
                    let (addr, len, _padding) = (body.u64()?, body.u32()?, body.u32()?);
                    backing.push((GuestAddress(addr), len));
                }
                self.resource(id)?.backing = backing;
            }
            CMD_DETACH_BACKING => {
                let id = body.u32()?;
                self.resource(id)?.backing = Vec::new();
            }
            CMD_TRANSFER_2D => {
                let rect = body.rect()?;
                let (offset, id) = (body.u64()?, body.u32()?);
                let resource = self.resource(id)?;
                let inside = rect
                    .x
                    .checked_add(rect.width)
                    .is_some_and(|right| right <= resource.width)
                    && rect
                        .y
                        .checked_add(rect.height)
                        .is_some_and(|bottom| bottom <= resource.height);
                if !inside {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                let stride = resource.width as usize * BYTES_PER_PIXEL;
                let row_len = rect.width as usize * BYTES_PER_PIXEL;
                for row in 0..rect.height as usize {
                    // The guest's copy has the same layout as ours; `offset` is where the rectangle starts in it.
                    let from = offset
                        .checked_add((row * stride) as u64)
                        .ok_or(RESP_ERR_INVALID_PARAMETER)?;
                    let to = (rect.y as usize + row) * stride + rect.x as usize * BYTES_PER_PIXEL;
                    read_backing(
                        mem,
                        &resource.backing,
                        from,
                        &mut resource.pixels[to..to + row_len],
                    )
                    .ok_or(RESP_ERR_INVALID_PARAMETER)?;
                }
            }
            CMD_FLUSH => {
                let _rect = body.rect()?;
                let id = body.u32()?;
                let shown = self.scanout.filter(|(shown, _)| *shown == id);
                let resource = self
                    .resources
                    .get(&id)
                    .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                // The shown part must still lie inside the resource before it is sliced.
                let fits = |rect: &Rect| {
                    rect.x
                        .checked_add(rect.width)
                        .is_some_and(|r| r <= resource.width)
                        && rect
                            .y
                            .checked_add(rect.height)
                            .is_some_and(|b| b <= resource.height)
                };
                if let (Some((_, rect)), Some(sink)) =
                    (shown.filter(|(_, rect)| fits(rect)), &self.sink)
                {
                    // A resource can be larger than what is shown of it (drivers round sizes up).
                    let whole = rect.width == resource.width && rect.height == resource.height;
                    let pixels = if whole {
                        &resource.pixels
                    } else {
                        let stride = resource.width as usize * BYTES_PER_PIXEL;
                        let left = rect.x as usize * BYTES_PER_PIXEL;
                        let row_len = rect.width as usize * BYTES_PER_PIXEL;
                        let rows = resource.pixels.chunks_exact(stride);
                        self.cropped.clear();
                        for row in rows.skip(rect.y as usize).take(rect.height as usize) {
                            self.cropped.extend_from_slice(&row[left..left + row_len]);
                        }
                        &self.cropped
                    };
                    sink.present(&Frame {
                        width: rect.width,
                        height: rect.height,
                        format: resource.format,
                        pixels,
                    });
                }
            }
            _ => return Err(RESP_ERR_UNSPEC),
        }
        Ok(())
    }

    /// The device's answer to one control command.
    fn handle(
        &mut self,
        mem: &GuestMemoryMmap,
        request: &[u8; HEADER_LEN],
        body: &mut impl Read,
    ) -> Vec<u8> {
        let kind = word(request, 0);
        if kind == CMD_GET_EDID {
            return edid_response(self.mode, request, body);
        }
        let outcome = match kind {
            CMD_GET_DISPLAY_INFO => RESP_OK_DISPLAY_INFO,
            // With nowhere to show frames, there is nothing to keep track of.
            CMD_FIRST_2D..=CMD_LAST_2D if self.sink.is_none() => RESP_OK_NODATA,
            CMD_FIRST_2D..=CMD_LAST_2D => self
                .execute(mem, kind, body)
                .map_or_else(|error| error, |()| RESP_OK_NODATA),
            // 3D and capability sets are not offered, so a driver has no business asking.
            _ => RESP_ERR_UNSPEC,
        };
        respond(self.mode, request, outcome)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    /// Frames per second the display claims, through its EDID.
    pub refresh_hz: u32,
}

impl DisplayMode {
    /// The display's EDID, or `None` if this mode cannot be described by one.
    fn edid(self) -> Option<[u8; super::edid::EDID_LEN]> {
        super::edid::build(self.width, self.height, self.refresh_hz, EDID_DPI)
    }
}

/// The answer to `GET_EDID`: the display's EDID for scanout 0.
fn edid_response(mode: DisplayMode, request: &[u8; HEADER_LEN], body: &mut impl Read) -> Vec<u8> {
    let scanout = Body(body).u32();
    let edid = match scanout {
        Ok(scanout) if scanout < SCANOUTS => mode.edid(),
        _ => return respond(mode, request, RESP_ERR_INVALID_SCANOUT_ID),
    };
    let Some(edid) = edid else {
        return respond(mode, request, RESP_ERR_UNSPEC);
    };
    let mut answer = vec![0u8; 2 * size_of::<u32>() + EDID_ROOM];
    answer[..size_of::<u32>()].copy_from_slice(&(edid.len() as u32).to_le_bytes());
    answer[2 * size_of::<u32>()..][..edid.len()].copy_from_slice(&edid);
    respond_with(mode, request, RESP_OK_EDID, &answer)
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(
        bytes[at..at + size_of::<u32>()]
            .try_into()
            .expect("four bytes"),
    )
}

/// Builds the response of the given kind to `request`.
fn respond(mode: DisplayMode, request: &[u8; HEADER_LEN], kind: u32) -> Vec<u8> {
    let body_len = if kind == RESP_OK_DISPLAY_INFO {
        DISPLAY_ENTRY_LEN * VIRTIO_GPU_MAX_SCANOUTS as usize
    } else {
        0
    };
    respond_with(mode, request, kind, &vec![0u8; body_len])
}

/// Builds the response of the given kind to `request`, with `body` after the header.
fn respond_with(mode: DisplayMode, request: &[u8; HEADER_LEN], kind: u32, body: &[u8]) -> Vec<u8> {
    let mut response = vec![0u8; HEADER_LEN];
    response.extend_from_slice(body);
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

/// The 3D side: the renderer's state, where frames go, and answers waiting for their fences.
struct Accelerated {
    gpu: Gpu3d,
    sink: Option<Arc<dyn FrameSink>>,
    /// Descriptor heads and bytes written, of fenced commands not yet answered.
    pending: PendingFences<(u16, u32)>,
}

impl Accelerated {
    /// Reads back and presents the frame the guest flushed, if one waits.
    fn present_due(&mut self) {
        if self.present_due_into_slot() {
            return;
        }
        match self.gpu.read_back() {
            Some(Ok(super::gpu3d::Answer::Frame {
                width,
                height,
                format,
                pixels,
            })) => {
                let spare = match &self.sink {
                    Some(sink) => sink.present_owned(width, height, format, pixels),
                    None => pixels,
                };
                self.gpu.recycle(spare);
            }
            Some(Err(code)) => log::debug!("{}", copy::gpu_readback_failed(code)),
            Some(Ok(_)) | None => {}
        }
    }

    /// Reads the due frame straight into a slot the sink lends, if it lends one. False: nothing
    /// was read, and the frame is still due.
    fn present_due_into_slot(&mut self) -> bool {
        let Some(sink) = &self.sink else {
            return false;
        };
        let Some((width, height)) = self.gpu.due_size() else {
            return false;
        };
        let Some(mut slot) = sink.slot(width, height) else {
            return false;
        };
        match self.gpu.read_back_into(slot.bytes()) {
            Some(Ok(format)) => sink.present_slot(slot, format),
            Some(Err(code)) => log::debug!("{}", copy::gpu_readback_failed(code)),
            None => {}
        }
        true
    }

    /// The answer to one control command, and the fence it waits for, if any.
    fn handle(
        &mut self,
        mode: DisplayMode,
        mem: &GuestMemoryMmap,
        request: &[u8; HEADER_LEN],
        body: &mut impl Read,
    ) -> (Vec<u8>, Option<Fence>) {
        let header = Header {
            kind: word(request, 0),
            flags: word(request, HEADER_FLAGS_AT),
            fence: u64::from(word(request, 8)) | (u64::from(word(request, 12)) << 32),
            ctx: word(request, 16),
            ring: request[20],
        };
        if header.kind == CMD_GET_DISPLAY_INFO {
            return (respond(mode, request, RESP_OK_DISPLAY_INFO), None);
        }
        if header.kind == CMD_GET_EDID {
            return (edid_response(mode, request, body), None);
        }
        let (kind, answer) = match self.gpu.execute(mem, &header, body) {
            Ok(answer) => answer.response(),
            Err(error) => (error, Vec::new()),
        };
        // The answer waits for the fence, if the renderer takes it; otherwise it goes now.
        let fence = Fence::of(&header).filter(|fence| match self.gpu.fence(*fence) {
            Ok(()) => true,
            Err(why) => {
                log::debug!("{}", copy::gpu_renderer_refused("fence", &why));
                false
            }
        });
        (respond_with(mode, request, kind, &answer), fence)
    }
}

/// The two ways the display works.
enum Backend {
    Plain(Display),
    Accelerated(Box<Accelerated>),
}

/// Answers every pending control command. Returns whether anything was used.
fn drain_control(
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
    backend: &mut Backend,
    mode: DisplayMode,
    irq: &VirtioInterrupt,
) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    // The guest is told not to kick while commands are being answered: it sends thousands a
    // second, and each kick is a trip out of the guest and a wake of this thread.
    queue.disable_notification(mem)?;
    loop {
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let mut request = [0u8; HEADER_LEN];
            // A chain too short to hold a header, or with no room for an answer, is returned unused.
            let handled = chain.clone().reader(mem).ok().and_then(|mut reader| {
                reader.read_exact(&mut request).ok()?;
                Some(match backend {
                    Backend::Plain(display) => (display.handle(mem, &request, &mut reader), None),
                    Backend::Accelerated(gpu) => gpu.handle(mode, mem, &request, &mut reader),
                })
            });
            let (response, fence) = handled.unzip();
            let written = response
                .and_then(|response| {
                    let mut writer = chain.writer(mem).ok()?;
                    writer.write_all(&response).ok()?;
                    Some(writer.bytes_written())
                })
                .unwrap_or(0);
            // The answer is in the guest's buffer already; a fenced one is handed back when its fence
            // is done, which is what tells the guest the fence signalled.
            match (fence.flatten(), &mut *backend) {
                (Some(fence), Backend::Accelerated(gpu)) => {
                    gpu.pending.hold(fence, (head, written as u32))
                }
                _ => {
                    queue.add_used(mem, head, written as u32)?;
                    used_any = true;
                }
            }
            // A flushed frame is read back after the guest has its answer, so that it can start
            // the next frame while the host copies this one.
            if let Backend::Accelerated(gpu) = backend
                && gpu.gpu.frame_due()
            {
                if used_any {
                    irq.signal_used();
                }
                gpu.present_due();
            }
        }
        // Re-arming reports commands that slipped in after the last pop; go round again for those.
        if !queue.enable_notification(mem)? {
            return Ok(used_any);
        }
    }
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
    /// The renderer finished the work before this fence.
    Fence(Fence),
    Stop,
}

/// How the device is accelerated, if it is.
pub struct Acceleration {
    pub renderer: Arc<RendererFactory>,
    pub mapper: Arc<dyn BlobMapper>,
    /// The host-visible window blobs are mapped into (shmid 1).
    pub window: ShmRegion,
    /// Host page size: blob mappings are whole pages.
    pub page: u64,
    /// Capability sets the renderer offers; the driver reads the count before the device starts.
    pub capsets: u32,
}

/// What the worker needs to start a renderer: how to make one, where blobs go, the window's size
/// and the host page size.
type Rendering = (Arc<RendererFactory>, Arc<dyn BlobMapper>, u64, u64);

struct Served {
    mode: DisplayMode,
    sink: Option<Arc<dyn FrameSink>>,
    acceleration: Option<Rendering>,
}

fn serve(
    served: Served,
    mut control: Queue,
    mut cursor: Queue,
    mem: GuestMemoryMmap,
    irq: Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
    wakes: Sender<Wake>,
) {
    crate::interactive_thread();
    let Served {
        mode,
        sink,
        acceleration,
    } = served;
    let plain = || {
        Backend::Plain(Display {
            mode,
            sink: sink.clone(),
            resources: HashMap::new(),
            scanout: None,
            cropped: Vec::new(),
        })
    };
    // The renderer is made here, on the thread that uses it.
    let mut backend = match acceleration {
        Some((factory, mapper, window, page)) => {
            let fences = std::sync::Mutex::new(wakes);
            let done = Box::new(move |fence| {
                let wakes = fences
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = wakes.send(Wake::Fence(fence));
            });
            match factory(done) {
                Ok(renderer) => Backend::Accelerated(Box::new(Accelerated {
                    gpu: Gpu3d::new(renderer, mapper, window, page),
                    sink: sink.clone(),
                    pending: PendingFences::default(),
                })),
                Err(why) => {
                    log::error!("{}", copy::gpu_renderer_failed(&why));
                    plain()
                }
            }
        }
        None => plain(),
    };
    loop {
        let used = match wake.recv() {
            Ok(Wake::Queue) => drain_control(&mut control, &mem, &mut backend, mode, &irq)
                .and_then(|a| drain_cursor(&mut cursor, &mem).map(|b| a | b)),
            Ok(Wake::Fence(done)) => {
                let Backend::Accelerated(gpu) = &mut backend else {
                    continue;
                };
                let ready = gpu.pending.release(done);
                let any = !ready.is_empty();
                // Answer every released fence even if one fails; report the first error.
                ready
                    .into_iter()
                    .map(|(head, written)| queue_used(&mut control, &mem, head, written))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .collect::<Result<(), _>>()
                    .map(|()| any)
            }
            Ok(Wake::Stop) | Err(_) => return,
        };
        match used {
            Ok(true) => irq.signal_used(),
            Ok(false) => {}
            Err(err) => log::warn!("{}", copy::virtio_queue_error(&err)),
        }
    }
}

fn queue_used(
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
    head: u16,
    written: u32,
) -> Result<(), virtio_queue::Error> {
    queue.add_used(mem, head, written)
}

pub struct Gpu {
    mode: DisplayMode,
    sink: Option<Arc<dyn FrameSink>>,
    acceleration: Option<Acceleration>,
    worker: Option<(Sender<Wake>, JoinHandle<()>)>,
}

impl Gpu {
    /// A display that acknowledges everything and keeps nothing.
    pub fn headless(mode: DisplayMode) -> Self {
        Self {
            mode,
            sink: None,
            acceleration: None,
            worker: None,
        }
    }

    /// A display whose frames go to `sink`.
    pub fn with_sink(mode: DisplayMode, sink: Arc<dyn FrameSink>) -> Self {
        Self {
            mode,
            sink: Some(sink),
            acceleration: None,
            worker: None,
        }
    }

    /// The same display, with its 3D commands done by a host renderer.
    pub fn accelerated(mut self, acceleration: Acceleration) -> Self {
        self.acceleration = Some(acceleration);
        self
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
        let accelerated = [
            VIRTIO_GPU_F_VIRGL,
            VIRTIO_GPU_F_RESOURCE_BLOB,
            VIRTIO_GPU_F_CONTEXT_INIT,
        ];
        let edid = self.mode.edid().map_or(0, |_| 1u64 << VIRTIO_GPU_F_EDID);
        edid | match self.acceleration {
            Some(_) => accelerated.iter().map(|bit| 1u64 << bit).sum(),
            None => 0,
        }
    }

    fn shm_regions(&self) -> &[ShmRegion] {
        self.acceleration.as_ref().map_or(&[], |acceleration| {
            std::slice::from_ref(&acceleration.window)
        })
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        config[CONFIG_NUM_SCANOUTS_AT..CONFIG_NUM_SCANOUTS_AT + 4]
            .copy_from_slice(&SCANOUTS.to_le_bytes());
        let capsets = self.acceleration.as_ref().map_or(0, |a| a.capsets);
        config[CONFIG_NUM_CAPSETS_AT..CONFIG_NUM_CAPSETS_AT + 4]
            .copy_from_slice(&capsets.to_le_bytes());
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
        // The driver may have turned 3D down; then the device is the plain one.
        let wanted = 1u64 << VIRTIO_GPU_F_VIRGL;
        let acceleration = self
            .acceleration
            .as_ref()
            .filter(|_| ctx.features & wanted != 0)
            .map(|a| {
                (
                    Arc::clone(&a.renderer),
                    Arc::clone(&a.mapper),
                    a.window.len,
                    a.page,
                )
            });
        let served = Served {
            mode: self.mode,
            sink: self.sink.clone(),
            acceleration,
        };
        let fences = wake.clone();
        let thread = thread::Builder::new()
            .name(copy::GPU_WORKER_NAME.into())
            .spawn(move || serve(served, control, cursor, ctx.mem, ctx.irq, wakes, fences))?;
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
            let asked = std::time::Instant::now();
            while !thread.is_finished() && asked.elapsed() < WORKER_STOP_LIMIT {
                thread::sleep(WORKER_STOP_POLL);
            }
            if !thread.is_finished() {
                // Left running: the renderer is stuck in its teardown and would block for good.
                // shortcut: a guest reboot after this starts a new renderer beside the stuck one;
                // restart the process instead if that ever shows up.
                log::warn!("{}", copy::GPU_WORKER_STUCK);
            } else if thread.join().is_err() {
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
    use std::sync::Mutex;

    use virtio_bindings::virtio_gpu::{
        virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SUBMIT_3D as CMD_SUBMIT_3D,
        virtio_gpu_formats_VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM as FORMAT_BGRX,
    };
    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;

    use super::*;

    const MODE: DisplayMode = DisplayMode {
        width: 1280,
        height: 720,
        refresh_hz: 120,
    };
    const MEM_SIZE: usize = 2 << 20;
    const REQUEST_AT: u64 = 0x10_0000;
    const RESPONSE_AT: u64 = 0x11_0000;
    /// Where the guest keeps its copy of the test image, in two pieces.
    const BACKING_A: u64 = 0x12_0000;
    const BACKING_B: u64 = 0x13_0000;
    const QUEUE_LEN: u16 = 16;
    const DISPLAY_INFO_LEN: usize = HEADER_LEN + DISPLAY_ENTRY_LEN * 16;
    const IMAGE: u32 = 7;
    const WIDTH: u32 = 4;
    const HEIGHT: u32 = 3;
    const IMAGE_BYTES: usize = (WIDTH * HEIGHT) as usize * BYTES_PER_PIXEL;

    fn header(kind: u32, flags: u32, fence: u64, ctx: u32) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&kind.to_le_bytes());
        header[4..8].copy_from_slice(&flags.to_le_bytes());
        header[8..16].copy_from_slice(&fence.to_le_bytes());
        header[16..20].copy_from_slice(&ctx.to_le_bytes());
        header
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// Width, height, format and pixels of one presented frame.
    type Presented = (u32, u32, u32, Vec<u8>);

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Presented>>);

    impl FrameSink for Recorder {
        fn present(&self, frame: &Frame<'_>) {
            self.0.lock().unwrap().push((
                frame.width,
                frame.height,
                frame.format,
                frame.pixels.to_vec(),
            ));
        }
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap()
    }

    fn display(sink: Option<Arc<Recorder>>) -> Display {
        Display {
            mode: MODE,
            sink: sink.map(|sink| sink as Arc<dyn FrameSink>),
            resources: HashMap::new(),
            scanout: None,
            cropped: Vec::new(),
        }
    }

    /// Runs one command and returns the response's type.
    fn run(display: &mut Display, mem: &GuestMemoryMmap, kind: u32, body: &[u8]) -> u32 {
        word(
            &display.handle(mem, &header(kind, 0, 0, 0), &mut &body[..]),
            0,
        )
    }

    /// The guest's image: pixel `n` is the four bytes `n, n, n, n`, split over two backing pieces.
    fn guest_image(mem: &GuestMemoryMmap) -> Vec<u8> {
        let image: Vec<u8> = (0..WIDTH * HEIGHT)
            .flat_map(|pixel| [pixel as u8; BYTES_PER_PIXEL])
            .collect();
        let split = 20;
        mem.write_slice(&image[..split], GuestAddress(BACKING_A))
            .unwrap();
        mem.write_slice(&image[split..], GuestAddress(BACKING_B))
            .unwrap();
        image
    }

    fn attach(display: &mut Display, mem: &GuestMemoryMmap) -> u32 {
        let mut body = words(&[IMAGE, 2]);
        body.extend(BACKING_A.to_le_bytes());
        body.extend(words(&[20, 0]));
        body.extend(BACKING_B.to_le_bytes());
        body.extend(words(&[IMAGE_BYTES as u32 - 20, 0]));
        run(display, mem, CMD_ATTACH_BACKING, &body)
    }

    fn transfer(display: &mut Display, mem: &GuestMemoryMmap, rect: [u32; 4], offset: u64) -> u32 {
        let mut body = words(&rect);
        body.extend(offset.to_le_bytes());
        body.extend(words(&[IMAGE, 0]));
        run(display, mem, CMD_TRANSFER_2D, &body)
    }

    #[test]
    fn display_info_reports_one_enabled_scanout_of_the_configured_size() {
        let response = display(None).handle(
            &memory(),
            &header(CMD_GET_DISPLAY_INFO, 0, 0, 0),
            &mut &[][..],
        );
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
    fn headless_display_acknowledges_two_d_commands_without_reading_them() {
        let mut headless = display(None);
        let mem = memory();
        for kind in CMD_FIRST_2D..=CMD_LAST_2D {
            assert_eq!(
                run(&mut headless, &mem, kind, &[]),
                RESP_OK_NODATA,
                "command {kind:#x}"
            );
        }
        assert!(headless.resources.is_empty());
        for kind in [0, CMD_SUBMIT_3D, CMD_LAST_2D + 1, u32::MAX] {
            assert_eq!(
                run(&mut headless, &mem, kind, &[]),
                RESP_ERR_UNSPEC,
                "command {kind:#x}"
            );
        }
    }

    #[test]
    fn fenced_command_gets_its_fence_back_and_an_unfenced_one_gets_zeros() {
        let request = header(CMD_FLUSH, VIRTIO_GPU_FLAG_FENCE | 0x80, 0xfeed_beef, 7);
        let fenced = respond(MODE, &request, RESP_OK_NODATA);
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
        let plain = respond(MODE, &header(CMD_FLUSH, 0, 0xfeed_beef, 7), RESP_OK_NODATA);
        assert!(plain[4..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn flushed_scanout_reaches_the_sink_with_the_guests_pixels() {
        let sink = Arc::new(Recorder::default());
        let mut display = display(Some(sink.clone()));
        let mem = memory();
        let image = guest_image(&mem);

        assert_eq!(
            run(
                &mut display,
                &mem,
                CMD_CREATE_2D,
                &words(&[IMAGE, FORMAT_BGRX, WIDTH, HEIGHT])
            ),
            RESP_OK_NODATA
        );
        assert_eq!(attach(&mut display, &mem), RESP_OK_NODATA);
        assert_eq!(
            transfer(&mut display, &mem, [0, 0, WIDTH, HEIGHT], 0),
            RESP_OK_NODATA
        );
        // Flushing before the resource is on the scanout shows nothing.
        let flush = words(&[0, 0, WIDTH, HEIGHT, IMAGE, 0]);
        assert_eq!(run(&mut display, &mem, CMD_FLUSH, &flush), RESP_OK_NODATA);
        assert!(sink.0.lock().unwrap().is_empty());

        assert_eq!(
            run(
                &mut display,
                &mem,
                CMD_SET_SCANOUT,
                &words(&[0, 0, WIDTH, HEIGHT, 0, IMAGE])
            ),
            RESP_OK_NODATA
        );
        assert_eq!(run(&mut display, &mem, CMD_FLUSH, &flush), RESP_OK_NODATA);
        assert_eq!(
            *sink.0.lock().unwrap(),
            [(WIDTH, HEIGHT, FORMAT_BGRX, image.clone())]
        );

        // A scanout that shows only part of the resource: the middle two columns of the lower rows.
        let part = [1, 1, 2, HEIGHT - 1];
        assert_eq!(
            run(
                &mut display,
                &mem,
                CMD_SET_SCANOUT,
                &words(&[part[0], part[1], part[2], part[3], 0, IMAGE])
            ),
            RESP_OK_NODATA
        );
        assert_eq!(run(&mut display, &mem, CMD_FLUSH, &flush), RESP_OK_NODATA);
        let stride = WIDTH as usize * BYTES_PER_PIXEL;
        let expected: Vec<u8> = image
            .chunks_exact(stride)
            .skip(1)
            .flat_map(|row| row[BYTES_PER_PIXEL..3 * BYTES_PER_PIXEL].to_vec())
            .collect();
        assert_eq!(
            sink.0.lock().unwrap().last().unwrap(),
            &(2, HEIGHT - 1, FORMAT_BGRX, expected)
        );

        // A shown part that reaches outside the resource, or has no area, is refused.
        for rect in [
            [1, 0, WIDTH, HEIGHT],
            [0, 0, WIDTH, HEIGHT + 1],
            [0, 0, 0, HEIGHT],
        ] {
            assert_eq!(
                run(
                    &mut display,
                    &mem,
                    CMD_SET_SCANOUT,
                    &words(&[rect[0], rect[1], rect[2], rect[3], 0, IMAGE])
                ),
                RESP_ERR_INVALID_PARAMETER
            );
        }
    }

    #[test]
    fn partial_transfer_updates_only_its_rectangle() {
        let sink = Arc::new(Recorder::default());
        let mut display = display(Some(sink.clone()));
        let mem = memory();
        let image = guest_image(&mem);
        run(
            &mut display,
            &mem,
            CMD_CREATE_2D,
            &words(&[IMAGE, FORMAT_BGRX, WIDTH, HEIGHT]),
        );
        attach(&mut display, &mem);
        // A 2x1 rectangle at (1, 2): its first pixel is number 2 * 4 + 1 = 9 in the image.
        let offset = (9 * BYTES_PER_PIXEL) as u64;
        assert_eq!(
            transfer(&mut display, &mem, [1, 2, 2, 1], offset),
            RESP_OK_NODATA
        );
        let pixels = &display.resources[&IMAGE].pixels;
        let start = 9 * BYTES_PER_PIXEL;
        assert_eq!(&pixels[start..start + 8], &image[start..start + 8]);
        assert!(
            pixels[..start]
                .iter()
                .chain(&pixels[start + 8..])
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn recreating_a_resource_is_refused_so_a_flush_cannot_outrun_the_scanout() {
        let mut display = display(Some(Arc::new(Recorder::default())));
        let mem = memory();
        let create = |display: &mut Display, size| {
            run(
                display,
                &mem,
                CMD_CREATE_2D,
                &words(&[1, FORMAT_BGRX, size, size]),
            )
        };
        assert_eq!(create(&mut display, 64), RESP_OK_NODATA);
        run(
            &mut display,
            &mem,
            CMD_SET_SCANOUT,
            &words(&[0, 0, 64, 64, 0, 1]),
        );
        assert_eq!(create(&mut display, 16), RESP_ERR_INVALID_RESOURCE_ID);
        let flush = words(&[0, 0, 64, 64, 1, 0]);
        assert_eq!(run(&mut display, &mem, CMD_FLUSH, &flush), RESP_OK_NODATA);
    }

    #[test]
    fn resources_together_are_capped_by_memory() {
        let mut display = display(Some(Arc::new(Recorder::default())));
        let mem = memory();
        let biggest = MAX_DIMENSION * MAX_DIMENSION * BYTES_PER_PIXEL as u32;
        let fit = MAX_RESOURCE_BYTES / biggest as usize;
        for id in 1..=fit as u32 {
            let body = words(&[id, FORMAT_BGRX, MAX_DIMENSION, MAX_DIMENSION]);
            assert_eq!(
                run(&mut display, &mem, CMD_CREATE_2D, &body),
                RESP_OK_NODATA
            );
        }
        let body = words(&[fit as u32 + 1, FORMAT_BGRX, MAX_DIMENSION, MAX_DIMENSION]);
        assert_eq!(
            run(&mut display, &mem, CMD_CREATE_2D, &body),
            RESP_ERR_OUT_OF_MEMORY
        );
    }

    #[test]
    fn commands_that_do_not_add_up_are_refused() {
        let mut display = display(Some(Arc::new(Recorder::default())));
        let mem = memory();
        let create = |display: &mut Display, id, width, height| {
            run(
                display,
                &mem,
                CMD_CREATE_2D,
                &words(&[id, FORMAT_BGRX, width, height]),
            )
        };
        assert_eq!(
            create(&mut display, 0, 4, 4),
            RESP_ERR_INVALID_PARAMETER,
            "resource id zero"
        );
        assert_eq!(
            create(&mut display, 1, 0, 4),
            RESP_ERR_INVALID_PARAMETER,
            "empty"
        );
        assert_eq!(
            create(&mut display, 1, MAX_DIMENSION + 1, 4),
            RESP_ERR_INVALID_PARAMETER,
            "too wide"
        );
        assert_eq!(
            run(&mut display, &mem, CMD_CREATE_2D, &words(&[1, FORMAT_BGRX])),
            RESP_ERR_INVALID_PARAMETER,
            "short body"
        );

        assert_eq!(create(&mut display, IMAGE, WIDTH, HEIGHT), RESP_OK_NODATA);
        assert_eq!(
            transfer(&mut display, &mem, [0, 0, WIDTH, HEIGHT], 0),
            RESP_ERR_INVALID_PARAMETER,
            "no backing yet"
        );
        guest_image(&mem);
        attach(&mut display, &mem);
        assert_eq!(
            transfer(&mut display, &mem, [1, 0, WIDTH, HEIGHT], 0),
            RESP_ERR_INVALID_PARAMETER,
            "off the right edge"
        );
        assert_eq!(
            transfer(&mut display, &mem, [u32::MAX, 0, 2, 1], 0),
            RESP_ERR_INVALID_PARAMETER,
            "overflowing x"
        );
        assert_eq!(
            transfer(&mut display, &mem, [0, 0, WIDTH, HEIGHT], 4),
            RESP_ERR_INVALID_PARAMETER,
            "past the backing"
        );
        assert_eq!(
            transfer(&mut display, &mem, [0, 0, WIDTH, HEIGHT], u64::MAX),
            RESP_ERR_INVALID_PARAMETER
        );

        assert_eq!(
            run(&mut display, &mem, CMD_FLUSH, &words(&[0, 0, 1, 1, 99, 0])),
            RESP_ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            run(
                &mut display,
                &mem,
                CMD_SET_SCANOUT,
                &words(&[0, 0, 1, 1, 0, 99])
            ),
            RESP_ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            run(
                &mut display,
                &mem,
                CMD_SET_SCANOUT,
                &words(&[0, 0, 1, 1, 1, IMAGE])
            ),
            RESP_ERR_INVALID_SCANOUT_ID
        );
        assert_eq!(
            run(
                &mut display,
                &mem,
                CMD_ATTACH_BACKING,
                &words(&[IMAGE, MAX_BACKING_ENTRIES + 1])
            ),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            run(&mut display, &mem, CMD_UNREF, &words(&[99, 0])),
            RESP_ERR_INVALID_RESOURCE_ID
        );
    }

    #[test]
    fn backing_outside_guest_memory_fails_the_transfer() {
        let mut display = display(Some(Arc::new(Recorder::default())));
        let mem = memory();
        run(
            &mut display,
            &mem,
            CMD_CREATE_2D,
            &words(&[IMAGE, FORMAT_BGRX, WIDTH, HEIGHT]),
        );
        let mut body = words(&[IMAGE, 1]);
        body.extend((MEM_SIZE as u64).to_le_bytes());
        body.extend(words(&[IMAGE_BYTES as u32, 0]));
        assert_eq!(
            run(&mut display, &mem, CMD_ATTACH_BACKING, &body),
            RESP_OK_NODATA
        );
        assert_eq!(
            transfer(&mut display, &mem, [0, 0, WIDTH, HEIGHT], 0),
            RESP_ERR_INVALID_PARAMETER
        );
    }

    #[test]
    fn unreferencing_the_shown_resource_turns_the_scanout_off_and_resources_are_capped() {
        let mut display = display(Some(Arc::new(Recorder::default())));
        let mem = memory();
        run(
            &mut display,
            &mem,
            CMD_CREATE_2D,
            &words(&[IMAGE, FORMAT_BGRX, WIDTH, HEIGHT]),
        );
        run(
            &mut display,
            &mem,
            CMD_SET_SCANOUT,
            &words(&[0, 0, WIDTH, HEIGHT, 0, IMAGE]),
        );
        assert_eq!(display.scanout.map(|(id, _)| id), Some(IMAGE));
        assert_eq!(
            run(&mut display, &mem, CMD_UNREF, &words(&[IMAGE, 0])),
            RESP_OK_NODATA
        );
        assert!(display.scanout.is_none());

        for id in 1..=MAX_RESOURCES as u32 {
            assert_eq!(
                run(
                    &mut display,
                    &mem,
                    CMD_CREATE_2D,
                    &words(&[id, FORMAT_BGRX, 1, 1])
                ),
                RESP_OK_NODATA
            );
        }
        let one_more = words(&[MAX_RESOURCES as u32 + 1, FORMAT_BGRX, 1, 1]);
        assert_eq!(
            run(&mut display, &mem, CMD_CREATE_2D, &one_more),
            RESP_ERR_OUT_OF_MEMORY
        );
    }

    #[test]
    fn control_queue_answers_in_the_guests_buffer_and_survives_bad_chains() {
        let mem = memory();
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
        let mut display = Backend::Plain(display(None));
        let irq = VirtioInterrupt::new(Arc::new(crate::virtio::testing::LineLog::default()));

        assert!(drain_control(&mut queue, &mem, &mut display, MODE, &irq).unwrap());
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
            respond(
                MODE,
                &header(CMD_GET_DISPLAY_INFO, 0, 0, 0),
                RESP_OK_DISPLAY_INFO
            )
        );
        assert!(
            !drain_control(&mut queue, &mem, &mut display, MODE, &irq).unwrap(),
            "nothing left"
        );
    }

    #[test]
    fn edid_describes_the_configured_mode() {
        let mut body: &[u8] = &0u32.to_le_bytes();
        let response = display(None).handle(&memory(), &header(CMD_GET_EDID, 0, 0, 0), &mut body);
        assert_eq!(word(&response, 0), RESP_OK_EDID);
        let size = word(&response, HEADER_LEN) as usize;
        assert_eq!(size, super::super::edid::EDID_LEN);
        let edid = &response[HEADER_LEN + 8..HEADER_LEN + 8 + size];
        assert_eq!(edid, MODE.edid().expect("fits"));
        assert_eq!(response.len(), HEADER_LEN + 8 + EDID_ROOM);

        let mut wrong: &[u8] = &1u32.to_le_bytes();
        let response = display(None).handle(&memory(), &header(CMD_GET_EDID, 0, 0, 0), &mut wrong);
        assert_eq!(word(&response, 0), RESP_ERR_INVALID_SCANOUT_ID);
    }

    #[test]
    fn edid_is_offered_only_when_the_mode_fits_one() {
        assert_ne!(Gpu::headless(MODE).features() & (1 << VIRTIO_GPU_F_EDID), 0);
        let too_fast = DisplayMode {
            refresh_hz: 100_000,
            ..MODE
        };
        assert_eq!(
            Gpu::headless(too_fast).features() & (1 << VIRTIO_GPU_F_EDID),
            0
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
