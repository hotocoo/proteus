//! gfxstream through `rutabaga_gfx`.

use std::io::{IoSlice, IoSliceMut};
use std::ptr::NonNull;

use devices::virtio::gpu3d::{
    Blob, Create3d, Fence, FenceDone, HostRange, Mapping, Renderer, Timeline, Transfer,
};
use rutabaga_gfx::{
    RUTABAGA_FLAG_FENCE, RUTABAGA_FLAG_INFO_RING_IDX, ResourceCreate3D, ResourceCreateBlob,
    Rutabaga, RutabagaBuilder, RutabagaComponentType, RutabagaFence, RutabagaFenceHandler,
    RutabagaIovec, Transfer3D,
};

/// gfxstream's capability sets offered: Vulkan, which the guest renders with (and OpenGL ES through
/// ANGLE on top of it, blueprint section 7.1), and the composer's, which the guest's display
/// composer and allocator open. These are the two Google's Cuttlefish launcher offers with ANGLE.
const CAPSET_GFXSTREAM_VULKAN: u32 = 3;
const CAPSET_GFXSTREAM_COMPOSER: u32 = 9;
/// How many capability sets that is.
pub const CAPSETS: u32 = 2;

pub struct Gfxstream(Rutabaga);

fn text(err: impl std::fmt::Display) -> String {
    err.to_string()
}

fn iovecs(backing: Vec<HostRange>) -> Vec<RutabagaIovec> {
    backing
        .into_iter()
        .map(|range| RutabagaIovec {
            base: range.addr as *mut std::ffi::c_void,
            len: range.len,
        })
        .collect()
}

fn transfer(t: &Transfer) -> Transfer3D {
    Transfer3D {
        x: t.x,
        y: t.y,
        z: t.z,
        w: t.width,
        h: t.height,
        d: t.depth,
        level: t.level,
        stride: t.stride,
        layer_stride: t.layer_stride,
        offset: t.offset,
    }
}

impl Gfxstream {
    /// Starts the renderer, reporting finished fences through `done`.
    pub fn start(done: FenceDone) -> Result<Self, String> {
        let fences = RutabagaFenceHandler::new(move |fence: RutabagaFence| {
            let ring = (fence.flags & RUTABAGA_FLAG_INFO_RING_IDX != 0).then_some(fence.ring_idx);
            let timeline = match ring {
                Some(ring) => Timeline {
                    ctx: fence.ctx_id,
                    ring: Some(ring),
                },
                None => Timeline { ctx: 0, ring: None },
            };
            done(Fence {
                id: fence.fence_id,
                timeline,
            });
        });
        let capsets = (1 << CAPSET_GFXSTREAM_VULKAN) | (1 << CAPSET_GFXSTREAM_COMPOSER);
        let rutabaga = RutabagaBuilder::new(capsets, fences)
            .set_default_component(RutabagaComponentType::Gfxstream)
            .set_use_vulkan(true)
            .set_use_gles(false)
            .set_use_surfaceless(true)
            .build()
            .map_err(text)?;
        Ok(Self(rutabaga))
    }
}

/// Starts and stops the renderer once: what `emu --gpu-check` does in its own process.
pub fn check() -> Result<(), String> {
    let renderer = Gfxstream::start(Box::new(|_| {}))?;
    renderer
        .capset_info(0)
        .map(drop)
        .ok_or_else(|| "no capability set".to_owned())
}

impl Renderer for Gfxstream {
    fn capset_info(&self, index: u32) -> Option<(u32, u32, u32)> {
        self.0.get_capset_info(index).ok()
    }

    fn capset(&self, id: u32, version: u32) -> Result<Vec<u8>, String> {
        self.0.get_capset(id, version).map_err(text)
    }

    fn context_create(&mut self, ctx: u32, init: u32, name: &[u8]) -> Result<(), String> {
        let name = std::str::from_utf8(name).ok();
        self.0.create_context(ctx, init, name).map_err(text)
    }

    fn context_destroy(&mut self, ctx: u32) -> Result<(), String> {
        self.0.destroy_context(ctx).map_err(text)
    }

    fn context_attach(&mut self, ctx: u32, resource: u32) -> Result<(), String> {
        self.0.context_attach_resource(ctx, resource).map_err(text)
    }

    fn context_detach(&mut self, ctx: u32, resource: u32) -> Result<(), String> {
        self.0.context_detach_resource(ctx, resource).map_err(text)
    }

    fn resource_create_3d(&mut self, resource: u32, c: Create3d) -> Result<(), String> {
        let create = ResourceCreate3D {
            target: c.target,
            format: c.format,
            bind: c.bind,
            width: c.width,
            height: c.height,
            depth: c.depth,
            array_size: c.array_size,
            last_level: c.last_level,
            nr_samples: c.nr_samples,
            flags: c.flags,
        };
        self.0.resource_create_3d(resource, create).map_err(text)
    }

    fn attach_backing(&mut self, resource: u32, backing: Vec<HostRange>) -> Result<(), String> {
        self.0
            .attach_backing(resource, iovecs(backing))
            .map_err(text)
    }

    fn detach_backing(&mut self, resource: u32) -> Result<(), String> {
        self.0.detach_backing(resource).map_err(text)
    }

    fn resource_unref(&mut self, resource: u32) -> Result<(), String> {
        self.0.unref_resource(resource).map_err(text)
    }

    fn transfer_write(&mut self, t: Transfer) -> Result<(), String> {
        self.0
            .transfer_write(t.ctx, t.resource, transfer(&t), None::<IoSlice>)
            .map_err(text)
    }

    fn transfer_read(&mut self, t: Transfer, into: Option<&mut [u8]>) -> Result<(), String> {
        let buf = into.map(IoSliceMut::new);
        self.0
            .transfer_read(t.ctx, t.resource, transfer(&t), buf)
            .map_err(text)
    }

    fn submit(&mut self, ctx: u32, commands: &mut [u8], fences: &[u64]) -> Result<(), String> {
        self.0.submit_command(ctx, commands, fences).map_err(text)
    }

    fn create_blob(&mut self, b: Blob, backing: Option<Vec<HostRange>>) -> Result<(), String> {
        let create = ResourceCreateBlob {
            blob_mem: b.mem,
            blob_flags: b.flags,
            blob_id: b.id,
            size: b.size,
        };
        self.0
            .resource_create_blob(b.ctx, b.resource, create, backing.map(iovecs), None)
            .map_err(text)
    }

    fn map_blob(&mut self, resource: u32) -> Result<Mapping, String> {
        let mapping = self.0.map(resource).map_err(text)?;
        let map_info = match self.0.map_info(resource) {
            Ok(info) => info,
            Err(err) => {
                // Don't leave the blob mapped when the caller sees a failure.
                let _ = self.0.unmap(resource);
                return Err(text(err));
            }
        };
        let host = NonNull::new(mapping.ptr as *mut u8).ok_or("the renderer mapped nothing")?;
        Ok(Mapping {
            host,
            len: mapping.size,
            map_info,
        })
    }

    fn unmap_blob(&mut self, resource: u32) -> Result<(), String> {
        self.0.unmap(resource).map_err(text)
    }

    fn create_fence(&mut self, fence: Fence) -> Result<(), String> {
        let (ring, flags) = match fence.timeline.ring {
            Some(ring) => (ring, RUTABAGA_FLAG_FENCE | RUTABAGA_FLAG_INFO_RING_IDX),
            None => (0, RUTABAGA_FLAG_FENCE),
        };
        let fence = RutabagaFence {
            flags,
            fence_id: fence.id,
            ctx_id: fence.timeline.ctx,
            ring_idx: ring,
        };
        self.0.create_fence(fence).map_err(text)
    }
}
