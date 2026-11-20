//! The guest's picture as a GPU texture. Frames in a layout the GPU reads (BGRX, RGBA) are copied
//! to it as they are, once; turning the picture upright is done by where the corners sample from,
//! not by moving pixels. Other layouts are converted on the CPU first.

use std::sync::Arc;

use devices::virtio::gpu::{BYTES_PER_PIXEL, Frame};
use egui::{Color32, Pos2, Rect, TextureId};

use crate::frames::{FORMATS_BGRX, FORMATS_RGBX};
use crate::orientation;
use crate::shared::SharedBuffer;

/// The GPU's name for a frame layout it can read as it is, if there is one. The fourth byte may
/// be padding rather than alpha; the picture is drawn over black, so either way it shows as it is.
fn native_format(format: u32) -> Option<wgpu::TextureFormat> {
    if FORMATS_BGRX.contains(&format) {
        Some(wgpu::TextureFormat::Bgra8Unorm)
    } else if FORMATS_RGBX.contains(&format) {
        Some(wgpu::TextureFormat::Rgba8Unorm)
    } else {
        None
    }
}

/// Where in the panel's texture (0 to 1 across and down) the upright picture's point `(u, v)`
/// is, the picture being turned by `turns` quarter turns. The same turn as
/// `orientation::to_panel`, on a unit square.
pub fn panel_uv(u: f32, v: f32, turns: u8) -> (f32, f32) {
    match turns % 4 {
        0 => (u, v),
        1 => (1.0 - v, u),
        2 => (1.0 - u, 1.0 - v),
        _ => (v, 1.0 - u),
    }
}

struct Native {
    texture: wgpu::Texture,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    /// The texture is a view of a shared buffer the renderer fills, which the CPU cannot upload to.
    shared: bool,
}

/// A shared buffer's texture, kept so it is made once.
struct SharedTexture {
    /// The buffer, the layout of its bytes and the picture's size.
    key: (u64, wgpu::TextureFormat, (u32, u32)),
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// A texture over the bytes of `buffer`, which the renderer writes as they lie. `None` where the
/// GPU is not Metal or refuses it.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn shared_texture(
    device: &wgpu::Device,
    buffer: &SharedBuffer,
    format: wgpu::TextureFormat,
    size: (u32, u32),
) -> Option<wgpu::Texture> {
    use objc2_metal::{MTLPixelFormat, MTLTextureType};
    use wgpu::hal::api::Metal;

    let pixel = match format {
        wgpu::TextureFormat::Bgra8Unorm => MTLPixelFormat::BGRA8Unorm,
        wgpu::TextureFormat::Rgba8Unorm => MTLPixelFormat::RGBA8Unorm,
        _ => return None,
    };
    // SAFETY: only asks whether the device is a Metal one.
    unsafe { device.as_hal::<Metal>() }?;
    let raw = buffer.texture(pixel, size.0, size.1)?;
    let extent = wgpu::hal::CopyExtent {
        width: size.0,
        height: size.1,
        depth: 1,
    };
    // SAFETY: `raw` is a 2D, single-level, single-layer texture of `format` and `size`, made
    // for the system's GPU, which a Metal wgpu device on Apple silicon is; shader reads only.
    let hal = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            raw,
            format,
            MTLTextureType::Type2D,
            1,
            1,
            extent,
            None,
        )
    };
    let descriptor = wgpu::TextureDescriptor {
        label: Some(crate::copy::GUEST_TEXTURE),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        // Drawn from, and copied from by tests and screenshots of the texture.
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    };
    // SAFETY: `hal` was just made for this device's backend, and matches `descriptor`.
    Some(unsafe {
        device.create_texture_from_hal::<Metal>(hal, &descriptor, wgpu::wgt::TextureUses::RESOURCE)
    })
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn shared_texture(
    _device: &wgpu::Device,
    buffer: &SharedBuffer,
    _format: wgpu::TextureFormat,
    _size: (u32, u32),
) -> Option<wgpu::Texture> {
    match *buffer {}
}

/// The newest frame, on the GPU.
#[derive(Default)]
pub struct Picture {
    id: Option<TextureId>,
    native: Option<Native>,
    /// The CPU-converted texture, for layouts the GPU cannot read as they are.
    converted: Option<egui::TextureHandle>,
    /// Quarter turns the texture needs to stand upright, and the panel's size.
    turns: u8,
    panel: (u32, u32),
    /// Textures of the shared buffers frames came in.
    shared: Vec<SharedTexture>,
    /// The shared buffer on screen, held so that it is not read into again while it is.
    holding: Option<Arc<SharedBuffer>>,
    /// Whether the last frame was shown from its shared buffer, for the debug log.
    was_shared: bool,
}

impl Picture {
    /// Size of the picture shown upright, if there is one yet.
    pub fn upright(&self) -> Option<(u32, u32)> {
        self.id
            .map(|_| orientation::upright_size(self.panel, self.turns))
    }

    /// Quarter turns the panel is shown turned by.
    pub fn turns(&self) -> u8 {
        self.turns
    }

    /// The shared buffer the picture is drawn from, if it is: it must stay unwritten until the
    /// GPU has finished the frame drawn with it.
    pub fn sampled(&self) -> Option<Arc<SharedBuffer>> {
        self.holding.clone()
    }

    /// Puts `frame`, turned by `turns`, on the GPU. `buffer` is the shared buffer it lies in, if
    /// it does, which the GPU then reads as it is.
    pub fn update(
        &mut self,
        frame: &Frame<'_>,
        buffer: Option<&Arc<SharedBuffer>>,
        turns: u8,
        render: &egui_wgpu::RenderState,
        ctx: &egui::Context,
    ) {
        let stride = frame.width as usize * BYTES_PER_PIXEL;
        if frame.width == 0 || frame.pixels.len() != stride * frame.height as usize {
            return;
        }
        match native_format(frame.format) {
            Some(format) => {
                let size = (frame.width, frame.height);
                let shared =
                    buffer.is_some_and(|buffer| self.show_shared(buffer, size, format, render));
                if shared != self.was_shared {
                    self.was_shared = shared;
                    log::debug!("{}", crate::copy::frame_path(shared));
                }
                if !shared {
                    self.holding = None;
                    self.upload(frame, format, render);
                }
            }
            None => {
                self.holding = None;
                let Some(image) = crate::window::color_image(frame, turns) else {
                    return;
                };
                let options = egui::TextureOptions::LINEAR;
                match &mut self.converted {
                    Some(texture) => texture.set(image, options),
                    None => {
                        let name = crate::copy::GUEST_TEXTURE;
                        self.converted = Some(ctx.load_texture(name, image, options));
                    }
                }
                self.id = self.converted.as_ref().map(egui::TextureHandle::id);
                self.native = None;
            }
        }
        self.turns = turns;
        self.panel = (frame.width, frame.height);
    }

    /// Shows `buffer` as it is. False if the GPU cannot, and the frame must be uploaded.
    fn show_shared(
        &mut self,
        buffer: &Arc<SharedBuffer>,
        size: (u32, u32),
        format: wgpu::TextureFormat,
        render: &egui_wgpu::RenderState,
    ) -> bool {
        let key = (buffer.id(), format, size);
        let at = match self.shared.iter().position(|made| made.key == key) {
            Some(at) => at,
            None => {
                let Some(texture) = shared_texture(&render.device, buffer, format, size) else {
                    return false;
                };
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                // Buffers of another size are gone: the guest's screen changed.
                self.shared.retain(|made| made.key.2 == size);
                self.shared.push(SharedTexture { key, texture, view });
                self.shared.len() - 1
            }
        };
        let made = &self.shared[at];
        let (texture, view) = (made.texture.clone(), made.view.clone());
        self.bind(&view, render);
        self.native = Some(Native {
            texture,
            size,
            format,
            shared: true,
        });
        self.converted = None;
        self.holding = Some(Arc::clone(buffer));
        true
    }

    /// Has egui draw `view`, as the picture's texture.
    fn bind(&mut self, view: &wgpu::TextureView, render: &egui_wgpu::RenderState) {
        let mut renderer = render.renderer.write();
        let filter = wgpu::FilterMode::Linear;
        match self.id.filter(|_| self.native.is_some()) {
            Some(id) => {
                renderer.update_egui_texture_from_wgpu_texture(&render.device, view, filter, id);
            }
            None => {
                self.id = Some(renderer.register_native_texture(&render.device, view, filter));
            }
        }
    }

    fn upload(
        &mut self,
        frame: &Frame<'_>,
        format: wgpu::TextureFormat,
        render: &egui_wgpu::RenderState,
    ) {
        let size = (frame.width, frame.height);
        let extent = wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        };
        let fits = self
            .native
            .as_ref()
            .is_some_and(|native| native.size == size && native.format == format && !native.shared);
        if !fits {
            let texture = render.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(crate::copy::GUEST_TEXTURE),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.bind(&view, render);
            self.native = Some(Native {
                texture,
                size,
                format,
                shared: false,
            });
            self.converted = None;
        }
        if let Some(native) = &self.native {
            render.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &native.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                frame.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.0 * BYTES_PER_PIXEL as u32),
                    rows_per_image: Some(size.1),
                },
                extent,
            );
        }
    }

    /// Draws the picture upright in `rect`, over black.
    pub fn paint(&self, painter: &egui::Painter, rect: Rect) {
        let Some(id) = self.id else {
            return;
        };
        painter.rect_filled(rect, 0.0, Color32::BLACK);
        // The converted texture was turned upright when it was made.
        let turns = if self.native.is_some() { self.turns } else { 0 };
        let mut mesh = egui::Mesh::with_texture(id);
        let corners = [
            (rect.left_top(), (0.0, 0.0)),
            (rect.right_top(), (1.0, 0.0)),
            (rect.right_bottom(), (1.0, 1.0)),
            (rect.left_bottom(), (0.0, 1.0)),
        ];
        for (pos, (u, v)) in corners {
            let (x, y) = panel_uv(u, v, turns);
            mesh.vertices.push(egui::epaint::Vertex {
                pos,
                uv: Pos2::new(x, y),
                color: Color32::WHITE,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(mesh));
    }

    /// Lets go of the GPU texture, before the renderer goes.
    pub fn release(&mut self, render: Option<&egui_wgpu::RenderState>) {
        if let (Some(id), Some(render), Some(_)) = (self.id, render, &self.native) {
            render.renderer.write().free_texture(&id);
        }
        self.shared.clear();
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_sample_where_the_cpu_turn_puts_them() {
        // A 4x2 panel; each upright corner's uv must land on the panel pixel to_panel picks.
        let panel = (4u32, 2u32);
        for turns in 0..4 {
            let (w, h) = orientation::upright_size(panel, turns);
            for (x, y, u, v) in [
                (0, 0, 0.0, 0.0),
                (w - 1, 0, 1.0, 0.0),
                (w - 1, h - 1, 1.0, 1.0),
                (0, h - 1, 0.0, 1.0),
            ] {
                let (px, py) = orientation::to_panel(x, y, panel, turns).unwrap();
                let (pu, pv) = panel_uv(u, v, turns);
                // The pixel's own corner of the unit square, rounded to the panel's edges.
                let expected = (
                    if px == 0 { 0.0 } else { 1.0 },
                    if py == 0 { 0.0 } else { 1.0 },
                );
                assert_eq!((pu, pv), expected, "turns {turns} corner {x},{y}");
            }
        }
    }

    #[test]
    fn layouts_the_gpu_reads_go_up_as_they_are() {
        assert_eq!(
            native_format(FORMATS_BGRX[0]),
            Some(wgpu::TextureFormat::Bgra8Unorm)
        );
        assert_eq!(
            native_format(FORMATS_RGBX[0]),
            Some(wgpu::TextureFormat::Rgba8Unorm)
        );
        assert_eq!(native_format(3), None);
    }

    /// A real GPU: frames read into a shared buffer reach a texture the GPU samples with no upload.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    mod shared_on_the_gpu {
        use std::time::Instant;

        use super::super::*;
        use crate::painter::block_on;
        use crate::shared::SharedPool;

        const WIDTH: u32 = 1080;
        const HEIGHT: u32 = 2400;
        /// Frames timed, to show what an upload costs against what sharing does.
        const TIMED_FRAMES: u32 = 30;

        fn pattern(len: usize) -> Vec<u8> {
            (0..len).map(|at| (at % 251) as u8).collect()
        }

        fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
            let instance = wgpu::Instance::default();
            let adapter =
                block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
            block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
        }

        /// Copies `texture` into a buffer and returns its bytes, rows packed.
        fn read_texture(
            device: &wgpu::Device,
            queue: &wgpu::Queue,
            texture: &wgpu::Texture,
            (width, height): (u32, u32),
        ) -> Vec<u8> {
            let row = width * BYTES_PER_PIXEL as u32;
            let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let out = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(padded * height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            encoder.copy_texture_to_buffer(
                texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &out,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit([encoder.finish()]);
            out.slice(..)
                .map_async(wgpu::MapMode::Read, |result| result.unwrap());
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let data = out.slice(..).get_mapped_range().unwrap();
            data.chunks(padded as usize)
                .flat_map(|line| &line[..row as usize])
                .copied()
                .collect()
        }

        #[test]
        fn a_shared_buffer_is_sampled_as_the_renderer_wrote_it() {
            let (Some((device, queue)), Some(pool)) = (device(), SharedPool::new()) else {
                eprintln!("no GPU; test skipped");
                return;
            };
            let (width, height) = (64, 32);
            let buffer = pool.acquire(width, height).unwrap();
            let image = pattern((width * height) as usize * BYTES_PER_PIXEL);
            // SAFETY: the only holder besides the pool.
            unsafe { buffer.bytes_mut() }.copy_from_slice(&image);
            let texture = shared_texture(
                &device,
                &buffer,
                wgpu::TextureFormat::Bgra8Unorm,
                (width, height),
            );
            let Some(texture) = texture else {
                eprintln!("not a Metal device; test skipped");
                return;
            };
            assert_eq!(
                read_texture(&device, &queue, &texture, (width, height)),
                image
            );
        }

        #[test]
        fn sharing_a_frame_costs_the_window_no_copy() {
            let (Some((device, queue)), Some(pool)) = (device(), SharedPool::new()) else {
                eprintln!("no GPU; test skipped");
                return;
            };
            let len = (WIDTH * HEIGHT) as usize * BYTES_PER_PIXEL;
            let image = pattern(len);
            let extent = wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            };
            let upload = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let mut upload_time = std::time::Duration::ZERO;
            for _ in 0..TIMED_FRAMES {
                let started = Instant::now();
                queue.write_texture(
                    upload.as_image_copy(),
                    &image,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(WIDTH * BYTES_PER_PIXEL as u32),
                        rows_per_image: Some(HEIGHT),
                    },
                    extent,
                );
                queue.submit([]);
                upload_time += started.elapsed();
                device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            }
            let buffer = pool.acquire(WIDTH, HEIGHT).unwrap();
            let started = Instant::now();
            let texture = shared_texture(
                &device,
                &buffer,
                wgpu::TextureFormat::Bgra8Unorm,
                (WIDTH, HEIGHT),
            );
            let made = started.elapsed();
            let Some(_texture) = texture else {
                eprintln!("not a Metal device; test skipped");
                return;
            };
            eprintln!(
                "window thread per frame: upload {:?}; shared: no copy (texture made once in {made:?})",
                upload_time / TIMED_FRAMES,
            );
            // What the GPU would sample is what the upload path gives, byte for byte.
            let uploaded = read_texture(&device, &queue, &upload, (WIDTH, HEIGHT));
            assert_eq!(uploaded, image);
        }
    }
}
