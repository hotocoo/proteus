//! Draws egui into a winit window with wgpu: the guest's picture and the controls around it.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use egui_wgpu::winit::Painter as WgpuPainter;
use egui_wgpu::{RendererOptions, WgpuConfiguration, WgpuSetup};
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

/// What the window shows where nothing else is drawn.
const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Runs a future to its end on this thread. wgpu's native futures are ready at once or after a
/// short wait for the driver, so polling is enough and no async runtime is needed.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut context) {
            return out;
        }
        std::thread::yield_now();
    }
}

pub struct Painter {
    pub ctx: egui::Context,
    state: egui_winit::State,
    painter: WgpuPainter,
    /// When egui asked to be drawn again, if it did.
    pub repaint_at: Option<Instant>,
    /// Screenshots of the window to take with the next frame.
    shots: Vec<egui::ScreenshotCallback>,
}

impl Painter {
    pub fn new(event_loop: &ActiveEventLoop, window: &Arc<Window>) -> Result<Self, String> {
        let ctx = egui::Context::default();
        // ⌘+ and ⌘− belong to the guest's keys and the window's own shortcuts, not to zooming.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        let config = WgpuConfiguration {
            wgpu_setup: WgpuSetup::from_display_handle(event_loop.owned_display_handle()),
            ..WgpuConfiguration::default()
        };
        let mut painter = block_on(WgpuPainter::new(
            ctx.clone(),
            config,
            false,
            RendererOptions::default(),
        ));
        block_on(painter.set_window(egui::ViewportId::ROOT, Some(Arc::clone(window))))
            .map_err(|err| err.to_string())?;
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            None,
            painter.max_texture_side(),
        );
        Ok(Self {
            ctx,
            state,
            painter,
            repaint_at: None,
            shots: Vec::new(),
        })
    }

    /// The GPU device, queue and egui's renderer, once the window has a surface.
    pub fn render_state(&self) -> Option<egui_wgpu::RenderState> {
        self.painter.render_state()
    }

    /// Takes a screenshot of the window with the next frame. It is drawn to a texture of its own,
    /// so it works while the window is covered or the screen is asleep.
    pub fn capture(
        &mut self,
        done: impl FnOnce(std::sync::Arc<egui::ColorImage>) + Send + 'static,
    ) {
        self.shots.push(egui::ScreenshotCallback::new(done));
    }

    /// Waits for the GPU to finish what was submitted, so callbacks waiting on it (a screenshot's
    /// readback) run now and not at some later frame.
    pub fn finish(&self) {
        if let Some(render) = self.painter.render_state() {
            let wait = wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(crate::config::WINDOW_REQUEST_TIMEOUT),
            };
            if let Err(err) = render.device.poll(wait) {
                log::warn!("{}", crate::copy::gpu_wait_failed(&err));
            }
        }
    }

    /// Gives a window event to egui. True if egui used it, so the guest should not get it.
    pub fn on_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        if let WindowEvent::Resized(size) = event
            && let (Some(width), Some(height)) =
                (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        {
            self.painter
                .on_window_resized(egui::ViewportId::ROOT, width, height);
        }
        let response = self.state.on_window_event(window, event);
        // egui asks to repaint after every event, a redraw included; asking again for that one
        // would draw without end.
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            window.request_redraw();
        }
        response.consumed
    }

    /// Lays out and draws one frame with `ui`.
    pub fn paint(&mut self, window: &Arc<Window>, ui: impl FnMut(&mut egui::Ui)) {
        let input = self.state.take_egui_input(window);
        let output = self.ctx.run_ui(input, ui);
        self.state
            .handle_platform_output(window, output.platform_output);
        let primitives = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        let mut textures = output.textures_delta;
        self.painter.paint_and_update_textures(
            egui::ViewportId::ROOT,
            output.pixels_per_point,
            CLEAR,
            &primitives,
            &mut textures,
            std::mem::take(&mut self.shots),
            window,
        );
        let delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |viewport| viewport.repaint_delay);
        self.repaint_at = Instant::now().checked_add(delay);
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.painter.destroy();
    }
}
