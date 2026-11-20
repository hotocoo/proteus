//! The window: shows the guest's display with the controls around it (`gui`), and turns the mouse
//! and keyboard into the guest's touchscreen and keyboard.
//!
//! Everything that decides what happens (where a click lands in the guest, which key a key is, how a
//! frame is turned and scaled) is a plain function with tests, because the rest can only be checked
//! by eye.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use devices::virtio::gpu::{BYTES_PER_PIXEL, Frame};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::config::{MIN_REMEMBERED_SIDE, PANEL_WIDTH, TOOLBAR_WIDTH};
use crate::ctl::{Command, Controller};
use crate::frames::channel_order;
use crate::gui::{Effect, Gui};
use crate::input::button_code;
use crate::keyedit::Editor;
use crate::keymap::{self, Mapper, Scroll, Touch};
use crate::painter::Painter;
use crate::picture::Picture;
use crate::{VmmError, copy, orientation, profiles};

/// Quarter turns in a whole turn, and degrees in a quarter turn.
const QUARTER_TURNS: u8 = 4;
const DEGREES_PER_TURN: f64 = 90.0;
/// The finger the mouse stands for.
const MOUSE_FINGER: u32 = 0;

/// News for the window from other threads.
#[derive(Clone, Debug)]
pub enum Notice {
    /// The guest has shown a new frame.
    Frame,
    /// The machine has stopped for good; the window should close.
    Stopped,
    /// This app is now in front.
    Foreground(String),
    /// Android turned its screen; the window turns the picture back upright.
    Rotated(u8),
    /// A line for the status bar, from work that finished.
    Status(String),
    /// The apps installed in the guest.
    Packages(Vec<String>),
    /// A request from the control socket that only the window can carry out.
    Request(crate::ctl::WindowRequest),
    /// Android was asked for the logical display size `wanted` (natural orientation, or none) and
    /// has `got` (`None` if it did not answer).
    Filled {
        wanted: Option<(u32, u32)>,
        got: Option<Option<(u32, u32)>>,
    },
}

/// The largest rectangle of the picture's shape that fits in `space`, centred, in points.
fn fit(space: egui::Rect, picture: (u32, u32)) -> Option<egui::Rect> {
    let (width, height) = (picture.0 as f32, picture.1 as f32);
    if width <= 0.0 || height <= 0.0 || space.width() <= 0.0 || space.height() <= 0.0 {
        return None;
    }
    let scale = (space.width() / width).min(space.height() / height);
    Some(egui::Rect::from_center_size(
        space.center(),
        egui::vec2(width * scale, height * scale),
    ))
}

/// The upright guest pixel under `point`, for a picture of `picture` pixels drawn in `rect`.
/// `None` outside it.
fn guest_pixel(rect: egui::Rect, picture: (u32, u32), point: egui::Pos2) -> Option<(u32, u32)> {
    if !rect.contains(point) || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return None;
    }
    let across = (point.x - rect.left()) / rect.width() * picture.0 as f32;
    let down = (point.y - rect.top()) / rect.height() * picture.1 as f32;
    Some((
        (across as u32).min(picture.0.saturating_sub(1)),
        (down as u32).min(picture.1.saturating_sub(1)),
    ))
}

/// `frame` turned upright, as an RGBA image for the window. `None` if its size and pixels disagree.
pub(crate) fn color_image(frame: &Frame<'_>, turns: u8) -> Option<egui::ColorImage> {
    let stride = frame.width as usize * BYTES_PER_PIXEL;
    if frame.width == 0 || frame.pixels.len() != stride * frame.height as usize {
        return None;
    }
    let (width, height, pixels) = orientation::upright(frame, turns);
    let [red, green, blue] = channel_order(frame.format);
    let rgba: Vec<egui::Color32> = pixels
        .as_chunks::<BYTES_PER_PIXEL>()
        .0
        .iter()
        .map(|pixel| egui::Color32::from_rgb(pixel[red], pixel[green], pixel[blue]))
        .collect();
    Some(egui::ColorImage::new(
        [width as usize, height as usize],
        rgba,
    ))
}

/// Saves a screenshot of the window as a PNG.
fn save_shot(image: &egui::ColorImage, path: &std::path::Path) -> Result<(), String> {
    let pixels: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_array())
        .collect();
    let frame = Frame {
        width: image.size[0] as u32,
        height: image.size[1] as u32,
        format: crate::frames::FORMATS_RGBX[0],
        pixels: &pixels,
    };
    let png = crate::frames::png(&frame).ok_or(copy::CTL_NO_PICTURE)?;
    std::fs::write(path, png).map_err(|err| copy::file(path, &err))
}

/// What a shortcut does: one of Android's buttons, a turn of the device, or a screenshot.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shortcut {
    Button(u16),
    /// Turn the device a quarter turn: clockwise when true.
    Turn(bool),
    Screenshot,
}

/// Whether the shortcut modifier is held: ⌘ on a Mac, Ctrl+Alt elsewhere (the Windows/Super key
/// belongs to the desktop there).
fn shortcut_held(modifiers: ModifiersState) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.super_key()
    } else {
        modifiers.control_key() && modifiers.alt_key()
    }
}

/// The shortcut for `key` pressed with the shortcut modifier, if it is one. Keys used this way never
/// reach the guest.
fn shortcut(key: KeyCode, shift: bool) -> Option<Shortcut> {
    use crate::input::{
        KEY_APPSELECT, KEY_BACK, KEY_HOMEPAGE, KEY_POWER, KEY_VOLUMEDOWN, KEY_VOLUMEUP,
    };
    Some(match key {
        KeyCode::Backspace | KeyCode::KeyB => Shortcut::Button(KEY_BACK),
        KeyCode::KeyH if shift => Shortcut::Button(KEY_HOMEPAGE),
        KeyCode::KeyO => Shortcut::Button(KEY_APPSELECT),
        KeyCode::KeyP => Shortcut::Button(KEY_POWER),
        KeyCode::ArrowUp => Shortcut::Button(KEY_VOLUMEUP),
        KeyCode::ArrowDown => Shortcut::Button(KEY_VOLUMEDOWN),
        KeyCode::ArrowLeft => Shortcut::Turn(false),
        KeyCode::ArrowRight => Shortcut::Turn(true),
        KeyCode::KeyS => Shortcut::Screenshot,
        _ => return None,
    })
}

/// Whether `key` is the key that makes shortcuts, which is not passed to the guest on a Mac (⌘ is
/// no key Android knows, and as Meta it would start Android's own shortcuts too).
fn is_shortcut_modifier(key: KeyCode) -> bool {
    cfg!(target_os = "macos") && matches!(key, KeyCode::SuperLeft | KeyCode::SuperRight)
}

/// The Linux key code the guest's keyboard sends for a key of the host's keyboard, by position.
/// `None` for keys the guest's keyboard does not have. The window system's name for a key is the
/// name the key table and key maps use.
fn linux_key(key: KeyCode) -> Option<u16> {
    crate::input::key_code(&key_name(key))
}

/// The window system's name for a key, which key maps use too.
pub fn key_name(key: KeyCode) -> String {
    format!("{key:?}")
}

/// The window's `size` without a side panel of `panel` points, which may be open or shut when the
/// size comes back. `None` for a window too small to mean anything (minimized).
fn remembered_size(size: LogicalSize<f64>, panel: f64) -> Option<LogicalSize<f64>> {
    let size = LogicalSize::new(size.width - panel, size.height);
    (size.width > MIN_REMEMBERED_SIDE && size.height > MIN_REMEMBERED_SIDE).then_some(size)
}

/// The window's size after the picture turns to `picture` pixels: the same scale as before, made
/// smaller if needed to fit a share of the monitor, with `chrome` points of controls beside it.
fn turned_size(
    picture: (u32, u32),
    scale: f64,
    chrome: f64,
    monitor: Option<(f64, f64)>,
) -> (f64, f64) {
    let (width, height) = (f64::from(picture.0), f64::from(picture.1));
    let fits = monitor.map_or(scale, |(monitor_w, monitor_h)| {
        let share = crate::config::WINDOW_SCREEN_SHARE;
        ((monitor_w * share - chrome) / width).min(monitor_h * share / height)
    });
    let scale = scale.min(fits);
    (width * scale + chrome, height * scale)
}

/// The window's first size, in logical pixels: the guest's picture, made smaller if needed so it
/// fits a share of the monitor (`None` when the monitor is not known), and the toolbar beside it.
fn initial_size(picture: (u32, u32), monitor: Option<(f64, f64)>) -> (f64, f64) {
    let toolbar = f64::from(TOOLBAR_WIDTH);
    let (width, height) = (f64::from(picture.0), f64::from(picture.1));
    let scale = monitor.map_or(1.0, |(monitor_w, monitor_h)| {
        let share = crate::config::WINDOW_SCREEN_SHARE;
        ((monitor_w * share - toolbar) / width)
            .min(monitor_h * share / height)
            .min(1.0)
    });
    (width * scale + toolbar, height * scale)
}

struct Shown {
    window: Arc<Window>,
    painter: Painter,
}

struct App {
    controller: Arc<Controller>,
    /// Size of the guest's display panel, in its own (natural) orientation.
    guest: (u32, u32),
    shown: Option<Shown>,
    gui: Gui,
    /// The guest's picture, as last uploaded, and whether a newer frame has come since.
    picture_texture: Picture,
    frame_waiting: bool,
    /// Where the whole panel was drawn last, in points (it may reach past what is shown), and its
    /// upright size in guest pixels.
    picture: Option<(egui::Rect, (u32, u32))>,
    /// The part of the picture shown, in points: what Android draws its logical display into.
    shown_part: Option<egui::Rect>,
    /// The space the picture had to fit in, in points.
    space: Option<egui::Rect>,
    /// The logical display size Android has (natural orientation), if any, as it last said.
    logical: Option<(u32, u32)>,
    /// Whether `logical` was read from Android since the window opened, and whether a request
    /// is on its way.
    synced: bool,
    asking: bool,
    /// When the window last changed size, until Android has been given its new shape.
    resized: Option<Instant>,
    /// Passes news to the window from other threads.
    notify: Arc<dyn Fn(Notice) + Send + Sync>,
    /// Where the pointer is, in window pixels.
    pointer: PhysicalPosition<f64>,
    /// Whether the mouse's finger is on the guest's screen.
    touching: bool,
    /// Keys and mouse movements that stand for touches, if the user gave a key map.
    mapper: Option<Mapper>,
    /// Key maps named after apps, if the map follows the app in front.
    profiles: Option<PathBuf>,
    /// The key map being edited on the picture, if one is.
    editor: Option<Editor>,
    /// Whether the window is keeping the pointer for the key map.
    grabbed: bool,
    /// Why the window could not be made, if it could not.
    failed: Option<String>,
    /// Modifier keys held now.
    modifiers: ModifiersState,
    /// Guest key codes sent down and not yet up, so each gets its up even if the window loses
    /// focus or a shortcut modifier comes between.
    held: HashSet<u16>,
    /// How far the device is turned about the screen's axis, in quarter turns clockwise.
    device_turns: u8,
    /// What the user asked for when the window closed: a restart, maybe after a factory reset.
    exit: crate::Exit,
    /// Whether Android has been asked to shut down.
    shutting_down: bool,
    /// Frames the guest has sent since `counted`, for the debug log's frame rate.
    frames: u32,
    counted: Instant,
    /// Recent waits between frames, for the 1% low.
    gaps: crate::usage::FrameGaps,
    /// Whether a panel was beside the picture when the window last drew.
    panel_shown: bool,
    /// Whether the window has the keyboard and mouse.
    focused: bool,
    /// Android's screen turn, in quarter turns, as last reported.
    turns: u8,
    /// The window's last size (logical pixels) with the screen upright and turned sideways, so
    /// turning back restores the size the user gave it. The width is without a side panel.
    sizes: [Option<LogicalSize<f64>>; 2],
    /// This process's cost at the last count, for the CPU meter.
    usage: crate::usage::Reading,
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
    /// Only while the window has focus: a lock kept after switching apps would hold the
    /// pointer in other apps, and one macOS dropped on the switch would never come back.
    fn sync_pointer(&mut self) {
        let wanted = self.focused && self.mapper.as_ref().is_some_and(Mapper::wants_pointer);
        let Some(shown) = self.shown.as_ref().filter(|_| wanted != self.grabbed) else {
            return;
        };
        // A locked pointer stays where it is, and clicks land there: over the picture, not on a
        // button of the toolbar or the window's edge.
        if wanted && let Some((rect, _)) = self.picture {
            let scale = shown.painter.ctx.pixels_per_point();
            let center = rect.center();
            let position =
                PhysicalPosition::new(f64::from(center.x * scale), f64::from(center.y * scale));
            if shown.window.set_cursor_position(position).is_ok() {
                self.pointer = position;
            }
        }
        let mode = if wanted {
            CursorGrabMode::Locked
        } else {
            CursorGrabMode::None
        };
        // X11 cannot lock the pointer in place, only keep it inside the window; that does too,
        // since the key map reads the mouse's raw movement, not where the pointer is.
        let grabbed = shown
            .window
            .set_cursor_grab(mode)
            .or_else(|err| match mode {
                CursorGrabMode::Locked => shown.window.set_cursor_grab(CursorGrabMode::Confined),
                _ => Err(err),
            });
        match grabbed {
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

    /// Size of the guest's picture as it is shown, upright.
    fn upright(&self) -> (u32, u32) {
        orientation::upright_size(self.guest, self.controller.turns())
    }

    /// The pointer, in points.
    fn pointer_points(&self) -> Option<egui::Pos2> {
        // egui's own scale, which is the one the picture's rectangle is in.
        let scale = f64::from(self.shown.as_ref()?.painter.ctx.pixels_per_point());
        Some(egui::pos2(
            (self.pointer.x / scale) as f32,
            (self.pointer.y / scale) as f32,
        ))
    }

    /// The upright guest pixel under the pointer, if it is over the picture.
    fn pointer_pixel(&self) -> Option<(u32, u32)> {
        let (rect, size) = self.picture?;
        let point = self.pointer_points()?;
        self.shown_part
            .is_some_and(|part| part.contains(point))
            .then(|| guest_pixel(rect, size, point))?
    }

    /// Gives Android the shape of the space the picture has, once the window has stopped
    /// changing size, so the picture fills it.
    fn fill(&mut self) {
        let retry = Instant::now() + crate::config::FILL_RETRY;
        let (Some(space), Some(part), Some(full)) =
            (self.space, self.shown_part, self.picture_texture.upright())
        else {
            // Nothing shown yet: Android is still starting.
            self.resized = Some(retry);
            return;
        };
        if space.height() <= 0.0 || part.height() <= 0.0 || self.asking {
            self.resized = Some(retry);
            return;
        }
        let aspect = space.width() / space.height();
        let tolerance = crate::config::FILL_TOLERANCE;
        if self.synced && (aspect / (part.width() / part.height()) - 1.0).abs() <= tolerance {
            return;
        }
        let turns = self.picture_texture.turns();
        let range = crate::config::FILL_ASPECT_RANGE;
        let wanted = crate::fill::logical_for(full, aspect, range, tolerance)
            .map(|upright| orientation::upright_size(upright, turns));
        if self.synced && wanted == self.logical {
            return;
        }
        use crate::fill::{WHEN_BOOTED, WM_SIZE, WM_SIZE_RESET};
        let set = match wanted {
            Some((width, height)) => format!("{WM_SIZE} {width}x{height}"),
            None => WM_SIZE_RESET.to_owned(),
        };
        // Set once booted, then read back what Android has, which is what the window shows.
        let command = format!("{WHEN_BOOTED} {set}; {WM_SIZE}");
        let controller = Arc::clone(&self.controller);
        let notify = Arc::clone(&self.notify);
        self.asking = true;
        let asked = std::thread::Builder::new()
            .name(copy::GUI_FILL_THREAD.into())
            .spawn(move || {
                let got = match controller.shell(&command) {
                    Ok(printed) => crate::fill::parse_wm_size(&printed),
                    Err(why) => {
                        log::debug!("{why}");
                        None
                    }
                };
                notify(Notice::Filled { wanted, got });
            });
        if let Err(err) = asked {
            self.asking = false;
            self.resized = Some(retry);
            log::error!("{}", copy::thread(&err));
        }
    }

    /// Moves the mouse's finger to the pointer, puts it down or lifts it, as `pressed` says.
    fn touch(&mut self, pressed: bool) {
        match (pressed, self.pointer_pixel()) {
            (true, Some((x, y))) => {
                log::debug!(
                    "{}",
                    copy::window_touch(self.pointer.x, self.pointer.y, x, y)
                );
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

    /// Uploads the newest frame, lays out the controls and draws everything.
    fn draw(&mut self) {
        if self.frame_waiting
            && let Some(shown) = &self.shown
            && let Some(screen) = &self.controller.screen
            && let Some(render) = shown.painter.render_state()
        {
            self.frame_waiting = false;
            let turns = screen.turns();
            let picture = &mut self.picture_texture;
            let ctx = &shown.painter.ctx;
            screen.with_latest(|frame| picture.update(frame, turns, &render, ctx));
        }
        // The picture's shape is the one of the frame on screen, so clicks match what is seen.
        let upright = self
            .picture_texture
            .upright()
            .unwrap_or_else(|| self.upright());
        let Some(shown) = self.shown.as_mut() else {
            return;
        };
        let mut effects = Vec::new();
        let gui = &mut self.gui;
        let texture = &self.picture_texture;
        let editor = &mut self.editor;
        let mut picture = None;
        let mut shown_part = None;
        let mut seen_space = None;
        let logical = self.logical;
        shown.painter.paint(&shown.window, |ui| {
            egui::Panel::right(copy::TOOLBAR_ID)
                .resizable(false)
                .exact_size(TOOLBAR_WIDTH)
                // Scrolls when the window is shorter than the buttons.
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| gui.toolbar(ui, &mut effects))
                });
            if let Some(editor) = editor.as_mut() {
                egui::Panel::right(copy::PANEL_ID)
                    .resizable(false)
                    .exact_size(PANEL_WIDTH)
                    .show(ui, |ui| editor.panel(ui, &mut effects));
            } else if gui.open.is_some() {
                egui::Panel::right(copy::PANEL_ID)
                    .resizable(false)
                    .exact_size(PANEL_WIDTH)
                    .show(ui, |ui| {
                        gui.panel(ui, &mut effects);
                    });
            }
            if !gui.status().is_empty() {
                egui::Panel::bottom(copy::STATUS_ID).show(ui, |ui| {
                    ui.label(gui.status());
                });
            }
            egui::CentralPanel::no_frame().show(ui, |ui| {
                let space = ui.max_rect();
                seen_space = Some(space);
                // The part of the panel Android draws its logical display into, if it has one.
                let (x, y, width, height) =
                    logical.map_or((0, 0, upright.0, upright.1), |natural| {
                        let turns = texture.turns();
                        crate::fill::shown_rect(upright, orientation::upright_size(natural, turns))
                    });
                match (texture.upright(), fit(space, (width, height))) {
                    (Some(_), Some(part)) => {
                        let scale = part.width() / width.max(1) as f32;
                        let rect = egui::Rect::from_min_size(
                            part.min - egui::vec2(x as f32 * scale, y as f32 * scale),
                            egui::vec2(upright.0 as f32 * scale, upright.1 as f32 * scale),
                        );
                        texture.paint(&ui.painter().with_clip_rect(part), rect);
                        picture = Some((rect, upright));
                        shown_part = Some(part);
                        if let Some(editor) = editor.as_mut() {
                            editor.overlay(ui, rect, upright);
                        }
                    }
                    _ => {
                        ui.centered_and_justified(|ui| ui.label(copy::gui::STARTING));
                    }
                }
            });
        });
        self.picture = picture;
        self.shown_part = shown_part;
        self.space = seen_space;
        for effect in effects {
            self.effect(effect);
        }
        self.fit_panel();
    }

    /// Widens the window when a panel opens, and narrows it when it closes, so the picture keeps
    /// its size.
    fn fit_panel(&mut self) {
        let open = self.editor.is_some() || self.gui.open.is_some();
        let Some(shown) = self.shown.as_ref().filter(|_| open != self.panel_shown) else {
            return;
        };
        self.panel_shown = open;
        let size = shown
            .window
            .inner_size()
            .to_logical::<f64>(shown.window.scale_factor());
        let change = if open { PANEL_WIDTH } else { -PANEL_WIDTH };
        let width = (size.width + f64::from(change)).max(f64::from(TOOLBAR_WIDTH));
        let _ = shown
            .window
            .request_inner_size(LogicalSize::new(width, size.height));
    }

    fn effect(&mut self, effect: Effect) {
        match effect {
            Effect::LoadKeymap(path) => self.use_keymap(path),
            Effect::EditKeymap => {
                let path = PathBuf::from(self.gui.keymap_path());
                match Editor::open(&path) {
                    Ok(editor) => {
                        self.use_keymap(None);
                        self.editor = Some(editor);
                    }
                    Err(why) => self.gui.set_status(why),
                }
            }
            Effect::CloseEditor(saved) => {
                self.editor = None;
                if let Some(path) = saved {
                    self.gui.set_keymap_path(&path);
                    self.use_keymap(Some(path));
                }
            }
            Effect::Restart => {
                self.exit = crate::Exit::Restart;
                self.shut_down();
            }
            Effect::FactoryReset => {
                self.exit = crate::Exit::FactoryReset;
                self.shut_down();
            }
            Effect::Turn(clockwise) => self.shortcut(Shortcut::Turn(clockwise)),
        }
        if let Some(shown) = &self.shown {
            shown.window.request_redraw();
        }
    }

    /// Starts using the key map at `path`, or none, lifting the old map's fingers first.
    fn use_keymap(&mut self, path: Option<PathBuf>) {
        let mapper = match path {
            Some(path) => match keymap::load(&path, orientation::any_way_up(self.guest)) {
                Ok(mapper) => {
                    self.gui.set_status(copy::gui::keymap_in_use(&path));
                    Some(mapper)
                }
                Err(why) => {
                    self.gui.set_status(why);
                    return;
                }
            },
            None => None,
        };
        if let Some(old) = self.mapper.as_mut() {
            let touches = old.release();
            self.apply(&touches);
        }
        self.mapper = mapper;
        self.sync_pointer();
    }

    /// Carries out a request from the control socket.
    fn request(&mut self, event_loop: &ActiveEventLoop, request: crate::ctl::WindowRequest) {
        use crate::ctl::WindowRequest;
        match request {
            WindowRequest::Shot(path, reply) => match self.shown.as_mut() {
                Some(shown) => {
                    shown.painter.capture(move |image| {
                        let _ = reply.send(save_shot(&image, &path));
                    });
                    // Drawn now: macOS sends no redraw to a covered window, and a capture is
                    // drawn to a texture of its own, so it needs none.
                    self.frame_waiting = true;
                    self.draw();
                    if let Some(shown) = &self.shown {
                        shown.painter.finish();
                    }
                }
                None => {
                    let _ = reply.send(Err(copy::CTL_NO_PICTURE.to_owned()));
                }
            },
            WindowRequest::Click { x, y, reply } => {
                let _ = reply.send(self.click(event_loop, x, y));
            }
            WindowRequest::Scroll { x, y, lines, reply } => {
                let _ = reply.send(self.scroll(event_loop, x, y, lines));
            }
            WindowRequest::Resize {
                width,
                height,
                reply,
            } => {
                let answer = match &self.shown {
                    Some(shown) => {
                        let _ = shown
                            .window
                            .request_inner_size(LogicalSize::new(width, height));
                        Ok(())
                    }
                    None => Err(copy::CTL_NO_PICTURE.to_owned()),
                };
                let _ = reply.send(answer);
            }
        }
    }

    /// Clicks the window at `(x, y)` points through the same path the mouse takes: egui sees
    /// it first, and the guest gets it if it lands on the picture.
    fn click(&mut self, event_loop: &ActiveEventLoop, x: f64, y: f64) -> Result<(), String> {
        let Some(shown) = &self.shown else {
            return Err(copy::CTL_NO_PICTURE.to_owned());
        };
        let id = shown.window.id();
        let scale = shown.window.scale_factor();
        let device_id = DeviceId::dummy();
        let position = PhysicalPosition::new(x * scale, y * scale);
        let events = [
            WindowEvent::CursorMoved {
                device_id,
                position,
            },
            WindowEvent::MouseInput {
                device_id,
                state: ElementState::Pressed,
                button: MouseButton::Left,
            },
        ];
        for event in events {
            self.window_event(event_loop, id, event);
        }
        // A frame between press and release, and a hand's hold, so egui and Android see a click.
        self.draw();
        std::thread::sleep(crate::config::WINDOW_CLICK_HOLD);
        let release = WindowEvent::MouseInput {
            device_id,
            state: ElementState::Released,
            button: MouseButton::Left,
        };
        self.window_event(event_loop, id, release);
        self.draw();
        Ok(())
    }

    /// Turns the mouse wheel over `(x, y)` points, through the same path the mouse takes.
    fn scroll(
        &mut self,
        event_loop: &ActiveEventLoop,
        x: f64,
        y: f64,
        lines: f64,
    ) -> Result<(), String> {
        let Some(shown) = &self.shown else {
            return Err(copy::CTL_NO_PICTURE.to_owned());
        };
        let id = shown.window.id();
        let scale = shown.window.scale_factor();
        let device_id = DeviceId::dummy();
        let moved = WindowEvent::CursorMoved {
            device_id,
            position: PhysicalPosition::new(x * scale, y * scale),
        };
        self.window_event(event_loop, id, moved);
        let wheel = WindowEvent::MouseWheel {
            device_id,
            delta: MouseScrollDelta::LineDelta(0.0, lines as f32),
            phase: winit::event::TouchPhase::Moved,
        };
        self.window_event(event_loop, id, wheel);
        self.draw();
        Ok(())
    }

    /// Shuts Android down on a thread of its own, so the window keeps drawing meanwhile.
    fn shut_down(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.gui.set_status(copy::gui::SHUTTING_DOWN);
        let controller = Arc::clone(&self.controller);
        let started = std::thread::Builder::new()
            .name(crate::config::SHUTDOWN_THREAD.into())
            .spawn(move || controller.shut_down());
        if let Err(err) = started {
            log::error!("{}", copy::thread(&err));
            self.controller.power.off();
        }
    }

    /// Does what a shortcut stands for.
    fn shortcut(&mut self, shortcut: Shortcut) {
        match shortcut {
            Shortcut::Button(code) => self.send(Command::Key { code, down: None }),
            Shortcut::Turn(clockwise) => {
                let step = if clockwise { 1 } else { QUARTER_TURNS - 1 };
                self.device_turns = (self.device_turns + step) % QUARTER_TURNS;
                let degrees = f64::from(self.device_turns) * DEGREES_PER_TURN;
                // The sensors take the short way to this angle, so 0 to 270 is a quarter back.
                self.send(Command::Rotate([0.0, 0.0, degrees]));
            }
            Shortcut::Screenshot => self.gui.screenshot(),
        }
    }

    /// The width of the side panel now beside the picture, in logical pixels.
    fn panel_width(&self) -> f64 {
        if self.panel_shown {
            f64::from(PANEL_WIDTH)
        } else {
            0.0
        }
    }

    /// Android turned its screen: lift the finger, and give the window the picture's new shape at
    /// the same scale.
    fn rotated(&mut self, turns: u8) {
        self.touch(false);
        self.frame_waiting = true;
        self.turns = turns;
        let (Some(shown), Some((rect, drawn))) = (&self.shown, self.picture) else {
            return;
        };
        // Back to a way up the window has been before: the size it had then.
        if let Some(size) = self.sizes[usize::from(turns % 2)] {
            let wide = size.width + self.panel_width();
            let _ = shown
                .window
                .request_inner_size(LogicalSize::new(wide, size.height));
            shown.window.request_redraw();
            return;
        }
        // The size the picture was drawn at, not the guest's turn, which has already changed.
        let scale = f64::from(rect.width()) / f64::from(drawn.0.max(1));
        let full = orientation::upright_size(self.guest, turns);
        // Only the part Android draws its logical display into is shown.
        let new = self.logical.map_or(full, |natural| {
            let (_, _, width, height) =
                crate::fill::shown_rect(full, orientation::upright_size(natural, turns));
            (width, height)
        });
        let window = &shown.window;
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        let shown_width = self.shown_part.map_or(rect.width(), |part| part.width());
        let chrome = size.width - f64::from(shown_width);
        let monitor = window.current_monitor().map(|monitor| {
            let size = monitor.size().to_logical::<f64>(monitor.scale_factor());
            (size.width, size.height)
        });
        let (width, height) = turned_size(new, scale, chrome, monitor);
        let _ = window.request_inner_size(LogicalSize::new(width, height));
        window.request_redraw();
    }

    /// Changes to the key map for `package`, lifting the old map's fingers first.
    fn follow(&mut self, package: &str) {
        let Some(dir) = &self.profiles else {
            return;
        };
        let path = profiles::map_for(dir, package);
        eprintln!("{}", copy::keymap_switched(package, path.as_deref()));
        match path {
            Some(path) => self.use_keymap(Some(path)),
            None => self.use_keymap(None),
        }
    }

    /// Updates the toolbar's meters: frames shown, and the CPU and memory this process uses.
    fn count(&mut self) {
        let seconds = self.counted.elapsed().as_secs_f64();
        log::trace!("{}", copy::frame_rate(self.frames, self.counted.elapsed()));
        let now = crate::usage::read();
        self.gui.fps = Some(f64::from(self.frames) / seconds);
        self.gui.cpu = crate::usage::cpu_percent(&self.usage, &now);
        self.gui.low_fps = self.gaps.one_percent_low();
        self.gui.gpu = crate::usage::gpu_percent();
        self.gui.memory_bytes = now.memory_bytes;
        self.usage = now;
        self.frames = 0;
        self.counted = Instant::now();
        if let Some(shown) = &self.shown {
            shown.window.request_redraw();
        }
    }

    /// The mouse moved by this much, wherever the pointer is.
    fn mouse_moved(&mut self, dx: f64, dy: f64) {
        let touches = self.mapper.as_mut().filter(|map| map.wants_pointer());
        if let Some(touches) = touches.map(|map| map.pointer(dx, dy)) {
            self.apply(&touches);
        }
    }

    /// Lets go of everything held: the releases would go to another window.
    fn let_go(&mut self) {
        for code in std::mem::take(&mut self.held) {
            self.send(Command::Key {
                code,
                down: Some(false),
            });
        }
        if let Some(map) = self.mapper.as_mut() {
            let touches = map.release();
            self.apply(&touches);
        }
        self.touch(false);
        self.modifiers = ModifiersState::empty();
    }

    fn key(&mut self, key: KeyCode, down: bool) {
        if let Some(editor) = self.editor.as_mut()
            && editor.wants_key()
        {
            if down {
                editor.key(&key_name(key));
            }
            return;
        }
        if is_shortcut_modifier(key) {
            return;
        }
        if shortcut_held(self.modifiers)
            && let Some(shortcut) = shortcut(key, self.modifiers.shift_key())
        {
            if down {
                self.shortcut(shortcut);
            } else if let Some(code) = linux_key(key)
                && self.held.remove(&code)
            {
                // Pressed before the shortcut modifier: its release still goes to the guest.
                self.send(Command::Key {
                    code,
                    down: Some(false),
                });
            }
            return;
        }
        if let Some(code) = linux_key(key)
            && !self.mapped(code, down)
        {
            if down {
                self.held.insert(code);
            } else {
                self.held.remove(&code);
            }
            self.send(Command::Key {
                code,
                down: Some(down),
            });
        }
    }
}

impl ApplicationHandler<Notice> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.shown.is_some() {
            return;
        }
        let monitor = event_loop.primary_monitor().map(|monitor| {
            let size = monitor.size().to_logical::<f64>(monitor.scale_factor());
            (size.width, size.height)
        });
        let (width, height) = initial_size(self.upright(), monitor);
        let attributes = Window::default_attributes()
            .with_title(copy::WINDOW_TITLE)
            .with_window_icon(crate::app_icon())
            .with_inner_size(LogicalSize::new(width, height));
        let shown = event_loop
            .create_window(attributes)
            .map_err(|err| err.to_string())
            .and_then(|window| {
                let window = Arc::new(window);
                let painter = Painter::new(event_loop, &window)?;
                crate::style::apply(&painter.ctx);
                Ok(Shown { window, painter })
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
                self.frame_waiting = true;
                self.frames += 1;
                self.gaps.frame(Instant::now());
            }
            Notice::Stopped => event_loop.exit(),
            Notice::Foreground(package) => self.follow(&package),
            Notice::Rotated(turns) => self.rotated(turns),
            notice @ (Notice::Status(_) | Notice::Packages(_)) => self.gui.notice(notice),
            Notice::Request(request) => self.request(event_loop, request),
            Notice::Filled { wanted, got } => {
                self.asking = false;
                if let Some(got) = got {
                    self.logical = got;
                    self.synced = true;
                    self.frame_waiting = true;
                }
                if got != Some(wanted) {
                    self.resized = Some(Instant::now() + crate::config::FILL_RETRY);
                }
            }
        }
        if let Some(shown) = &self.shown {
            shown.window.request_redraw();
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            self.mouse_moved(dx, dy);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.shown.is_some() && self.counted.elapsed() >= crate::config::FRAME_RATE_PERIOD {
            self.count();
        }
        let Some(shown) = &self.shown else {
            return;
        };
        let next_count = self.counted + crate::config::FRAME_RATE_PERIOD;
        let mut wake = Some(
            shown
                .painter
                .repaint_at
                .map_or(next_count, |at| at.min(next_count)),
        );
        if let Some(at) = shown.painter.repaint_at
            && at <= Instant::now()
        {
            shown.window.request_redraw();
            wake = None;
        }
        if let Some(resized) = self.resized {
            let settled = resized + crate::config::FILL_SETTLE;
            if settled <= Instant::now() {
                self.resized = None;
                self.fill();
            } else {
                wake = Some(wake.map_or(settled, |at| at.min(settled)));
            }
        }
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // The controls see every event; what happens over the picture belongs to the guest.
        let used = match self.shown.as_mut() {
            Some(shown) => shown.painter.on_event(&shown.window, &event),
            None => false,
        };
        let typing = self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.painter.ctx.egui_wants_keyboard_input());
        match event {
            // The machine is switched off; the window closes when it has stopped.
            // Android shuts down first; the window closes when the machine has stopped. A second
            // close pulls the plug.
            WindowEvent::CloseRequested if self.shutting_down => self.controller.power.off(),
            WindowEvent::CloseRequested => self.shut_down(),
            WindowEvent::RedrawRequested => self.draw(),
            // The picture moves when the window changes; draw now so clicks use where it is.
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(shown) = &self.shown {
                    let window = &shown.window;
                    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
                    if let Some(size) = remembered_size(size, self.panel_width()) {
                        self.sizes[usize::from(self.turns % 2)] = Some(size);
                    }
                }
                self.touch(false);
                self.draw();
                self.resized = Some(Instant::now());
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = position;
                if self.touching {
                    self.touch(true);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                let over_picture = self.pointer_pixel().is_some();
                if self.editor.is_some() {
                    return;
                }
                let index = match button {
                    MouseButton::Left => Some(0),
                    MouseButton::Right => Some(1),
                    MouseButton::Middle => Some(2),
                    _ => None,
                };
                let taken = (over_picture || self.grabbed || !pressed)
                    && index
                        .and_then(button_code)
                        .is_some_and(|code| self.mapped(code, pressed));
                // The left button is the finger, unless the key map has the mouse or the button.
                if !taken && !self.grabbed && button == MouseButton::Left {
                    if pressed && !over_picture {
                        return;
                    }
                    self.touch(pressed);
                }
            }
            WindowEvent::CursorLeft { .. } => self.touch(false),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if !focused {
                    self.let_go();
                }
                self.sync_pointer();
            }
            // An install takes a while; the window keeps drawing meanwhile.
            WindowEvent::DroppedFile(path) => {
                let controller = Arc::clone(&self.controller);
                self.gui.set_status(copy::gui::WORKING);
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
            WindowEvent::MouseWheel { delta, .. } if !used => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => Scroll::Turns(f64::from(lines)),
                    MouseScrollDelta::PixelDelta(moved) => Scroll::Pixels(moved.y),
                };
                if let Some(map) = self.mapper.as_mut() {
                    let touches = map.scroll(scroll);
                    self.apply(&touches);
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } if !event.repeat => {
                let PhysicalKey::Code(key) = event.physical_key else {
                    return;
                };
                let down = event.state == ElementState::Pressed;
                // A release of a key the guest has down always goes, even into a text field.
                let owed = !down && linux_key(key).is_some_and(|code| self.held.contains(&code));
                if typing && !owed {
                    return;
                }
                self.key(key, down);
            }
            _ => {}
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
    /// Returns what the user asked for as the window closed.
    pub fn run(
        self,
        controller: Arc<Controller>,
        guest: (u32, u32),
        keymap: Option<PathBuf>,
        profiles: Option<PathBuf>,
        instance: Option<String>,
    ) -> Result<crate::Exit, VmmError> {
        let notify = Arc::new(self.notifier());
        let link = self.notifier();
        *controller
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Box::new(move |request| link(Notice::Request(request))));
        let mapper = keymap
            .as_ref()
            .map(|path| keymap::load(path, orientation::any_way_up(guest)))
            .transpose()
            .map_err(VmmError::Device)?;
        let fill_notify: Arc<dyn Fn(Notice) + Send + Sync> = Arc::new(self.notifier());
        let gui = Gui::new(Arc::clone(&controller), notify, instance, keymap.as_deref());
        let mut app = App {
            controller,
            guest,
            shown: None,
            gui,
            picture_texture: Picture::default(),
            frame_waiting: true,
            picture: None,
            shown_part: None,
            space: None,
            logical: None,
            synced: false,
            asking: false,
            // Android may still have a shape from an earlier run; the window finds out once it
            // shows something, and gives Android its own.
            resized: Some(Instant::now()),
            notify: fill_notify,
            pointer: PhysicalPosition::default(),
            touching: false,
            mapper,
            profiles,
            editor: None,
            grabbed: false,
            failed: None,
            modifiers: ModifiersState::empty(),
            held: HashSet::new(),
            device_turns: 0,
            exit: crate::Exit::Closed,
            panel_shown: false,
            focused: true,
            turns: 0,
            sizes: [None; 2],
            usage: crate::usage::read(),
            shutting_down: false,
            frames: 0,
            counted: Instant::now(),
            gaps: crate::usage::FrameGaps::default(),
        };
        let ran = self.0.run_app(&mut app);
        // The GPU objects go before the window they draw into.
        let render = app
            .shown
            .as_ref()
            .and_then(|shown| shown.painter.render_state());
        app.picture_texture.release(render.as_ref());
        app.shown = None;
        *app.controller
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        match (app.failed, ran) {
            (Some(why), _) => Err(VmmError::Device(copy::window_failed(&why))),
            (None, Err(err)) => Err(VmmError::Device(copy::window_failed(&err))),
            (None, Ok(())) => Ok(app.exit),
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
        let space = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 360.0));
        let rect = fit(space, (1280, 720)).unwrap();
        assert_eq!(rect.min, egui::pos2(180.0, 0.0));
        assert_eq!(rect.size(), egui::vec2(640.0, 360.0));
        let tall = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 1000.0));
        let rect = fit(tall, (1280, 720)).unwrap();
        assert_eq!(rect.min, egui::pos2(0.0, 320.0));
        assert!(fit(space, (0, 720)).is_none());
    }

    #[test]
    fn pointer_maps_to_the_guest_pixel_under_it_and_to_nothing_on_the_bars() {
        let rect = egui::Rect::from_min_size(egui::pos2(180.0, 0.0), egui::vec2(640.0, 360.0));
        let picture = (1280, 720);
        assert_eq!(
            guest_pixel(rect, picture, egui::pos2(180.0, 0.0)),
            Some((0, 0))
        );
        assert_eq!(
            guest_pixel(rect, picture, egui::pos2(819.9, 359.9)),
            Some((1279, 719))
        );
        assert_eq!(
            guest_pixel(rect, picture, egui::pos2(500.0, 180.0)),
            Some((640, 360))
        );
        assert_eq!(guest_pixel(rect, picture, egui::pos2(179.9, 10.0)), None);
        assert_eq!(guest_pixel(rect, picture, egui::pos2(820.1, 10.0)), None);
    }

    #[test]
    fn frame_becomes_an_upright_rgba_image() {
        // A 2x1 panel, red then blue, in a layout with blue first.
        let pixels = [0, 0, 255, 0, 255, 0, 0, 0];
        let frame = Frame {
            width: 2,
            height: 1,
            format: FORMAT_BGRX,
            pixels: &pixels,
        };
        let image = color_image(&frame, 0).unwrap();
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels, [egui::Color32::RED, egui::Color32::BLUE]);
        // A quarter turn: the panel's right end is the top.
        let image = color_image(&frame, 1).unwrap();
        assert_eq!(image.size, [1, 2]);
        assert_eq!(image.pixels, [egui::Color32::BLUE, egui::Color32::RED]);
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
        let image = color_image(&frame, 0).unwrap();
        assert_eq!(image.pixels, [egui::Color32::from_rgb(10, 20, 30)]);
    }

    #[test]
    fn frames_of_the_wrong_size_are_left_alone() {
        let pixels = [0u8; 4];
        let frame = Frame {
            width: 2,
            height: 1,
            format: FORMAT_RGBA,
            pixels: &pixels,
        };
        assert!(color_image(&frame, 0).is_none());
    }

    #[test]
    fn first_window_fits_the_monitor_and_never_grows_the_picture() {
        let share = crate::config::WINDOW_SCREEN_SHARE;
        let toolbar = f64::from(TOOLBAR_WIDTH);
        let (width, height) = initial_size((720, 1280), Some((1440.0, 900.0)));
        assert!((height - 900.0 * share).abs() < 1e-6);
        assert!(((width - toolbar) / height - 720.0 / 1280.0).abs() < 1e-6);
        assert_eq!(
            initial_size((720, 1280), Some((4000.0, 4000.0))),
            (720.0 + toolbar, 1280.0)
        );
    }

    #[test]
    fn a_turned_picture_keeps_its_scale_unless_the_monitor_is_too_small() {
        let toolbar = f64::from(TOOLBAR_WIDTH);
        // A 1080x1920 phone drawn at 0.6375 turns landscape: same scale, wide window.
        let (width, height) = turned_size((1920, 1080), 0.6375, toolbar, Some((2560.0, 1440.0)));
        assert!((width - (1920.0 * 0.6375 + toolbar)).abs() < 1e-6);
        assert!((height - 1080.0 * 0.6375).abs() < 1e-6);
        // On a small monitor it shrinks to fit.
        let share = crate::config::WINDOW_SCREEN_SHARE;
        let (width, _) = turned_size((1920, 1080), 0.6375, toolbar, Some((1280.0, 800.0)));
        assert!(width <= 1280.0 * share + 1e-6);
    }

    #[test]
    fn shortcuts_press_androids_buttons_and_leave_other_keys_alone() {
        use crate::input::{KEY_APPSELECT, KEY_BACK, KEY_HOMEPAGE};
        assert_eq!(
            shortcut(KeyCode::Backspace, false),
            Some(Shortcut::Button(KEY_BACK))
        );
        assert_eq!(
            shortcut(KeyCode::KeyH, true),
            Some(Shortcut::Button(KEY_HOMEPAGE))
        );
        // Without shift, ⌘H stays the Mac's "hide".
        assert_eq!(shortcut(KeyCode::KeyH, false), None);
        assert_eq!(
            shortcut(KeyCode::KeyO, false),
            Some(Shortcut::Button(KEY_APPSELECT))
        );
        assert_eq!(
            shortcut(KeyCode::ArrowRight, false),
            Some(Shortcut::Turn(true))
        );
        // ⌘Q quits the Mac way and is not taken.
        assert_eq!(shortcut(KeyCode::KeyQ, false), None);
        for key in [KEY_BACK, KEY_HOMEPAGE, KEY_APPSELECT] {
            assert!(crate::input::is_key(key), "{key}");
        }
    }

    #[test]
    fn remembered_window_size_leaves_out_the_panel_and_ignores_a_minimized_window() {
        let panel = f64::from(PANEL_WIDTH);
        let wide = LogicalSize::new(400.0 + panel, 800.0);
        assert_eq!(
            remembered_size(wide, panel),
            Some(LogicalSize::new(400.0, 800.0))
        );
        assert_eq!(remembered_size(LogicalSize::new(0.0, 0.0), 0.0), None);
        assert_eq!(remembered_size(LogicalSize::new(panel, 800.0), panel), None);
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
