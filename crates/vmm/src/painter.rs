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
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut context) {
            return out;
        }
        std::thread::yield_now();
    }
}

/// Has Metal run the window's drawing ahead of other programs' GPU work. Another program's long
/// GPU jobs (a model being trained or run) otherwise hold back every frame the window shows:
/// under such load, a chain of 8 GPU round trips took 24 ms at Metal's default priority and 16 ms
/// at this one. `-setGPUPriority:` is private; a queue without it keeps the default.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn raise_gpu_priority(queue: &wgpu::Queue) {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, sel};
    use wgpu::hal::api::Metal;

    // SAFETY: only borrows the Metal queue wgpu made, for the call below.
    let Some(hal) = (unsafe { queue.as_hal::<Metal>() }) else {
        return;
    };
    let raw: &AnyObject = hal.as_raw().as_ref();
    if raw.class().responds_to(sel!(setGPUPriority:)) {
        // SAFETY: the queue answers `-setGPUPriority:(NSUInteger)` returning BOOL, as checked.
        let _: bool = unsafe { msg_send![raw, setGPUPriority: crate::config::WINDOW_GPU_PRIORITY] };
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
    /// The size the surface was last set to, in physical pixels.
    surface_size: (u32, u32),
}

impl Painter {
    pub fn new(event_loop: &ActiveEventLoop, window: &Arc<Window>) -> Result<Self, String> {
        let ctx = egui::Context::default();
        // ⌘+ and ⌘− belong to the guest's keys and the window's own shortcuts, not to zooming.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        let config = WgpuConfiguration {
            // A frame is drawn when the guest shows one, not to a schedule: hand it to the screen
            // at once, with no frame queued ahead, so a touch shows as soon as the guest drew it.
            surface: egui_wgpu::SurfaceConfig {
                present_mode: wgpu::PresentMode::AutoNoVsync,
                desired_maximum_frame_latency: Some(1),
            },
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
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        if let Some(render) = painter.render_state() {
            raise_gpu_priority(&render.queue);
        }
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
            surface_size: (0, 0),
        })
    }

    /// The GPU device, queue and egui's renderer, once the window has a surface.
    pub fn render_state(&self) -> Option<egui_wgpu::RenderState> {
        self.painter.render_state()
    }

    /// Keeps `held` alive until the GPU has finished what is submitted so far, which is the
    /// frame just drawn. Memory that frame reads is then not written meanwhile.
    pub fn hold_until_done(&self, held: impl Send + 'static) {
        if let Some(render) = self.painter.render_state() {
            render.queue.on_submitted_work_done(move || drop(held));
            // Finished work is noticed here; nothing else would look.
            let _ = render.device.poll(wgpu::PollType::Poll);
        }
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
            self.surface_size = (size.width, size.height);
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
        // The surface follows the window's size before every frame. A resize the program asks
        // for (a turn, a panel opening) can be drawn before its Resized event arrives; a frame
        // drawn at the old size is then stretched by the system to the new one.
        let size = window.inner_size();
        if (size.width, size.height) != self.surface_size
            && let (Some(width), Some(height)) =
                (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        {
            self.painter
                .on_window_resized(egui::ViewportId::ROOT, width, height);
            self.surface_size = (size.width, size.height);
        }
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
