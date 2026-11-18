//! The window: shows the guest's display, and turns the mouse and keyboard into the guest's
//! touchscreen and keyboard.
//!
//! Everything that decides what happens (where a click lands in the guest, which key a key is, how a
//! frame is scaled) is a plain function with tests, because the rest can only be checked by eye.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use devices::virtio::gpu::{BYTES_PER_PIXEL, Frame};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::ctl::{Command, Controller};
use crate::frames::channel_order;
use crate::input::button_code;
use crate::keymap::{self, Mapper, Scroll, Touch};
use crate::profiles;
use crate::{VmmError, copy};

/// The finger the mouse stands for.
const MOUSE_FINGER: u32 = 0;
/// What the window shows where the guest's picture does not reach, and before there is one.
const BACKGROUND: u32 = 0;

/// News for the window from other threads.
#[derive(Clone, Debug)]
pub enum Notice {
    /// The guest has shown a new frame.
    Frame,
    /// The machine has stopped for good; the window should close.
    Stopped,
    /// This app is now in front.
    Foreground(String),
}

/// Where the guest's picture sits inside the window: as large as fits, in proportion, centred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Viewport {
    /// Top-left corner and size of the picture, in window pixels.
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    /// Size of the guest's display.
    guest: (u32, u32),
}

impl Viewport {
    /// `None` if either the window or the guest display has no area.
    fn fit(window: (u32, u32), guest: (u32, u32)) -> Option<Self> {
        if window.0 == 0 || window.1 == 0 || guest.0 == 0 || guest.1 == 0 {
            return None;
        }
        let (window_w, window_h) = (u64::from(window.0), u64::from(window.1));
        let (guest_w, guest_h) = (u64::from(guest.0), u64::from(guest.1));
        // Whichever side runs out first sets the scale.
        let (width, height) = if window_w * guest_h <= window_h * guest_w {
            (window_w, (window_w * guest_h / guest_w).max(1))
        } else {
            ((window_h * guest_w / guest_h).max(1), window_h)
        };
        Some(Self {
            left: ((window_w - width) / 2) as u32,
            top: ((window_h - height) / 2) as u32,
            width: width as u32,
            height: height as u32,
            guest,
        })
    }

    /// The guest pixel under a window pixel, or `None` outside the picture.
    fn guest_pixel(self, x: f64, y: f64) -> Option<(u32, u32)> {
        let (x, y) = (x - f64::from(self.left), y - f64::from(self.top));
        if x < 0.0 || y < 0.0 || x >= f64::from(self.width) || y >= f64::from(self.height) {
            return None;
        }
        let across = (x * f64::from(self.guest.0) / f64::from(self.width)) as u32;
        let down = (y * f64::from(self.guest.1) / f64::from(self.height)) as u32;
        Some((across.min(self.guest.0 - 1), down.min(self.guest.1 - 1)))
    }
}

/// Draws `frame` into a window buffer of `window` pixels (one `0x00RRGGBB` word each), scaled to fit.
/// Returns false, drawing nothing, if the buffer or the frame is not the size it claims.
fn blit(frame: &Frame<'_>, window: (u32, u32), buffer: &mut [u32]) -> bool {
    let guest = (frame.width, frame.height);
    let Some(view) = Viewport::fit(window, guest) else {
        return false;
    };
    let stride = frame.width as usize * BYTES_PER_PIXEL;
    if buffer.len() != window.0 as usize * window.1 as usize
        || frame.pixels.len() != stride * frame.height as usize
    {
        return false;
    }
    let [red, green, blue] = channel_order(frame.format);
    // Which guest column each picture column shows; the same for every row.
    let columns: Vec<usize> = (0..u64::from(view.width))
        .map(|x| (x * u64::from(guest.0) / u64::from(view.width)) as usize * BYTES_PER_PIXEL)
        .collect();
    for (y, row) in buffer.chunks_exact_mut(window.0 as usize).enumerate() {
        let inside = (y as u32)
            .checked_sub(view.top)
            .filter(|y| *y < view.height);
        let Some(picture_y) = inside else {
            row.fill(BACKGROUND);
            continue;
        };
        let guest_y = (u64::from(picture_y) * u64::from(guest.1) / u64::from(view.height)) as usize;
        let source = &frame.pixels[guest_y * stride..(guest_y + 1) * stride];
        let (left, rest) = row.split_at_mut(view.left as usize);
        let (picture, right) = rest.split_at_mut(view.width as usize);
        left.fill(BACKGROUND);
        right.fill(BACKGROUND);
        for (pixel, at) in picture.iter_mut().zip(&columns) {
            *pixel = u32::from(source[at + red]) << 16
                | u32::from(source[at + green]) << 8
                | u32::from(source[at + blue]);
        }
    }
    true
}

/// The Linux key code the guest's keyboard sends for a key of the host's keyboard, by position.
/// `None` for keys the guest's keyboard does not have. The window system's name for a key is the
/// name the key table and key maps use.
fn linux_key(key: KeyCode) -> Option<u16> {
    crate::input::key_code(&format!("{key:?}"))
}

struct Shown {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
}

struct App {
    controller: Arc<Controller>,
    guest: (u32, u32),
    shown: Option<Shown>,
    /// Where the pointer is, in window pixels.
    pointer: PhysicalPosition<f64>,
    /// Whether the mouse's finger is on the guest's screen.
    touching: bool,
    /// Keys and mouse movements that stand for touches, if the user gave a key map.
    mapper: Option<Mapper>,
    /// Key maps named after apps, if the map follows the app in front.
    profiles: Option<PathBuf>,
    /// Whether the window is keeping the pointer for the key map.
    grabbed: bool,
    /// Why the window could not be made, if it could not.
    failed: Option<String>,
}

impl App {
    fn send(&self, command: Command) {
        if let Err(why) = self.controller.execute(&command) {
            log::debug!("{}", copy::window_input_dropped(&why));
        }
    }

    /// Gives a key or mouse button to the key map. Returns false if the map has no use for it.
    fn mapped(&mut self, code: u16, down: bool) -> bool {
        let touches = self.mapper.as_mut().and_then(|map| map.key(code, down));
        let Some(touches) = touches else {
            return false;
        };
        self.apply(&touches);
        self.sync_pointer();
        true
    }

    fn apply(&self, touches: &[Touch]) {
        for touch in touches {
            self.send(match *touch {
                Touch::Place { finger, x, y } => Command::Touch { finger, x, y },
                Touch::Lift { finger } => Command::Lift { finger },
                Touch::Tilt { x, z } => Command::Rotate([f64::from(x), 0.0, f64::from(z)]),
                Touch::Walk {
                    bearing,
                    km_per_hour,
                } => Command::Walk(
                    bearing.map(|bearing| (f64::from(bearing), f64::from(km_per_hour))),
                ),
            });
        }
    }

    /// Keeps and hides the pointer while the key map is using the mouse, and gives it back after.
    fn sync_pointer(&mut self) {
        let wanted = self.mapper.as_ref().is_some_and(Mapper::wants_pointer);
        let Some(shown) = self.shown.as_ref().filter(|_| wanted != self.grabbed) else {
            return;
        };
        let mode = if wanted {
            CursorGrabMode::Locked
        } else {
            CursorGrabMode::None
        };
        match shown.window.set_cursor_grab(mode) {
            Ok(()) => {
                shown.window.set_cursor_visible(!wanted);
                self.grabbed = wanted;
            }
            Err(err) => log::warn!("{}", copy::window_grab_failed(&err)),
        }
        if wanted && self.touching {
            self.touch(false);
        }
    }

    fn window_size(&self) -> Option<PhysicalSize<u32>> {
        Some(self.shown.as_ref()?.window.inner_size())
    }

    /// Moves the mouse's finger to the pointer, puts it down or lifts it, as `pressed` says.
    fn touch(&mut self, pressed: bool) {
        let size = self.window_size().unwrap_or_default();
        let view = Viewport::fit((size.width, size.height), self.guest);
        let at = view.and_then(|view| view.guest_pixel(self.pointer.x, self.pointer.y));
        match (pressed, at) {
            (true, Some((x, y))) => {
                self.touching = true;
                self.send(Command::Touch {
                    finger: MOUSE_FINGER,
                    x,
                    y,
                });
            }
            // Released, or dragged off the picture: either way the finger comes up.
            _ if self.touching => {
                self.touching = false;
                self.send(Command::Lift {
                    finger: MOUSE_FINGER,
                });
            }
            _ => {}
        }
    }

    fn draw(&mut self) {
        let Some(shown) = self.shown.as_mut() else {
            return;
        };
        let size = shown.window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        let presented = shown.surface.resize(width, height).and_then(|()| {
            let mut buffer = shown.surface.buffer_mut()?;
            let screen = self.controller.screen.as_ref();
            let drawn = screen.and_then(|screen| {
                screen.with_latest(|frame| blit(frame, (size.width, size.height), &mut buffer))
            });
            if drawn != Some(true) {
                buffer.fill(BACKGROUND);
            }
            buffer.present()
        });
        if let Err(err) = presented {
            log::warn!("{}", copy::window_draw_failed(&err));
        }
    }
}

impl ApplicationHandler<Notice> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.shown.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(copy::WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(self.guest.0, self.guest.1));
        let shown = event_loop
            .create_window(attributes)
            .map_err(|err| err.to_string())
            .and_then(|window| {
                let window = Rc::new(window);
                let context =
                    softbuffer::Context::new(Rc::clone(&window)).map_err(|err| err.to_string())?;
                let surface = softbuffer::Surface::new(&context, Rc::clone(&window))
                    .map_err(|err| err.to_string())?;
                Ok(Shown { window, surface })
            });
        match shown {
            Ok(shown) => self.shown = Some(shown),
            Err(why) => {
                self.failed = Some(why);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, notice: Notice) {
        match notice {
            Notice::Frame => {
                if let Some(shown) = &self.shown {
                    shown.window.request_redraw();
                }
            }
            Notice::Stopped => event_loop.exit(),
            Notice::Foreground(package) => self.follow(&package),
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            self.mouse_moved(dx, dy);
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // The machine is switched off; the window closes when it has stopped.
            WindowEvent::CloseRequested => self.controller.power.off(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) => {
                if let Some(shown) = &self.shown {
                    shown.window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = position;
                if self.touching {
                    self.touch(true);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                let index = match button {
                    MouseButton::Left => Some(0),
                    MouseButton::Right => Some(1),
                    MouseButton::Middle => Some(2),
                    _ => None,
                };
                let taken = index
                    .and_then(button_code)
                    .is_some_and(|code| self.mapped(code, pressed));
                // The left button is the finger, unless the key map has the mouse or the button.
                if !taken && !self.grabbed && button == MouseButton::Left {
                    self.touch(pressed);
                }
            }
            WindowEvent::CursorLeft { .. } => self.touch(false),
            // An install takes a while; the window keeps drawing meanwhile.
            WindowEvent::DroppedFile(path) => {
                let controller = Arc::clone(&self.controller);
                let installing = std::thread::Builder::new()
                    .name(copy::INSTALL_THREAD.into())
                    .spawn(move || match controller.execute(&Command::Install(path)) {
                        Ok(()) => eprintln!("{}", copy::INSTALLED),
                        Err(why) => eprintln!("{}", copy::fatal(&why)),
                    });
                if let Err(err) = installing {
                    log::error!("{}", copy::thread(&err));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => Scroll::Turns(f64::from(lines)),
                    MouseScrollDelta::PixelDelta(moved) => Scroll::Pixels(moved.y),
                };
                if let Some(map) = self.mapper.as_mut() {
                    let touches = map.scroll(scroll);
                    self.apply(&touches);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if !event.repeat => {
                let PhysicalKey::Code(key) = event.physical_key else {
                    return;
                };
                let down = event.state == ElementState::Pressed;
                if let Some(code) = linux_key(key)
                    && !self.mapped(code, down)
                {
                    self.send(Command::Key {
                        code,
                        down: Some(down),
                    });
                }
            }
            _ => {}
        }
    }
}

impl App {
    /// Changes to the key map for `package`, lifting the old map's fingers first.
    fn follow(&mut self, package: &str) {
        let Some(dir) = &self.profiles else {
            return;
        };
        let path = profiles::map_for(dir, package);
        eprintln!("{}", copy::keymap_switched(package, path.as_deref()));
        let mapper = path.and_then(|path| {
            keymap::load(&path, self.guest)
                .inspect_err(|why| eprintln!("{}", copy::fatal(why)))
                .ok()
        });
        if let Some(old) = self.mapper.as_mut() {
            let touches = old.release();
            self.apply(&touches);
        }
        self.mapper = mapper;
        self.sync_pointer();
    }

    /// The mouse moved by this much, wherever the pointer is.
    fn mouse_moved(&mut self, dx: f64, dy: f64) {
        let touches = self.mapper.as_mut().filter(|map| map.wants_pointer());
        if let Some(touches) = touches.map(|map| map.pointer(dx, dy)) {
            self.apply(&touches);
        }
    }
}

/// The window's event loop, before it runs. It has to be made, and run, on the program's first
/// thread; that is a rule of the host's window system.
pub struct WindowLoop(EventLoop<Notice>);

impl WindowLoop {
    pub fn new() -> Result<Self, VmmError> {
        let event_loop = EventLoop::<Notice>::with_user_event().build();
        event_loop
            .map(Self)
            .map_err(|err| VmmError::Device(copy::window_failed(&err)))
    }

    /// A way for other threads to send the window news.
    pub fn notifier(&self) -> impl Fn(Notice) + Send + Sync + Clone + 'static {
        let proxy: EventLoopProxy<Notice> = self.0.create_proxy();
        // After the window has closed there is nobody to tell, and that is fine.
        move |notice| {
            let _ = proxy.send_event(notice);
        }
    }

    /// Shows a window for a guest display of `guest` pixels and serves it until it is told
    /// `Notice::Stopped`. Closing the window switches the machine off through `controller`.
    pub fn run(
        self,
        controller: Arc<Controller>,
        guest: (u32, u32),
        mapper: Option<Mapper>,
        profiles: Option<PathBuf>,
    ) -> Result<(), VmmError> {
        let mut app = App {
            controller,
            guest,
            shown: None,
            pointer: PhysicalPosition::default(),
            touching: false,
            mapper,
            profiles,
            grabbed: false,
            failed: None,
        };
        let ran = self.0.run_app(&mut app);
        match (app.failed, ran) {
            (Some(why), _) => Err(VmmError::Device(copy::window_failed(&why))),
            (None, Err(err)) => Err(VmmError::Device(copy::window_failed(&err))),
            (None, Ok(())) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMAT_BGRX: u32 = 2;
    const FORMAT_RGBA: u32 = 67;

    #[test]
    fn picture_fills_the_side_that_runs_out_first_and_is_centred() {
        // Same shape: the whole window.
        let same = Viewport::fit((2560, 1440), (1280, 720)).unwrap();
        assert_eq!(
            (same.left, same.top, same.width, same.height),
            (0, 0, 2560, 1440)
        );
        // A wider window: bars left and right.
        let wide = Viewport::fit((1000, 360), (1280, 720)).unwrap();
        assert_eq!(
            (wide.left, wide.top, wide.width, wide.height),
            (180, 0, 640, 360)
        );
        // A taller window: bars above and below.
        let tall = Viewport::fit((640, 1000), (1280, 720)).unwrap();
        assert_eq!(
            (tall.left, tall.top, tall.width, tall.height),
            (0, 320, 640, 360)
        );
        assert!(Viewport::fit((0, 100), (1280, 720)).is_none());
        assert!(Viewport::fit((100, 100), (0, 720)).is_none());
    }

    #[test]
    fn pointer_maps_to_the_guest_pixel_under_it_and_to_nothing_on_the_bars() {
        let view = Viewport::fit((1000, 360), (1280, 720)).unwrap();
        assert_eq!(view.guest_pixel(180.0, 0.0), Some((0, 0)));
        assert_eq!(view.guest_pixel(819.9, 359.9), Some((1279, 719)));
        assert_eq!(view.guest_pixel(500.0, 180.0), Some((640, 360)));
        assert_eq!(view.guest_pixel(179.9, 10.0), None);
        assert_eq!(view.guest_pixel(820.0, 10.0), None);
        assert_eq!(view.guest_pixel(500.0, -1.0), None);
        assert_eq!(view.guest_pixel(500.0, 360.0), None);
    }

    #[test]
    fn frame_is_scaled_into_the_picture_and_the_bars_are_cleared() {
        // A 2x1 guest display: a red pixel and a blue one, in a layout with blue first.
        let pixels = [0, 0, 255, 0, 255, 0, 0, 0];
        let frame = Frame {
            width: 2,
            height: 1,
            format: FORMAT_BGRX,
            pixels: &pixels,
        };
        // A 4x4 window: the picture is 4x2, in the middle rows.
        let mut buffer = [0xdead_beefu32; 16];
        assert!(blit(&frame, (4, 4), &mut buffer));
        const RED: u32 = 0x00ff_0000;
        const BLUE: u32 = 0x0000_00ff;
        assert_eq!(buffer[..4], [BACKGROUND; 4]);
        assert_eq!(buffer[4..8], [RED, RED, BLUE, BLUE]);
        assert_eq!(buffer[8..12], [RED, RED, BLUE, BLUE]);
        assert_eq!(buffer[12..], [BACKGROUND; 4]);
    }

    #[test]
    fn channel_order_follows_the_frames_format() {
        let pixels = [10, 20, 30, 0];
        let frame = Frame {
            width: 1,
            height: 1,
            format: FORMAT_RGBA,
            pixels: &pixels,
        };
        let mut buffer = [0u32; 1];
        assert!(blit(&frame, (1, 1), &mut buffer));
        assert_eq!(buffer, [0x000a_141e]);
    }

    #[test]
    fn buffers_of_the_wrong_size_are_left_alone() {
        let pixels = [0u8; 8];
        let frame = Frame {
            width: 2,
            height: 1,
            format: FORMAT_RGBA,
            pixels: &pixels,
        };
        let mut short = [7u32; 3];
        assert!(!blit(&frame, (2, 2), &mut short));
        assert_eq!(short, [7; 3]);
        let truncated = Frame {
            pixels: &pixels[..4],
            ..frame
        };
        let mut buffer = [7u32; 4];
        assert!(!blit(&truncated, (2, 2), &mut buffer));
        assert_eq!(buffer, [7; 4]);
    }

    #[test]
    fn keys_map_by_position_to_linux_key_codes_the_guest_keyboard_has() {
        const KEY_Q: u16 = 16;
        const KEY_ENTER: u16 = 28;
        const KEY_SPACE: u16 = 57;
        const KEY_UP: u16 = 103;
        assert_eq!(linux_key(KeyCode::KeyQ), Some(KEY_Q));
        assert_eq!(linux_key(KeyCode::Enter), Some(KEY_ENTER));
        assert_eq!(linux_key(KeyCode::Space), Some(KEY_SPACE));
        assert_eq!(linux_key(KeyCode::ArrowUp), Some(KEY_UP));
        assert_eq!(linux_key(KeyCode::Fn), None);
        // Every key the window can send is one the guest's keyboard reports.
        for key in [
            KeyCode::Escape,
            KeyCode::KeyZ,
            KeyCode::F12,
            KeyCode::Delete,
            KeyCode::SuperRight,
            KeyCode::NumpadEnter,
        ] {
            assert!(crate::input::is_key(linux_key(key).unwrap()), "{key:?}");
        }
    }
}
