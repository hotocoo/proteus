//! The controls around the guest's picture: a toolbar with Android's buttons, and panels for apps,
//! location, sensors, macros, key maps, capture and settings. Every control is a front end to the
//! control socket's commands (`ctl::Command`), so each can also be done with `emu ctl`.
//!
//! Work that takes a while (installing, the guest's shell, recording) runs on its own thread and
//! reports back with a `Notice`, so the window keeps drawing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{RichText, Ui};
use serde::{Deserialize, Serialize};

use crate::config::{self, ROUTE_DEFAULT_KMH};
use crate::copy::gui as text;
use crate::ctl::{Command, Controller};
use crate::input::{
    KEY_APPSELECT, KEY_BACK, KEY_HOMEPAGE, KEY_POWER, KEY_VOLUMEDOWN, KEY_VOLUMEUP,
};
use crate::settings_ui::{Action, Form};
use crate::window::Notice;
use crate::{adb, instance};

/// Something the window, not the controls, has to do.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Use this key map from now on, or none.
    LoadKeymap(Option<PathBuf>),
    /// Start editing key map positions on the picture.
    EditKeymap,
    /// Stop editing; use the map saved at this path, if it was saved.
    CloseEditor(Option<PathBuf>),
    /// Switch the machine off and start this instance again.
    Restart,
    /// Switch the machine off, delete its Android data and start it again as a new phone.
    FactoryReset,
    /// Turn the device a quarter turn, clockwise when true.
    Turn(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Apps,
    Location,
    Sensors,
    Macros,
    Keys,
    Capture,
    Settings,
}

const PANELS: [(Panel, &str); 7] = [
    (Panel::Apps, text::APPS),
    (Panel::Location, text::LOCATION),
    (Panel::Sensors, text::SENSORS),
    (Panel::Macros, text::MACROS),
    (Panel::Keys, text::KEYS),
    (Panel::Capture, text::CAPTURE),
    (Panel::Settings, text::SETTINGS),
];

/// Guest shell commands the apps panel runs.
const LIST_PACKAGES: &str = "pm list packages -3";
/// Stops every app's background processes, which frees their memory.
const KILL_BACKGROUND: &str = "am kill-all";
/// Starts an app's main activity: `monkey -p <package> -c <category> 1`.
const LAUNCH_PREFIX: &str = "monkey -p";
const LAUNCH_SUFFIX: &str = "-c android.intent.category.LAUNCHER 1";
const FORCE_STOP: &str = "am force-stop";
const CLEAR_DATA: &str = "pm clear";
const UNINSTALL: &str = "pm uninstall";
/// Android's game mode: the app draws at a share of the screen's size and Android scales it up,
/// which lightens the GPU's work. Followed by the share (or `disable`) and the package. Stopping
/// the app makes its next start use it.
const GAME_DOWNSCALE: &str = "cmd game set --downscale";
const GAME_DOWNSCALE_OFF: &str = "disable";

/// How the device can be held, as rotations about its x, y and z axes in degrees, in the order of
/// `text::POSES`.
const DEVICE_POSES: [[f64; 3]; 4] = [
    [0.0, 0.0, 0.0],
    [0.0, 0.0, 90.0],
    [0.0, 0.0, -90.0],
    [-90.0, 0.0, 0.0],
];

/// A place kept in the saved-places file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Default, Serialize, Deserialize)]
struct PlacesFile {
    #[serde(default)]
    place: Vec<Place>,
}

/// Reads saved places; a missing or unreadable file is no places.
pub fn read_places(path: &Path) -> Vec<Place> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<PlacesFile>(&text).ok())
        .map(|file| file.place)
        .unwrap_or_default()
}

pub fn write_places(path: &Path, places: &[Place]) -> std::io::Result<()> {
    let file = PlacesFile {
        place: places.to_vec(),
    };
    let text = toml::to_string(&file).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

/// Package names in what `pm list packages` printed.
pub fn packages(printed: &str) -> Vec<String> {
    const PREFIX: &str = "package:";
    let mut names: Vec<String> = printed
        .lines()
        .filter_map(|line| line.trim().strip_prefix(PREFIX))
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// The macro files in `dir`, by name.
fn macro_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(config::MACRO_EXTENSION))
        .collect();
    names.sort();
    names
}

/// Where the emulator keeps files of its own (saved places, macros made without an instance).
fn home() -> Option<PathBuf> {
    instance::root()
        .ok()
        .and_then(|root| root.parent().map(Path::to_path_buf))
}

/// A file name the user typed, made safe to use inside a folder: no folders, no hidden files.
pub fn file_name(typed: &str, extension: &str) -> Option<String> {
    let name = typed.trim();
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.'));
    plain.then(|| {
        if name.ends_with(extension) {
            name.to_owned()
        } else {
            format!("{name}{extension}")
        }
    })
}

#[derive(Default)]
struct Apps {
    install_path: String,
    packages: Vec<String>,
    /// The package whose uninstall waits for a second click.
    confirm_uninstall: Option<String>,
}

struct Location {
    latitude: String,
    longitude: String,
    link: String,
    place_name: String,
    places: Vec<Place>,
    gpx: String,
    route_kmh: f64,
    walk_bearing: f64,
    walk_kmh: f64,
}

struct Macros {
    dir: PathBuf,
    files: Vec<String>,
    selected: Option<String>,
    text: String,
    new_name: String,
    times: u32,
    recording: bool,
}

struct Capture {
    seconds: u32,
}

pub struct Gui {
    controller: Arc<Controller>,
    notify: Arc<dyn Fn(Notice) + Send + Sync>,
    /// The instance this window belongs to, if it was started as one.
    instance: Option<String>,
    pub open: Option<Panel>,
    status: String,
    /// Frames the guest showed per second, as last counted.
    pub fps: Option<f64>,
    apps: Apps,
    location: Location,
    sensors: [f64; 3],
    macros: Macros,
    keymap_path: String,
    capture: Capture,
    form: Option<Form>,
    save_as: String,
}

impl Gui {
    pub fn new(
        controller: Arc<Controller>,
        notify: Arc<dyn Fn(Notice) + Send + Sync>,
        instance: Option<String>,
        keymap: Option<&Path>,
    ) -> Self {
        let instance_dir = instance
            .as_deref()
            .and_then(|name| instance::dir(&instance::root().ok()?, name).ok());
        let macro_dir = instance_dir
            .or_else(home)
            .unwrap_or_default()
            .join(config::MACROS_DIR);
        let places = home().map(|home| read_places(&home.join(config::PLACES_FILE)));
        Self {
            controller,
            notify,
            instance,
            open: None,
            status: String::new(),
            fps: None,
            apps: Apps::default(),
            location: Location {
                latitude: String::new(),
                longitude: String::new(),
                link: String::new(),
                place_name: String::new(),
                places: places.unwrap_or_default(),
                gpx: String::new(),
                route_kmh: ROUTE_DEFAULT_KMH,
                walk_bearing: 0.0,
                walk_kmh: ROUTE_DEFAULT_KMH,
            },
            sensors: [0.0; 3],
            macros: Macros {
                files: macro_files(&macro_dir),
                dir: macro_dir,
                selected: None,
                text: String::new(),
                new_name: String::new(),
                times: 1,
                recording: false,
            },
            keymap_path: keymap
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            capture: Capture {
                seconds: config::CAPTURE_DEFAULT_SECONDS,
            },
            form: None,
            save_as: String::new(),
        }
    }

    /// News for the controls from a thread that finished.
    pub fn notice(&mut self, notice: Notice) {
        match notice {
            Notice::Status(status) => self.status = status,
            Notice::Packages(packages) => {
                self.status = text::packages_listed(packages.len());
                self.apps.packages = packages;
            }
            _ => {}
        }
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    /// Runs a quick command now and shows what went wrong, if anything.
    fn run(&mut self, command: Command) {
        if let Err(why) = self.controller.execute(&command) {
            self.status = why;
        }
    }

    /// Runs a command on its own thread; `done` is shown when it worked.
    fn spawn(&mut self, command: Command, done: String) {
        let controller = Arc::clone(&self.controller);
        let notify = Arc::clone(&self.notify);
        self.status = text::WORKING.to_owned();
        let started = std::thread::Builder::new()
            .name(config::GUI_TASK_THREAD.into())
            .spawn(move || {
                let status = match controller.execute(&command) {
                    Ok(()) => done,
                    Err(why) => why,
                };
                notify(Notice::Status(status));
            });
        if let Err(err) = started {
            self.status = err.to_string();
        }
    }

    /// Runs `command` in the guest's shell on its own thread; `done` turns what it printed into
    /// news for the controls.
    fn shell(&mut self, command: String, done: impl FnOnce(String) -> Notice + Send + 'static) {
        let controller = Arc::clone(&self.controller);
        let notify = Arc::clone(&self.notify);
        self.status = text::WORKING.to_owned();
        let started = std::thread::Builder::new()
            .name(config::GUI_TASK_THREAD.into())
            .spawn(move || {
                notify(match controller.shell(&command) {
                    Ok(printed) => done(printed),
                    Err(why) => Notice::Status(why),
                });
            });
        if let Err(err) = started {
            self.status = err.to_string();
        }
    }

    fn key(&mut self, code: u16) {
        self.run(Command::Key { code, down: None });
    }

    /// The toolbar down the window's right edge.
    pub fn toolbar(&mut self, ui: &mut Ui, effects: &mut Vec<Effect>) {
        let width = config::TOOLBAR_WIDTH;
        let size = [
            width - 2.0 * config::TOOLBAR_MARGIN,
            config::TOOLBAR_BUTTON_HEIGHT,
        ];
        ui.vertical_centered_justified(|ui| {
            if let Some(fps) = self.fps {
                ui.label(text::fps(fps)).on_hover_text(text::FPS_HINT);
            }
            let button = |ui: &mut Ui, label: &str, hint: &str| {
                ui.add_sized(size, egui::Button::new(label))
                    .on_hover_text(hint)
                    .clicked()
            };
            if button(ui, text::BACK, text::BACK_HINT) {
                self.key(KEY_BACK);
            }
            if button(ui, text::HOME, text::HOME_HINT) {
                self.key(KEY_HOMEPAGE);
            }
            if button(ui, text::RECENTS, text::RECENTS_HINT) {
                self.key(KEY_APPSELECT);
            }
            ui.separator();
            if button(ui, text::POWER, text::POWER_HINT) {
                self.key(KEY_POWER);
            }
            if button(ui, text::VOLUME_UP, text::VOLUME_UP_HINT) {
                self.key(KEY_VOLUMEUP);
            }
            if button(ui, text::VOLUME_DOWN, text::VOLUME_DOWN_HINT) {
                self.key(KEY_VOLUMEDOWN);
            }
            if button(ui, text::TURN_LEFT, text::TURN_HINT) {
                effects.push(Effect::Turn(false));
            }
            if button(ui, text::TURN_RIGHT, text::TURN_HINT) {
                effects.push(Effect::Turn(true));
            }
            if button(ui, text::SCREENSHOT, text::SCREENSHOT_HINT) {
                self.screenshot();
            }
            ui.separator();
            for (panel, label) in PANELS {
                let open = self.open == Some(panel);
                let clicked = ui
                    .add_sized(size, egui::Button::new(label).selected(open))
                    .clicked();
                if clicked {
                    self.open = (!open).then_some(panel);
                    if self.open == Some(Panel::Settings) {
                        self.load_settings();
                    }
                }
            }
        });
    }

    /// The status line under the picture. Nothing when there is nothing to say.
    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn screenshot(&mut self) {
        let path = config::screenshot_path();
        match self.controller.execute(&Command::Screenshot(path.clone())) {
            Ok(()) => self.status = text::saved(&path),
            Err(why) => self.status = why,
        }
    }

    /// The open panel's controls.
    /// Returns how wide its content was laid out, which a test holds to the panel's width.
    pub fn panel(&mut self, ui: &mut Ui, effects: &mut Vec<Effect>) -> f32 {
        let Some(panel) = self.open else {
            return 0.0;
        };
        // Text fields beside a label or a button get what is left of the panel, not egui's
        // default width, which is wider than the panel can give.
        let room = ui.available_width() - config::PANEL_LABEL_ROOM;
        ui.spacing_mut().text_edit_width = room.max(config::PANEL_FIELD_MIN);
        let shown = egui::ScrollArea::vertical().show(ui, |ui| match panel {
            Panel::Apps => self.apps_panel(ui),
            Panel::Location => self.location_panel(ui),
            Panel::Sensors => self.sensors_panel(ui),
            Panel::Macros => self.macros_panel(ui),
            Panel::Keys => self.keys_panel(ui, effects),
            Panel::Capture => self.capture_panel(ui),
            Panel::Settings => self.settings_panel(ui, effects),
        });
        shown.content_size.x
    }

    fn apps_panel(&mut self, ui: &mut Ui) {
        ui.heading(text::APPS);
        ui.label(text::INSTALL_HINT);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.apps.install_path)
                    .hint_text(text::INSTALL_PATH_HINT)
                    .desired_width(f32::INFINITY),
            )
            .accessible_name(text::INSTALL_PATH_NAME);
        });
        if ui.button(text::INSTALL).clicked() {
            let path = PathBuf::from(self.apps.install_path.trim());
            self.spawn(Command::Install(path), text::INSTALLED.to_owned());
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(text::LIST_APPS).clicked() {
                self.shell(LIST_PACKAGES.to_owned(), |printed| {
                    Notice::Packages(packages(&printed))
                });
            }
            if ui
                .button(text::CLOSE_BACKGROUND)
                .on_hover_text(text::CLOSE_BACKGROUND_HINT)
                .clicked()
            {
                self.shell(KILL_BACKGROUND.to_owned(), |_| {
                    Notice::Status(text::BACKGROUND_CLOSED.to_owned())
                });
            }
        });
        let packages = self.apps.packages.clone();
        for package in packages {
            ui.separator();
            ui.label(RichText::new(&package).monospace());
            ui.horizontal_wrapped(|ui| {
                let quoted = adb::quote(&package);
                if ui.button(text::OPEN).clicked() {
                    self.shell(
                        format!("{} {quoted} {}", LAUNCH_PREFIX, LAUNCH_SUFFIX),
                        |_| Notice::Status(String::new()),
                    );
                }
                if ui.button(text::FORCE_STOP).clicked() {
                    let done = text::stopped(&package);
                    self.shell(format!("{} {quoted}", FORCE_STOP), move |_| {
                        Notice::Status(done)
                    });
                }
                ui.label(text::RENDER_SCALE)
                    .on_hover_text(text::RENDER_SCALE_HINT);
                for scale in config::RENDER_SCALES {
                    if ui.button(text::render_scale(scale)).clicked() {
                        let share = if scale >= 1.0 {
                            GAME_DOWNSCALE_OFF.to_owned()
                        } else {
                            scale.to_string()
                        };
                        let done = text::render_scale_set(&package, scale);
                        self.shell(
                            format!("{GAME_DOWNSCALE} {share} {quoted} && {FORCE_STOP} {quoted}"),
                            move |printed| {
                                let printed = printed.trim();
                                Notice::Status(if printed.is_empty() {
                                    done
                                } else {
                                    printed.to_owned()
                                })
                            },
                        );
                    }
                }
                if ui.button(text::CLEAR_DATA).clicked() {
                    self.shell(format!("{} {quoted}", CLEAR_DATA), |printed| {
                        Notice::Status(printed.trim().to_owned())
                    });
                }
                let confirming = self.apps.confirm_uninstall.as_deref() == Some(&package);
                let label = if confirming {
                    text::UNINSTALL_SURE
                } else {
                    text::UNINSTALL
                };
                if ui.button(label).clicked() {
                    if confirming {
                        self.apps.confirm_uninstall = None;
                        self.apps.packages.retain(|name| *name != package);
                        self.shell(format!("{} {quoted}", UNINSTALL), |printed| {
                            Notice::Status(printed.trim().to_owned())
                        });
                    } else {
                        self.apps.confirm_uninstall = Some(package.clone());
                    }
                }
            });
        }
    }

    fn location_panel(&mut self, ui: &mut Ui) {
        let location = &mut self.location;
        ui.heading(text::LOCATION);
        ui.label(text::LOCATION_HINT);
        let mut command = None;
        egui::Grid::new("coordinates")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label(text::LATITUDE);
                ui.text_edit_singleline(&mut location.latitude)
                    .accessible_name(text::LATITUDE);
                ui.end_row();
                ui.label(text::LONGITUDE);
                ui.text_edit_singleline(&mut location.longitude)
                    .accessible_name(text::LONGITUDE);
                ui.end_row();
            });
        if ui.button(text::SET_LOCATION).clicked() {
            let line = format!(
                "{} {} {}",
                crate::ctl::WORD_LOCATION,
                location.latitude.trim(),
                location.longitude.trim()
            );
            command = Some(crate::ctl::parse(&line).ok_or(text::BAD_COORDINATES));
        }
        ui.add(
            egui::TextEdit::singleline(&mut location.link)
                .hint_text(text::MAP_LINK_HINT)
                .desired_width(f32::INFINITY),
        )
        .accessible_name(text::MAP_LINK_NAME);
        if ui.button(text::SET_FROM_LINK).clicked() {
            let line = format!("{} {}", crate::ctl::WORD_LOCATION, location.link.trim());
            command = Some(crate::ctl::parse(&line).ok_or(text::BAD_LINK));
        }
        ui.separator();
        ui.label(text::SAVED_PLACES);
        let mut remove = None;
        for (index, place) in location.places.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.button(&place.name).clicked() {
                    command = Some(Ok(Command::Location {
                        latitude: place.latitude,
                        longitude: place.longitude,
                        altitude: 0.0,
                    }));
                }
                if ui.small_button(text::FORGET).clicked() {
                    remove = Some(index);
                }
            });
        }
        let mut changed = false;
        if let Some(index) = remove {
            location.places.remove(index);
            changed = true;
        }
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut location.place_name)
                .accessible_name(text::PLACE_NAME);
            let coordinates = (
                location.latitude.trim().parse::<f64>(),
                location.longitude.trim().parse::<f64>(),
            );
            if ui.button(text::SAVE_PLACE).clicked()
                && let (Ok(latitude), Ok(longitude)) = coordinates
                && !location.place_name.trim().is_empty()
            {
                location.places.push(Place {
                    name: location.place_name.trim().to_owned(),
                    latitude,
                    longitude,
                });
                location.place_name.clear();
                changed = true;
            }
        });
        if changed && let Some(home) = home() {
            let saved = write_places(&home.join(config::PLACES_FILE), &location.places);
            if let Err(err) = saved {
                self.status = err.to_string();
            }
        }
        ui.separator();
        ui.label(text::ROUTE);
        ui.add(
            egui::TextEdit::singleline(&mut location.gpx)
                .hint_text(text::GPX_HINT)
                .desired_width(f32::INFINITY),
        )
        .accessible_name(text::ROUTE);
        ui.add(egui::Slider::new(&mut location.route_kmh, config::KMH_RANGE).text(text::KMH));
        if ui.button(text::FOLLOW_ROUTE).clicked() {
            command = Some(Ok(Command::Route {
                path: PathBuf::from(location.gpx.trim()),
                km_per_hour: location.route_kmh,
            }));
        }
        ui.separator();
        ui.label(text::WALK);
        ui.add(
            egui::Slider::new(&mut location.walk_bearing, config::BEARING_RANGE)
                .text(text::BEARING),
        );
        ui.add(egui::Slider::new(&mut location.walk_kmh, config::KMH_RANGE).text(text::KMH));
        ui.horizontal(|ui| {
            if ui.button(text::WALK_START).clicked() {
                command = Some(Ok(Command::Walk(Some((
                    location.walk_bearing,
                    location.walk_kmh,
                )))));
            }
            if ui.button(text::WALK_STOP).clicked() {
                command = Some(Ok(Command::Walk(None)));
            }
        });
        match command {
            Some(Ok(command)) => {
                let slow = matches!(command, Command::Route { .. });
                if slow {
                    self.spawn(command, text::ROUTE_DONE.to_owned());
                } else {
                    self.run(command);
                }
            }
            Some(Err(why)) => self.status = why.to_owned(),
            None => {}
        }
    }

    fn sensors_panel(&mut self, ui: &mut Ui) {
        ui.heading(text::SENSORS);
        ui.label(text::SENSORS_HINT);
        let mut changed = false;
        for (value, label) in self.sensors.iter_mut().zip(text::AXES) {
            changed |= ui
                .add(egui::Slider::new(value, config::DEGREES_RANGE).text(label))
                .changed();
        }
        ui.horizontal_wrapped(|ui| {
            for (pose, name) in DEVICE_POSES.iter().zip(text::POSES) {
                if ui.button(name).clicked() {
                    self.sensors = *pose;
                    changed = true;
                }
            }
        });
        if changed {
            self.run(Command::Rotate(self.sensors));
        }
        if ui.button(text::SHAKE).clicked() {
            self.run(Command::Shake);
        }
    }

    fn macros_panel(&mut self, ui: &mut Ui) {
        ui.heading(text::MACROS);
        ui.label(text::macros_hint(&self.macros.dir));
        let mut open = None;
        for name in &self.macros.files {
            let selected = self.macros.selected.as_deref() == Some(name);
            if ui.selectable_label(selected, name).clicked() {
                open = Some(name.clone());
            }
        }
        if let Some(name) = open {
            self.macros.text =
                std::fs::read_to_string(self.macros.dir.join(&name)).unwrap_or_default();
            self.macros.selected = Some(name);
        }
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.macros.new_name)
                .accessible_name(text::MACRO_NAME);
            if ui.button(text::NEW_MACRO).clicked() {
                match file_name(&self.macros.new_name, config::MACRO_EXTENSION) {
                    Some(name) => {
                        self.macros.selected = Some(name);
                        self.macros.text = text::NEW_MACRO_TEMPLATE.to_owned();
                        self.macros.new_name.clear();
                    }
                    None => self.status = text::BAD_FILE_NAME.to_owned(),
                }
            }
        });
        let Some(name) = self.macros.selected.clone() else {
            return;
        };
        let path = self.macros.dir.join(&name);
        ui.separator();
        ui.label(RichText::new(&name).strong());
        ui.add(
            egui::TextEdit::multiline(&mut self.macros.text)
                .code_editor()
                .desired_rows(config::MACRO_EDITOR_ROWS)
                .desired_width(f32::INFINITY),
        )
        .accessible_name(text::MACRO_TEXT_NAME);
        ui.horizontal_wrapped(|ui| {
            if ui.button(text::SAVE).clicked() {
                let saved = std::fs::create_dir_all(&self.macros.dir)
                    .and_then(|()| std::fs::write(&path, &self.macros.text));
                self.status = match saved {
                    Ok(()) => text::saved(&path),
                    Err(err) => err.to_string(),
                };
                self.macros.files = macro_files(&self.macros.dir);
            }
            ui.add(egui::DragValue::new(&mut self.macros.times).range(config::MACRO_TIMES_RANGE))
                .accessible_name(text::TIMES);
            if ui.button(text::PLAY).clicked() {
                let command = Command::Run {
                    path: path.clone(),
                    times: self.macros.times,
                };
                self.spawn(command, text::MACRO_DONE.to_owned());
            }
            if ui.button(text::CANCEL).clicked() {
                self.run(Command::Cancel);
            }
        });
        ui.horizontal(|ui| {
            if !self.macros.recording && ui.button(text::RECORD).clicked() {
                let made = std::fs::create_dir_all(&self.macros.dir);
                match made {
                    Ok(()) => {
                        self.run(Command::Record(path.clone()));
                        self.macros.recording = true;
                        self.status = text::RECORDING.to_owned();
                    }
                    Err(err) => self.status = err.to_string(),
                }
            }
            if self.macros.recording && ui.button(text::STOP_RECORDING).clicked() {
                self.run(Command::RecordStop);
                self.macros.recording = false;
                self.macros.text = std::fs::read_to_string(&path).unwrap_or_default();
                self.macros.files = macro_files(&self.macros.dir);
            }
        });
    }

    fn keys_panel(&mut self, ui: &mut Ui, effects: &mut Vec<Effect>) {
        ui.heading(text::KEYS);
        ui.label(text::KEYS_HINT);
        ui.add(
            egui::TextEdit::singleline(&mut self.keymap_path)
                .hint_text(text::KEYMAP_PATH_HINT)
                .desired_width(f32::INFINITY),
        )
        .accessible_name(text::KEYMAP_FILE);
        ui.horizontal_wrapped(|ui| {
            if ui.button(text::USE_KEYMAP).clicked() {
                let path = self.keymap_path.trim();
                effects.push(Effect::LoadKeymap(
                    (!path.is_empty()).then(|| PathBuf::from(path)),
                ));
            }
            if ui.button(text::NO_KEYMAP).clicked() {
                effects.push(Effect::LoadKeymap(None));
            }
            if ui.button(text::EDIT_KEYMAP).clicked() {
                effects.push(Effect::EditKeymap);
            }
        });
    }

    /// The key map file the panel names.
    pub fn keymap_path(&self) -> &str {
        self.keymap_path.trim()
    }

    pub fn set_keymap_path(&mut self, path: &Path) {
        self.keymap_path = path.display().to_string();
    }

    fn capture_panel(&mut self, ui: &mut Ui) {
        ui.heading(text::CAPTURE);
        if ui.button(text::SCREENSHOT).clicked() {
            self.screenshot();
        }
        ui.separator();
        ui.add(
            egui::Slider::new(&mut self.capture.seconds, config::CAPTURE_SECONDS_RANGE)
                .text(text::SECONDS),
        );
        if ui.button(text::RECORD_SCREEN).clicked() {
            let path = config::recording_path();
            let done = text::saved(&path);
            let command = Command::Screenrecord {
                path,
                duration: std::time::Duration::from_secs(self.capture.seconds.into()),
            };
            self.spawn(command, done);
        }
        ui.label(text::RECORD_HINT);
    }

    fn settings_panel(&mut self, ui: &mut Ui, effects: &mut Vec<Effect>) {
        ui.heading(text::SETTINGS);
        if self.instance.is_none() {
            ui.label(text::NOT_AN_INSTANCE);
            ui.add(
                egui::TextEdit::singleline(&mut self.save_as)
                    .hint_text(text::NAME)
                    .desired_width(f32::INFINITY),
            )
            .accessible_name(text::NAME);
            if ui.button(text::SAVE_AS_INSTANCE).clicked() {
                self.status = save_as_instance(self.save_as.trim());
            }
            return;
        }
        let Some(form) = self.form.as_mut() else {
            if ui.button(text::RELOAD).clicked() {
                self.load_settings();
            }
            return;
        };
        match form.show(ui, true) {
            Some(Action::Restart) => effects.push(Effect::Restart),
            Some(Action::FactoryReset) => effects.push(Effect::FactoryReset),
            None => {}
        }
    }

    fn load_settings(&mut self) {
        let Some(name) = self.instance.clone() else {
            return;
        };
        match Form::open(&name) {
            Ok(form) => self.form = Some(form),
            Err(why) => self.status = why,
        }
    }
}

/// Makes an instance of the options this emulator was started with, so its settings can be
/// changed in the panel. The new instance has its own, new Android data.
fn save_as_instance(name: &str) -> String {
    let own = ["--data-dir", "--ctl"];
    let mut args = Vec::new();
    let mut given = std::env::args().skip(1);
    while let Some(arg) = given.next() {
        let option = arg
            .split_once('=')
            .map_or(arg.as_str(), |(option, _)| option);
        if own.contains(&option) {
            if !arg.contains('=') {
                given.next();
            }
            continue;
        }
        args.push(arg);
    }
    let made = instance::root().and_then(|root| {
        let cwd = std::env::current_dir()?;
        instance::create(&root, name, &args, &cwd)
    });
    match made {
        Ok(()) => text::instance_made(name),
        Err(err) => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;

    use super::*;

    /// A controller with a keyboard and touchscreen, recording the input it is given into `macro_file`.
    fn recording_controller(macro_file: &Path) -> Arc<Controller> {
        use devices::virtio::input::Input;
        let (_keyboard, keys) = Input::new(crate::input::keyboard());
        let controller = Controller {
            keyboard: Some(keys),
            ..Controller::default()
        };
        controller
            .execute(&Command::Record(macro_file.to_owned()))
            .unwrap();
        Arc::new(controller)
    }

    #[test]
    fn toolbar_buttons_press_androids_buttons() {
        let dir = std::env::temp_dir().join(format!("emu-gui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let recorded = dir.join("toolbar.txt");
        let controller = recording_controller(&recorded);
        let notify: Arc<dyn Fn(Notice) + Send + Sync> = Arc::new(|_| {});
        let gui = Gui::new(Arc::clone(&controller), notify, None, None);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(200.0, 800.0))
            .build_ui_state(
                |ui, gui: &mut Gui| {
                    let mut effects = Vec::new();
                    gui.toolbar(ui, &mut effects);
                },
                gui,
            );
        for label in [
            text::BACK,
            text::HOME,
            text::RECENTS,
            text::POWER,
            text::VOLUME_UP,
        ] {
            harness.get_by_label(label).click();
            harness.run();
        }
        controller.execute(&Command::RecordStop).unwrap();
        let lines: Vec<String> = std::fs::read_to_string(&recorded)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("key"))
            .map(str::to_owned)
            .collect();
        let expected: Vec<String> = [
            KEY_BACK,
            KEY_HOMEPAGE,
            KEY_APPSELECT,
            KEY_POWER,
            KEY_VOLUMEUP,
        ]
        .iter()
        .map(|code| format!("key {code}"))
        .collect();
        assert_eq!(lines, expected);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn panels_open_from_the_toolbar_and_report_what_went_wrong() {
        let notify: Arc<dyn Fn(Notice) + Send + Sync> = Arc::new(|_| {});
        let gui = Gui::new(Arc::new(Controller::default()), notify, None, None);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(600.0, 900.0))
            .build_ui_state(
                |ui, gui: &mut Gui| {
                    let mut effects = Vec::new();
                    gui.toolbar(ui, &mut effects);
                    gui.panel(ui, &mut effects);
                },
                gui,
            );
        harness.get_by_label(text::SENSORS).click();
        harness.run();
        assert_eq!(harness.state().open, Some(Panel::Sensors));
        harness.get_by_label(text::SHAKE).click();
        harness.run();
        // No motion sensors in this controller: the panel says so instead of failing quietly.
        assert!(!harness.state().status().is_empty());
        harness.get_by_label(text::LOCATION).click();
        harness.run();
        assert_eq!(harness.state().open, Some(Panel::Location));
        harness.get_by_label(text::SET_LOCATION).click();
        harness.run();
        assert_eq!(harness.state().status(), text::BAD_COORDINATES);
    }

    fn harness(gui: Gui) -> egui_kittest::Harness<'static, Gui> {
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 1000.0))
            .build_ui_state(
                |ui, gui: &mut Gui| {
                    let mut effects = Vec::new();
                    gui.toolbar(ui, &mut effects);
                    gui.panel(ui, &mut effects);
                },
                gui,
            )
    }

    #[test]
    fn a_macro_is_made_saved_and_played_from_its_panel() {
        let dir = std::env::temp_dir().join(format!("emu-gui-macro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let recorded = dir.join("played.txt");
        std::fs::create_dir_all(&dir).unwrap();
        let controller = recording_controller(&recorded);
        let notify: Arc<dyn Fn(Notice) + Send + Sync> = Arc::new(|_| {});
        let mut gui = Gui::new(Arc::clone(&controller), notify, None, None);
        gui.macros.dir = dir.join("macros");
        let mut harness = harness(gui);
        harness.get_by_label(text::MACROS).click();
        harness.run();
        harness.state_mut().macros.new_name = "back twice".to_owned();
        harness.run();
        harness.get_by_label(text::NEW_MACRO).click();
        harness.run();
        harness.state_mut().macros.text = format!("key {KEY_BACK}\nwait 1\nkey {KEY_BACK}\n");
        harness.get_by_label(text::SAVE).click();
        harness.run();
        let saved = dir.join("macros").join("back twice.txt");
        assert!(saved.is_file(), "{}", harness.state().status());
        assert_eq!(harness.state().macros.files, ["back twice.txt"]);
        // Played on its own thread; wait for the keys to arrive.
        harness.get_by_label(text::PLAY).click();
        harness.run();
        let keys = || {
            std::fs::read_to_string(&recorded)
                .map(|text| text.matches(&format!("key {KEY_BACK}")).count())
                .unwrap_or(0)
        };
        std::thread::sleep(std::time::Duration::from_secs(1));
        controller.execute(&Command::RecordStop).unwrap();
        assert_eq!(keys(), 2, "{}", harness.state().status());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_other_panels_open_and_answer() {
        let notify: Arc<dyn Fn(Notice) + Send + Sync> = Arc::new(|_| {});
        let gui = Gui::new(Arc::new(Controller::default()), notify, None, None);
        let mut harness = harness(gui);
        for (panel, label) in PANELS {
            harness.get_by_label(label).click();
            harness.run();
            assert_eq!(harness.state().open, Some(panel), "{label}");
        }
        // Settings of an emulator started without an instance offer to make one.
        assert!(harness.query_by_label(text::SAVE_AS_INSTANCE).is_some());
        harness.get_by_label(text::KEYS).click();
        harness.run();
        harness.get_by_label(text::EDIT_KEYMAP).click();
        harness.run();
        harness.get_by_label(text::CAPTURE).click();
        harness.run();
        harness.get_by_label(text::RECORD_SCREEN).click();
        harness.run();
        harness.get_by_label(text::APPS).click();
        harness.run();
        harness.get_by_label(text::INSTALL).click();
        harness.run();
    }

    #[test]
    fn every_panel_fits_its_width() {
        let notify: Arc<dyn Fn(Notice) + Send + Sync> = Arc::new(|_| {});
        let mut gui = Gui::new(Arc::new(Controller::default()), notify, None, None);
        gui.location.places.push(Place {
            name: "A rather long place name".into(),
            latitude: 1.0,
            longitude: 2.0,
        });
        gui.macros.selected = Some("m.txt".into());
        for (panel, label) in PANELS {
            gui.open = Some(panel);
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(1200.0, 1200.0))
                .build_ui_state(
                    |ui, state: &mut (Gui, f32, f32)| {
                        let (gui, inner, content) = state;
                        egui::Panel::right("panel")
                            .resizable(false)
                            .exact_size(config::PANEL_WIDTH)
                            .show(ui, |ui| {
                                *inner = ui.available_width();
                                *content = gui.panel(ui, &mut Vec::new());
                            });
                    },
                    (gui, 0.0, 0.0),
                );
            harness.run();
            let (_, inner, content) = harness.state();
            assert!(content <= inner, "{label}: {content} wider than {inner}");
            gui = harness.into_state().0;
        }
    }

    #[test]
    fn package_names_are_read_from_the_package_manager() {
        let printed = "package:com.b.game\r\npackage:com.a.app\nWARNING: junk\npackage:\n";
        assert_eq!(packages(printed), ["com.a.app", "com.b.game"]);
    }

    #[test]
    fn typed_file_names_stay_inside_their_folder() {
        assert_eq!(
            file_name("farm loop", ".txt").as_deref(),
            Some("farm loop.txt")
        );
        assert_eq!(file_name("a.txt", ".txt").as_deref(), Some("a.txt"));
        assert_eq!(file_name("../x", ".txt"), None);
        assert_eq!(file_name(".hidden", ".txt"), None);
        assert_eq!(file_name("   ", ".txt"), None);
    }

    #[test]
    fn saved_places_come_back() {
        let dir = std::env::temp_dir().join(format!("emu-places-{}", std::process::id()));
        let path = dir.join("places.toml");
        let places = [Place {
            name: "Home".into(),
            latitude: 3.139,
            longitude: 101.6869,
        }];
        write_places(&path, &places).unwrap();
        assert_eq!(read_places(&path), places);
        assert!(read_places(&dir.join("missing.toml")).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
