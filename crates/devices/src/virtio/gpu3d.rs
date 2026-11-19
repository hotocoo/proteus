//! virtio-gpu's 3D commands, handed to a host renderer (gfxstream, behind the `Renderer` trait).
//!
//! The device decodes and checks what the guest sent; the renderer does the work. Memory the
//! renderer allocates for a mappable blob joins the guest's address space through a `BlobMapper`,
//! inside the device's host-visible shared-memory window, without being copied.
//!
//! A fenced command is not answered when it is decoded but when the renderer says the work it
//! stands for is done: the guest treats the answer as the fence signalling.

use std::collections::HashMap;
use std::io::Read;
use std::ptr::NonNull;

use virtio_bindings::virtio_gpu::{
    VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE as CMD_CTX_ATTACH,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_CREATE as CMD_CTX_CREATE,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_DESTROY as CMD_CTX_DESTROY,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE as CMD_CTX_DETACH,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_CAPSET as CMD_GET_CAPSET,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_GET_CAPSET_INFO as CMD_GET_CAPSET_INFO,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING as CMD_ATTACH_BACKING,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_2D as CMD_CREATE_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_3D as CMD_CREATE_3D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB as CMD_CREATE_BLOB,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING as CMD_DETACH_BACKING,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_FLUSH as CMD_FLUSH,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB as CMD_MAP_BLOB,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB as CMD_UNMAP_BLOB,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_RESOURCE_UNREF as CMD_UNREF,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SET_SCANOUT as CMD_SET_SCANOUT,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SET_SCANOUT_BLOB as CMD_SET_SCANOUT_BLOB,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_SUBMIT_3D as CMD_SUBMIT_3D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D as CMD_TRANSFER_FROM_3D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D as CMD_TRANSFER_2D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D as CMD_TRANSFER_TO_3D,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER as RESP_ERR_INVALID_PARAMETER,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID as RESP_ERR_INVALID_RESOURCE_ID,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID as RESP_ERR_INVALID_SCANOUT_ID,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_ERR_UNSPEC as RESP_ERR_UNSPEC,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_CAPSET as RESP_OK_CAPSET,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_CAPSET_INFO as RESP_OK_CAPSET_INFO,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_MAP_INFO as RESP_OK_MAP_INFO,
    virtio_gpu_ctrl_type_VIRTIO_GPU_RESP_OK_NODATA as RESP_OK_NODATA,
};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::copy;

/// Gallium's 2D texture target and render-target bind flag: what a 2D resource is to a 3D renderer.
const PIPE_TEXTURE_2D: u32 = 2;
const PIPE_BIND_RENDER_TARGET: u32 = 1 << 1;
/// Limits on what a guest can make the device do in one command.
const MAX_BACKING_ENTRIES: u32 = 16_384;
const MAX_SUBMIT: u32 = 16 << 20;
const MAX_IN_FENCES: u32 = 64;
const DEBUG_NAME_LEN: usize = 64;
const SCANOUTS: u32 = 1;
/// Largest scanout side the device accepts, as in the 2D path.
const MAX_DIMENSION: u32 = 8192;
/// Bytes per pixel of every format a scanout can have.
const BYTES_PER_PIXEL: u32 = 4;

/// The fence timeline a fenced command belongs to: its context and ring when the guest asked for
/// per-ring fences, one global timeline otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Timeline {
    pub ctx: u32,
    pub ring: Option<u8>,
}

/// A fence, as the guest asked for it and as the renderer reports it done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    pub id: u64,
    pub timeline: Timeline,
}

impl Fence {
    /// The fence a command's header asks for, if it asks for one.
    pub fn of(header: &Header) -> Option<Self> {
        if header.flags & VIRTIO_GPU_FLAG_FENCE == 0 {
            return None;
        }
        let ring = (header.flags & VIRTIO_GPU_FLAG_INFO_RING_IDX != 0).then_some(header.ring);
        let timeline = match ring {
            Some(ring) => Timeline {
                ctx: header.ctx,
                ring: Some(ring),
            },
            None => Timeline { ctx: 0, ring: None },
        };
        Some(Self {
            id: header.fence,
            timeline,
        })
    }
}

/// Where the renderer reports finished fences. Called on any thread.
pub type FenceDone = Box<dyn Fn(Fence) + Send + Sync>;

/// The fields of a command header the 3D path uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Header {
    pub kind: u32,
    pub flags: u32,
    pub fence: u64,
    pub ctx: u32,
    pub ring: u8,
}

/// `VIRTIO_GPU_CMD_RESOURCE_CREATE_3D`'s parameters, which are Gallium's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Create3d {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

/// A transfer between a resource and its guest backing (or, reading, a host buffer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transfer {
    pub ctx: u32,
    pub resource: u32,
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
    pub offset: u64,
}

/// `VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB`'s parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Blob {
    pub ctx: u32,
    pub resource: u32,
    pub mem: u32,
    pub flags: u32,
    pub id: u64,
    pub size: u64,
}

/// A run of guest memory as the host sees it: address in this process, and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRange {
    pub addr: usize,
    pub len: usize,
}

/// Host memory a renderer handed out for a blob, to map into the guest.
#[derive(Clone, Copy, Debug)]
pub struct Mapping {
    pub host: NonNull<u8>,
    pub len: u64,
    /// One of `VIRTIO_GPU_MAP_CACHE_*`: how the guest should map it.
    pub map_info: u32,
}

/// A host renderer. Errors are text for the log; the guest gets a generic error answer. Renderers
/// need not be `Send` (gfxstream's is not), so the device makes its one on its worker thread, through
/// a `RendererFactory`.
pub trait Renderer {
    /// The capability set at `index`: id, highest version, size in bytes.
    fn capset_info(&self, index: u32) -> Option<(u32, u32, u32)>;
    fn capset(&self, id: u32, version: u32) -> Result<Vec<u8>, String>;
    fn context_create(&mut self, ctx: u32, init: u32, name: &[u8]) -> Result<(), String>;
    fn context_destroy(&mut self, ctx: u32) -> Result<(), String>;
    fn context_attach(&mut self, ctx: u32, resource: u32) -> Result<(), String>;
    fn context_detach(&mut self, ctx: u32, resource: u32) -> Result<(), String>;
    fn resource_create_3d(&mut self, resource: u32, create: Create3d) -> Result<(), String>;
    fn attach_backing(&mut self, resource: u32, backing: Vec<HostRange>) -> Result<(), String>;
    fn detach_backing(&mut self, resource: u32) -> Result<(), String>;
    fn resource_unref(&mut self, resource: u32) -> Result<(), String>;
    fn transfer_write(&mut self, transfer: Transfer) -> Result<(), String>;
    /// Reads into the resource's guest backing, or into `into` when given.
    fn transfer_read(&mut self, transfer: Transfer, into: Option<&mut [u8]>) -> Result<(), String>;
    fn submit(&mut self, ctx: u32, commands: &mut [u8], fences: &[u64]) -> Result<(), String>;
    fn create_blob(&mut self, blob: Blob, backing: Option<Vec<HostRange>>) -> Result<(), String>;
    fn map_blob(&mut self, resource: u32) -> Result<Mapping, String>;
    fn unmap_blob(&mut self, resource: u32) -> Result<(), String>;
    /// Reports `fence` through `FenceDone` once the work submitted before it is done.
    fn create_fence(&mut self, fence: Fence) -> Result<(), String>;
}

/// Makes the renderer, on the thread that will use it, given where to report finished fences.
pub type RendererFactory =
    Box<dyn Fn(FenceDone) -> Result<Box<dyn Renderer>, String> + Send + Sync>;

/// Puts renderer memory into the guest's shared-memory window, `offset` bytes into it.
pub trait BlobMapper: Send + Sync {
    /// # Safety
    /// `host..host + len` stays valid and unmoved until `unmap` with the same offset and length.
    unsafe fn map(&self, offset: u64, host: NonNull<u8>, len: u64) -> Result<(), String>;
    fn unmap(&self, offset: u64, len: u64) -> Result<(), String>;
}

/// What a 3D command answers with, besides OK or an error.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    NoData,
    CapsetInfo {
        id: u32,
        version: u32,
        size: u32,
    },
    Capset(Vec<u8>),
    MapInfo(u32),
    /// The scanout's resource flushed: the shown rectangle's pixels, to present.
    Frame {
        width: u32,
        height: u32,
        format: u32,
        pixels: Vec<u8>,
    },
}

impl Answer {
    /// The response type and body for the guest.
    pub fn response(&self) -> (u32, Vec<u8>) {
        let words = |values: &[u32]| values.iter().flat_map(|v| v.to_le_bytes()).collect();
        match self {
            Self::NoData | Self::Frame { .. } => (RESP_OK_NODATA, Vec::new()),
            Self::CapsetInfo { id, version, size } => {
                (RESP_OK_CAPSET_INFO, words(&[*id, *version, *size, 0]))
            }
            Self::Capset(bytes) => (RESP_OK_CAPSET, bytes.clone()),
            Self::MapInfo(info) => (RESP_OK_MAP_INFO, words(&[*info, 0])),
        }
    }
}

struct Reader<'a, R: Read>(&'a mut R);

impl<R: Read> Reader<'_, R> {
    fn u32(&mut self) -> Result<u32, u32> {
        let mut raw = [0u8; 4];
        self.0
            .read_exact(&mut raw)
            .map_err(|_| RESP_ERR_INVALID_PARAMETER)?;
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, u32> {
        let mut raw = [0u8; 8];
        self.0
            .read_exact(&mut raw)
            .map_err(|_| RESP_ERR_INVALID_PARAMETER)?;
        Ok(u64::from_le_bytes(raw))
    }

    fn bytes(&mut self, len: usize) -> Result<Vec<u8>, u32> {
        let mut out = vec![0; len];
        self.0
            .read_exact(&mut out)
            .map_err(|_| RESP_ERR_INVALID_PARAMETER)?;
        Ok(out)
    }
}

/// A shown resource: which one, and the rectangle of it the scanout shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scanout {
    resource: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    format: u32,
}

/// The 3D side of the display's state.
pub struct Gpu3d {
    renderer: Box<dyn Renderer>,
    mapper: std::sync::Arc<dyn BlobMapper>,
    /// Size of the shared-memory window blobs are mapped into.
    window: u64,
    /// Host page size: blob mappings must be whole pages.
    page: u64,
    /// Blobs mapped into the window: resource, offset, length.
    mapped: HashMap<u32, (u64, u64)>,
    /// Formats of 2D-created resources, for presenting them.
    formats: HashMap<u32, (u32, u32, u32)>,
    scanout: Option<Scanout>,
}

fn renderer_failed(what: &str) -> impl Fn(String) -> u32 + '_ {
    move |why| {
        log::debug!("{}", copy::gpu_renderer_refused(what, &why));
        RESP_ERR_UNSPEC
    }
}

/// The guest's memory entries, as host ranges. Every entry has to lie in one region of guest memory.
fn host_ranges(mem: &GuestMemoryMmap, entries: &[(u64, u32)]) -> Result<Vec<HostRange>, u32> {
    entries
        .iter()
        .map(|&(addr, len)| {
            // Both ends have to be in guest memory, and contiguous on the host: one region.
            let start = GuestAddress(addr);
            let last = addr
                .checked_add(u64::from(len).saturating_sub(1))
                .map(GuestAddress)
                .ok_or(RESP_ERR_INVALID_PARAMETER)?;
            let first = mem
                .get_host_address(start)
                .map_err(|_| RESP_ERR_INVALID_PARAMETER)?;
            let end = mem
                .get_host_address(last)
                .map_err(|_| RESP_ERR_INVALID_PARAMETER)?;
            let span = (end as usize).wrapping_sub(first as usize);
            if len == 0 || span != len as usize - 1 {
                return Err(RESP_ERR_INVALID_PARAMETER);
            }
            Ok(HostRange {
                addr: first as usize,
                len: len as usize,
            })
        })
        .collect()
}

fn read_entries(body: &mut Reader<'_, impl Read>, count: u32) -> Result<Vec<(u64, u32)>, u32> {
    if count > MAX_BACKING_ENTRIES {
        return Err(RESP_ERR_INVALID_PARAMETER);
    }
    (0..count)
        .map(|_| {
            let (addr, len, _padding) = (body.u64()?, body.u32()?, body.u32()?);
            Ok((addr, len))
        })
        .collect()
}

impl Gpu3d {
    pub fn new(
        renderer: Box<dyn Renderer>,
        mapper: std::sync::Arc<dyn BlobMapper>,
        window: u64,
        page: u64,
    ) -> Self {
        Self {
            renderer,
            mapper,
            window,
            page,
            mapped: HashMap::new(),
            formats: HashMap::new(),
            scanout: None,
        }
    }

    /// How many capability sets the renderer has.
    pub fn capset_count(&self) -> u32 {
        (0..)
            .take_while(|i| self.renderer.capset_info(*i).is_some())
            .count() as u32
    }

    /// Asks the renderer to report `fence` when the work before it is done.
    pub fn fence(&mut self, fence: Fence) -> Result<(), String> {
        self.renderer.create_fence(fence)
    }

    /// Carries out one command. Returns the error response code if the guest got something wrong
    /// or the renderer refused.
    pub fn execute(
        &mut self,
        mem: &GuestMemoryMmap,
        header: &Header,
        body: &mut impl Read,
    ) -> Result<Answer, u32> {
        let mut body = Reader(body);
        let ctx = header.ctx;
        match header.kind {
            CMD_GET_CAPSET_INFO => {
                let index = body.u32()?;
                let (id, version, size) = self
                    .renderer
                    .capset_info(index)
                    .ok_or(RESP_ERR_INVALID_PARAMETER)?;
                Ok(Answer::CapsetInfo { id, version, size })
            }
            CMD_GET_CAPSET => {
                let (id, version) = (body.u32()?, body.u32()?);
                let caps = self.renderer.capset(id, version);
                Ok(Answer::Capset(caps.map_err(renderer_failed("capset"))?))
            }
            CMD_CTX_CREATE => {
                let (len, init) = (body.u32()?, body.u32()?);
                let name = body.bytes(DEBUG_NAME_LEN)?;
                let len = (len as usize).min(DEBUG_NAME_LEN);
                let created = self.renderer.context_create(ctx, init, &name[..len]);
                created.map_err(renderer_failed("context"))?;
                Ok(Answer::NoData)
            }
            CMD_CTX_DESTROY => {
                let destroyed = self.renderer.context_destroy(ctx);
                destroyed.map_err(renderer_failed("context"))?;
                Ok(Answer::NoData)
            }
            CMD_CTX_ATTACH | CMD_CTX_DETACH => {
                let resource = body.u32()?;
                let done = if header.kind == CMD_CTX_ATTACH {
                    self.renderer.context_attach(ctx, resource)
                } else {
                    self.renderer.context_detach(ctx, resource)
                };
                done.map_err(renderer_failed("context resource"))?;
                Ok(Answer::NoData)
            }
            CMD_CREATE_2D => {
                let (resource, format, width, height) =
                    (body.u32()?, body.u32()?, body.u32()?, body.u32()?);
                let create = Create3d {
                    target: PIPE_TEXTURE_2D,
                    format,
                    bind: PIPE_BIND_RENDER_TARGET,
                    width,
                    height,
                    depth: 1,
                    array_size: 1,
                    ..Create3d::default()
                };
                self.create_3d(resource, create)
            }
            CMD_CREATE_3D => {
                let resource = body.u32()?;
                let create = Create3d {
                    target: body.u32()?,
                    format: body.u32()?,
                    bind: body.u32()?,
                    width: body.u32()?,
                    height: body.u32()?,
                    depth: body.u32()?,
                    array_size: body.u32()?,
                    last_level: body.u32()?,
                    nr_samples: body.u32()?,
                    flags: body.u32()?,
                };
                self.create_3d(resource, create)
            }
            CMD_UNREF => {
                let resource = body.u32()?;
                if let Some((offset, len)) = self.mapped.remove(&resource) {
                    let _ = self.mapper.unmap(offset, len);
                }
                self.formats.remove(&resource);
                if self.scanout.is_some_and(|shown| shown.resource == resource) {
                    self.scanout = None;
                }
                let gone = self.renderer.resource_unref(resource);
                gone.map_err(renderer_failed("resource"))?;
                Ok(Answer::NoData)
            }
            CMD_ATTACH_BACKING => {
                let (resource, count) = (body.u32()?, body.u32()?);
                let entries = read_entries(&mut body, count)?;
                let backing = host_ranges(mem, &entries)?;
                let attached = self.renderer.attach_backing(resource, backing);
                attached.map_err(renderer_failed("backing"))?;
                Ok(Answer::NoData)
            }
            CMD_DETACH_BACKING => {
                let resource = body.u32()?;
                let detached = self.renderer.detach_backing(resource);
                detached.map_err(renderer_failed("backing"))?;
                Ok(Answer::NoData)
            }
            CMD_TRANSFER_2D => {
                let (x, y, width, height) = (body.u32()?, body.u32()?, body.u32()?, body.u32()?);
                let (offset, resource) = (body.u64()?, body.u32()?);
                let transfer = Transfer {
                    resource,
                    x,
                    y,
                    width,
                    height,
                    depth: 1,
                    offset,
                    ..Transfer::default()
                };
                let written = self.renderer.transfer_write(transfer);
                written.map_err(renderer_failed("transfer"))?;
                Ok(Answer::NoData)
            }
            CMD_TRANSFER_TO_3D | CMD_TRANSFER_FROM_3D => {
                let (x, y, z) = (body.u32()?, body.u32()?, body.u32()?);
                let (width, height, depth) = (body.u32()?, body.u32()?, body.u32()?);
                let (offset, resource) = (body.u64()?, body.u32()?);
                let (level, stride, layer_stride) = (body.u32()?, body.u32()?, body.u32()?);
                let transfer = Transfer {
                    ctx,
                    resource,
                    x,
                    y,
                    z,
                    width,
                    height,
                    depth,
                    level,
                    stride,
                    layer_stride,
                    offset,
                };
                let done = if header.kind == CMD_TRANSFER_TO_3D {
                    self.renderer.transfer_write(transfer)
                } else {
                    self.renderer.transfer_read(transfer, None)
                };
                done.map_err(renderer_failed("transfer"))?;
                Ok(Answer::NoData)
            }
            CMD_SUBMIT_3D => {
                // The commands, then the ids of fences they wait for (`num_in_fences`, which older
                // drivers leave as zero padding). The command's own fence is not among them.
                let (size, waits) = (body.u32()?, body.u32()?);
                if size > MAX_SUBMIT || waits > MAX_IN_FENCES {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                let mut commands = body.bytes(size as usize)?;
                let fences = (0..waits)
                    .map(|_| body.u64())
                    .collect::<Result<Vec<_>, _>>()?;
                let submitted = self.renderer.submit(ctx, &mut commands, &fences);
                submitted.map_err(renderer_failed("submit"))?;
                Ok(Answer::NoData)
            }
            CMD_CREATE_BLOB => {
                let (resource, mem_kind, flags, count) =
                    (body.u32()?, body.u32()?, body.u32()?, body.u32()?);
                let (id, size) = (body.u64()?, body.u64()?);
                let entries = read_entries(&mut body, count)?;
                let backing = if entries.is_empty() {
                    None
                } else {
                    Some(host_ranges(mem, &entries)?)
                };
                let blob = Blob {
                    ctx,
                    resource,
                    mem: mem_kind,
                    flags,
                    id,
                    size,
                };
                let created = self.renderer.create_blob(blob, backing);
                created.map_err(renderer_failed("blob"))?;
                Ok(Answer::NoData)
            }
            CMD_MAP_BLOB => {
                let (resource, _padding, offset) = (body.u32()?, body.u32()?, body.u64()?);
                self.map_blob(resource, offset)
            }
            CMD_UNMAP_BLOB => {
                let resource = body.u32()?;
                let (offset, len) = self
                    .mapped
                    .remove(&resource)
                    .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                self.mapper
                    .unmap(offset, len)
                    .map_err(renderer_failed("unmap"))?;
                let unmapped = self.renderer.unmap_blob(resource);
                unmapped.map_err(renderer_failed("unmap"))?;
                Ok(Answer::NoData)
            }
            CMD_SET_SCANOUT => {
                let (x, y, width, height) = (body.u32()?, body.u32()?, body.u32()?, body.u32()?);
                let (scanout, resource) = (body.u32()?, body.u32()?);
                let format = self
                    .formats
                    .get(&resource)
                    .map_or(0, |(format, ..)| *format);
                self.set_scanout(scanout, resource, [x, y, width, height], format)
            }
            CMD_SET_SCANOUT_BLOB => {
                let (x, y, width, height) = (body.u32()?, body.u32()?, body.u32()?, body.u32()?);
                let (scanout, resource) = (body.u32()?, body.u32()?);
                let (_blob_width, _blob_height, format) = (body.u32()?, body.u32()?, body.u32()?);
                self.set_scanout(scanout, resource, [x, y, width, height], format)
            }
            CMD_FLUSH => {
                let (_x, _y, _width, _height) =
                    (body.u32()?, body.u32()?, body.u32()?, body.u32()?);
                let resource = body.u32()?;
                self.flush(resource)
            }
            _ => Err(RESP_ERR_UNSPEC),
        }
    }

    fn create_3d(&mut self, resource: u32, create: Create3d) -> Result<Answer, u32> {
        if resource == 0 {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        let created = self.renderer.resource_create_3d(resource, create);
        created.map_err(renderer_failed("resource"))?;
        self.formats
            .insert(resource, (create.format, create.width, create.height));
        Ok(Answer::NoData)
    }

    fn map_blob(&mut self, resource: u32, offset: u64) -> Result<Answer, u32> {
        // A blob already in the window stays where it is; asking the renderer again, and undoing
        // that, would take its memory out from under the guest's mapping.
        if self.mapped.contains_key(&resource) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let mapping = self
            .renderer
            .map_blob(resource)
            .map_err(renderer_failed("map"))?;
        // The window is mapped in whole host pages; the guest has to keep to them. Nothing is rounded.
        let fits = offset
            .checked_add(mapping.len)
            .is_some_and(|end| end <= self.window);
        let aligned = offset.is_multiple_of(self.page)
            && mapping.len.is_multiple_of(self.page)
            && (mapping.host.as_ptr() as u64).is_multiple_of(self.page);
        if !fits || !aligned {
            log::warn!(
                "{}",
                copy::gpu_blob_misaligned(resource, offset, mapping.len)
            );
            let _ = self.renderer.unmap_blob(resource);
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        // SAFETY: the renderer keeps the mapping until `unmap_blob`, which is only called after
        // the mapper has taken it out of the guest again (UNMAP_BLOB, UNREF).
        let mapped = unsafe { self.mapper.map(offset, mapping.host, mapping.len) };
        if let Err(why) = mapped {
            log::warn!("{}", copy::gpu_renderer_refused("map", &why));
            let _ = self.renderer.unmap_blob(resource);
            return Err(RESP_ERR_UNSPEC);
        }
        self.mapped.insert(resource, (offset, mapping.len));
        Ok(Answer::MapInfo(mapping.map_info))
    }

    fn set_scanout(
        &mut self,
        scanout: u32,
        resource: u32,
        [x, y, width, height]: [u32; 4],
        format: u32,
    ) -> Result<Answer, u32> {
        if scanout >= SCANOUTS {
            return Err(RESP_ERR_INVALID_SCANOUT_ID);
        }
        // The rectangle sizes the read-back buffer of every flush, so it is held to what a display can be.
        let sane = (1..=MAX_DIMENSION).contains(&width) && (1..=MAX_DIMENSION).contains(&height);
        if resource != 0 && !sane {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        self.scanout = (resource != 0).then_some(Scanout {
            resource,
            x,
            y,
            width,
            height,
            format,
        });
        Ok(Answer::NoData)
    }

    /// The shown resource flushed: its pixels, read back from the renderer, to present.
    fn flush(&mut self, resource: u32) -> Result<Answer, u32> {
        let Some(shown) = self.scanout.filter(|shown| shown.resource == resource) else {
            return Ok(Answer::NoData);
        };
        let stride = shown.width.checked_mul(BYTES_PER_PIXEL);
        let len = stride.and_then(|stride| (stride as usize).checked_mul(shown.height as usize));
        let (Some(stride), Some(len)) = (stride, len) else {
            return Err(RESP_ERR_INVALID_PARAMETER);
        };
        let mut pixels = vec![0u8; len];
        let transfer = Transfer {
            resource,
            x: shown.x,
            y: shown.y,
            width: shown.width,
            height: shown.height,
            depth: 1,
            stride,
            ..Transfer::default()
        };
        let read = self.renderer.transfer_read(transfer, Some(&mut pixels));
        read.map_err(renderer_failed("read back"))?;
        Ok(Answer::Frame {
            width: shown.width,
            height: shown.height,
            format: shown.format,
            pixels,
        })
    }
}

impl Drop for Gpu3d {
    fn drop(&mut self) {
        // The guest must not keep pointing at renderer memory that goes away with the renderer.
        for (_, (offset, len)) in self.mapped.drain() {
            let _ = self.mapper.unmap(offset, len);
        }
    }
}

/// Fenced commands whose answers wait for their fence.
#[derive(Default)]
pub struct PendingFences<T> {
    waiting: Vec<(Fence, T)>,
}

impl<T> PendingFences<T> {
    pub fn hold(&mut self, fence: Fence, answer: T) {
        self.waiting.push((fence, answer));
    }

    /// Takes every held answer `done` releases: those on its timeline up to its id, in order.
    pub fn release(&mut self, done: Fence) -> Vec<T> {
        let (ready, still): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|(fence, _)| fence.timeline == done.timeline && fence.id <= done.id);
        self.waiting = still;
        ready.into_iter().map(|(_, answer)| answer).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    struct Fake {
        log: Arc<Log>,
        /// Page-aligned memory the fake hands out as its blob: offset into `blob`, and length.
        blob: Vec<u8>,
        at: usize,
        len: usize,
    }

    impl Fake {
        fn note(&self, what: String) -> Result<(), String> {
            self.log.0.lock().unwrap().push(what);
            Ok(())
        }
    }

    impl Renderer for Fake {
        fn capset_info(&self, index: u32) -> Option<(u32, u32, u32)> {
            (index == 0).then_some((3, 0, 4))
        }
        fn capset(&self, id: u32, _version: u32) -> Result<Vec<u8>, String> {
            if id == 3 {
                Ok(vec![1, 2, 3, 4])
            } else {
                Err("no such capset".into())
            }
        }
        fn context_create(&mut self, ctx: u32, init: u32, name: &[u8]) -> Result<(), String> {
            self.note(format!(
                "create {ctx} {init} {}",
                String::from_utf8_lossy(name)
            ))
        }
        fn context_destroy(&mut self, ctx: u32) -> Result<(), String> {
            self.note(format!("destroy {ctx}"))
        }
        fn context_attach(&mut self, ctx: u32, resource: u32) -> Result<(), String> {
            self.note(format!("attach {ctx} {resource}"))
        }
        fn context_detach(&mut self, ctx: u32, resource: u32) -> Result<(), String> {
            self.note(format!("detach {ctx} {resource}"))
        }
        fn resource_create_3d(&mut self, resource: u32, create: Create3d) -> Result<(), String> {
            self.note(format!(
                "res {resource} {}x{} target {}",
                create.width, create.height, create.target
            ))
        }
        fn attach_backing(&mut self, resource: u32, backing: Vec<HostRange>) -> Result<(), String> {
            self.note(format!("backing {resource} {}", backing.len()))
        }
        fn detach_backing(&mut self, resource: u32) -> Result<(), String> {
            self.note(format!("unbacking {resource}"))
        }
        fn resource_unref(&mut self, resource: u32) -> Result<(), String> {
            self.note(format!("unref {resource}"))
        }
        fn transfer_write(&mut self, t: Transfer) -> Result<(), String> {
            self.note(format!("write {} {}x{}", t.resource, t.width, t.height))
        }
        fn transfer_read(&mut self, t: Transfer, into: Option<&mut [u8]>) -> Result<(), String> {
            if let Some(into) = into {
                into.fill(0x7f);
            }
            self.note(format!("read {} {}x{}", t.resource, t.width, t.height))
        }
        fn submit(&mut self, ctx: u32, commands: &mut [u8], fences: &[u64]) -> Result<(), String> {
            self.note(format!("submit {ctx} {} {fences:?}", commands.len()))
        }
        fn create_blob(
            &mut self,
            blob: Blob,
            backing: Option<Vec<HostRange>>,
        ) -> Result<(), String> {
            self.note(format!(
                "blob {} {} {}",
                blob.resource,
                blob.size,
                backing.is_some()
            ))
        }
        fn map_blob(&mut self, _resource: u32) -> Result<Mapping, String> {
            let host = self.blob[self.at..].as_mut_ptr();
            Ok(Mapping {
                host: NonNull::new(host).unwrap(),
                len: self.len as u64,
                map_info: 1,
            })
        }
        fn unmap_blob(&mut self, resource: u32) -> Result<(), String> {
            self.note(format!("unmapped {resource}"))
        }
        fn create_fence(&mut self, fence: Fence) -> Result<(), String> {
            self.note(format!("fence {}", fence.id))
        }
    }

    #[derive(Default)]
    struct Window(Mutex<Vec<String>>);

    impl BlobMapper for Window {
        unsafe fn map(&self, offset: u64, _host: NonNull<u8>, len: u64) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("map {offset:#x} {len:#x}"));
            Ok(())
        }
        fn unmap(&self, offset: u64, len: u64) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("unmap {offset:#x} {len:#x}"));
            Ok(())
        }
    }

    const PAGE: u64 = 0x4000;

    fn setup(blob_len: usize) -> (Gpu3d, Arc<Log>, Arc<Window>) {
        let log = Arc::new(Log::default());
        // An over-allocation lets the fake hand out a page-aligned pointer.
        let blob = vec![0u8; blob_len + PAGE as usize];
        let at = blob.as_ptr().align_offset(PAGE as usize);
        let fake = Fake {
            log: Arc::clone(&log),
            blob,
            at,
            len: blob_len,
        };
        let window = Arc::new(Window::default());
        let gpu = Gpu3d::new(
            Box::new(fake),
            Arc::clone(&window) as Arc<dyn BlobMapper>,
            1 << 30,
            PAGE,
        );
        (gpu, log, window)
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap()
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn header(kind: u32, ctx: u32) -> Header {
        Header {
            kind,
            ctx,
            ..Header::default()
        }
    }

    fn run(gpu: &mut Gpu3d, kind: u32, ctx: u32, body: &[u8]) -> Result<Answer, u32> {
        gpu.execute(&memory(), &header(kind, ctx), &mut &body[..])
    }

    #[test]
    fn capsets_contexts_and_submissions_reach_the_renderer() {
        let (mut gpu, log, _) = setup(PAGE as usize);
        assert_eq!(gpu.capset_count(), 1);
        assert_eq!(
            run(&mut gpu, CMD_GET_CAPSET_INFO, 0, &words(&[0, 0])),
            Ok(Answer::CapsetInfo {
                id: 3,
                version: 0,
                size: 4
            })
        );
        assert_eq!(
            run(&mut gpu, CMD_GET_CAPSET_INFO, 0, &words(&[1, 0])),
            Err(RESP_ERR_INVALID_PARAMETER)
        );
        assert_eq!(
            run(&mut gpu, CMD_GET_CAPSET, 0, &words(&[3, 0])),
            Ok(Answer::Capset(vec![1, 2, 3, 4]))
        );
        assert_eq!(
            run(&mut gpu, CMD_GET_CAPSET, 0, &words(&[9, 0])),
            Err(RESP_ERR_UNSPEC)
        );

        let mut create = words(&[4, 3]);
        create.extend(b"game");
        create.resize(8 + DEBUG_NAME_LEN, 0);
        assert_eq!(
            run(&mut gpu, CMD_CTX_CREATE, 7, &create),
            Ok(Answer::NoData)
        );
        let mut submit = words(&[5, 1]);
        submit.extend([9; 5]);
        submit.extend(42u64.to_le_bytes());
        assert_eq!(run(&mut gpu, CMD_SUBMIT_3D, 7, &submit), Ok(Answer::NoData));
        // A submission longer than its body is refused before the renderer sees it.
        assert_eq!(
            run(&mut gpu, CMD_SUBMIT_3D, 7, &words(&[64, 0])),
            Err(RESP_ERR_INVALID_PARAMETER)
        );
        assert_eq!(
            *log.0.lock().unwrap(),
            ["create 7 3 game", "submit 7 5 [42]"]
        );
    }

    #[test]
    fn a_mapped_blob_joins_the_window_and_leaves_it_on_unmap_or_unref() {
        let (mut gpu, log, window) = setup(2 * PAGE as usize);
        let mut blob = words(&[11, 2, 1, 0]);
        blob.extend(0u64.to_le_bytes());
        blob.extend((2 * PAGE).to_le_bytes());
        assert_eq!(run(&mut gpu, CMD_CREATE_BLOB, 7, &blob), Ok(Answer::NoData));
        let mut map = words(&[11, 0]);
        map.extend((4 * PAGE).to_le_bytes());
        assert_eq!(run(&mut gpu, CMD_MAP_BLOB, 7, &map), Ok(Answer::MapInfo(1)));
        // Mapping it a second time, or somewhere not page-aligned, is refused.
        assert_eq!(
            run(&mut gpu, CMD_MAP_BLOB, 7, &map),
            Err(RESP_ERR_INVALID_PARAMETER)
        );
        // ...and leaves the renderer's mapping, which the guest still uses, alone.
        assert!(!log.0.lock().unwrap().contains(&"unmapped 11".to_owned()));
        assert_eq!(
            run(&mut gpu, CMD_UNMAP_BLOB, 7, &words(&[11, 0])),
            Ok(Answer::NoData)
        );
        let mut crooked = words(&[11, 0]);
        crooked.extend((PAGE + 1).to_le_bytes());
        assert_eq!(
            run(&mut gpu, CMD_MAP_BLOB, 7, &crooked),
            Err(RESP_ERR_INVALID_PARAMETER)
        );
        assert_eq!(run(&mut gpu, CMD_MAP_BLOB, 7, &map), Ok(Answer::MapInfo(1)));
        assert_eq!(
            run(&mut gpu, CMD_UNREF, 0, &words(&[11, 0])),
            Ok(Answer::NoData)
        );
        assert_eq!(
            *window.0.lock().unwrap(),
            [
                "map 0x10000 0x8000",
                "unmap 0x10000 0x8000",
                "map 0x10000 0x8000",
                "unmap 0x10000 0x8000"
            ]
        );
        assert!(log.0.lock().unwrap().contains(&"unref 11".to_owned()));
    }

    #[test]
    fn a_flushed_scanout_is_read_back_from_the_renderer() {
        let (mut gpu, _, _) = setup(PAGE as usize);
        assert_eq!(
            run(&mut gpu, CMD_CREATE_2D, 0, &words(&[5, 2, 64, 32])),
            Ok(Answer::NoData)
        );
        assert_eq!(
            run(&mut gpu, CMD_SET_SCANOUT, 0, &words(&[0, 0, 64, 32, 0, 5])),
            Ok(Answer::NoData)
        );
        let flushed = run(&mut gpu, CMD_FLUSH, 0, &words(&[0, 0, 64, 32, 5, 0])).unwrap();
        let Answer::Frame {
            width,
            height,
            format,
            pixels,
        } = flushed
        else {
            panic!("no frame");
        };
        assert_eq!(
            (width, height, format, pixels.len()),
            (64, 32, 2, 64 * 32 * 4)
        );
        assert!(pixels.iter().all(|byte| *byte == 0x7f));
        // A resource that is not shown flushes to nothing.
        assert_eq!(
            run(&mut gpu, CMD_FLUSH, 0, &words(&[0, 0, 1, 1, 6, 0])),
            Ok(Answer::NoData)
        );
        assert_eq!(
            run(&mut gpu, CMD_SET_SCANOUT, 0, &words(&[0, 0, 1, 1, 1, 5])),
            Err(RESP_ERR_INVALID_SCANOUT_ID)
        );
        // A rectangle no display has is refused, so a flush never sizes a buffer from it.
        let huge = words(&[0, 0, u32::MAX, u32::MAX, 0, 5]);
        assert_eq!(
            run(&mut gpu, CMD_SET_SCANOUT, 0, &huge),
            Err(RESP_ERR_INVALID_PARAMETER)
        );
        let empty = words(&[0, 0, 0, 32, 0, 5]);
        assert_eq!(
            run(&mut gpu, CMD_SET_SCANOUT, 0, &empty),
            Err(RESP_ERR_INVALID_PARAMETER)
        );
        // Resource zero still turns the scanout off, whatever the rectangle.
        let off = words(&[0, 0, 0, 0, 0, 0]);
        assert_eq!(run(&mut gpu, CMD_SET_SCANOUT, 0, &off), Ok(Answer::NoData));
    }

    #[test]
    fn fenced_answers_wait_for_their_fence_on_their_own_timeline() {
        let ring = |ctx, ring| Timeline {
            ctx,
            ring: Some(ring),
        };
        let fence = |id, timeline| Fence { id, timeline };
        let mut pending = PendingFences::default();
        pending.hold(fence(1, ring(7, 0)), "a");
        pending.hold(fence(2, ring(7, 0)), "b");
        pending.hold(fence(1, ring(7, 1)), "c");
        pending.hold(fence(5, Timeline { ctx: 0, ring: None }), "d");
        assert_eq!(pending.release(fence(1, ring(7, 0))), ["a"]);
        assert!(pending.release(fence(9, ring(8, 0))).is_empty());
        assert_eq!(pending.release(fence(3, ring(7, 0))), ["b"]);
        assert_eq!(
            pending.release(fence(5, Timeline { ctx: 0, ring: None })),
            ["d"]
        );
        assert_eq!(pending.release(fence(1, ring(7, 1))), ["c"]);
        assert!(pending.is_empty());

        let fenced = Header {
            flags: VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX,
            fence: 4,
            ctx: 7,
            ring: 2,
            ..Header::default()
        };
        assert_eq!(Fence::of(&fenced), Some(fence(4, ring(7, 2))));
        let global = Header {
            flags: VIRTIO_GPU_FLAG_FENCE,
            ..fenced
        };
        assert_eq!(
            Fence::of(&global).unwrap().timeline,
            Timeline { ctx: 0, ring: None }
        );
        assert_eq!(Fence::of(&Header::default()), None);
    }
}
