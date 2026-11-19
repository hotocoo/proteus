//! The guest's picture as a GPU texture. Frames in a layout the GPU reads (BGRX, RGBA) are copied
//! to it as they are, once; turning the picture upright is done by where the corners sample from,
//! not by moving pixels. Other layouts are converted on the CPU first.

use devices::virtio::gpu::{BYTES_PER_PIXEL, Frame};
use egui::{Color32, Pos2, Rect, TextureId};

use crate::frames::{FORMATS_BGRX, FORMATS_RGBX};
use crate::orientation;

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

    /// Puts `frame`, turned by `turns`, on the GPU.
    pub fn update(
        &mut self,
        frame: &Frame<'_>,
        turns: u8,
        render: &egui_wgpu::RenderState,
        ctx: &egui::Context,
    ) {
        let stride = frame.width as usize * BYTES_PER_PIXEL;
        if frame.width == 0 || frame.pixels.len() != stride * frame.height as usize {
            return;
        }
        match native_format(frame.format) {
            Some(format) => self.upload(frame, format, render),
            None => {
                let Some(image) = crate::window::color_image(frame, 0) else {
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
            .is_some_and(|native| native.size == size && native.format == format);
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
            let mut renderer = render.renderer.write();
            let filter = wgpu::FilterMode::Linear;
            match self.id.filter(|_| self.native.is_some()) {
                Some(id) => {
                    renderer.update_egui_texture_from_wgpu_texture(
                        &render.device,
                        &view,
                        filter,
                        id,
                    );
                }
                None => {
                    self.id = Some(renderer.register_native_texture(&render.device, &view, filter));
                }
            }
            self.native = Some(Native {
                texture,
                size,
                format,
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
        // The converted texture is already upright.
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
}
