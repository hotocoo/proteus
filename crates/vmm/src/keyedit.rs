//! Editing a key map on the guest's picture: taps and sticks are shown where they touch, placed by
//! clicking, and bound by pressing a key. The file is written in the format `keymap` reads; parts
//! of a map the editor does not show (look, walk, tilt, zoom, toggle) are kept as they were.

use std::path::{Path, PathBuf};

use egui::{Color32, Pos2, Rect, Stroke, Ui};
use serde::{Deserialize, Serialize};

use crate::config::{EDITOR_MARKER_RADIUS, EDITOR_STICK_RADIUS};
use crate::copy::gui as text;
use crate::gui::Effect;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TapEntry {
    pub key: String,
    pub at: [u32; 2],
    /// What the editor does not show (`pointer`, `then`), kept as it was.
    #[serde(flatten)]
    pub rest: toml::Table,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StickEntry {
    pub up: String,
    pub down: String,
    pub left: String,
    pub right: String,
    pub center: [u32; 2],
    pub radius: u32,
}

/// A key map as a file: what the editor shows, and the rest kept as it was.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MapFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tap: Vec<TapEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stick: Vec<StickEntry>,
    #[serde(flatten)]
    pub rest: toml::Table,
}

impl MapFile {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|err| err.to_string())
    }

    pub fn to_text(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|err| err.to_string())
    }
}

/// What a click on the picture or a key press goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waiting {
    Nothing,
    /// Where the new tap is.
    TapPlace,
    /// Which key the tap at this index is.
    TapKey(usize),
    /// Where the new stick's centre is.
    StickPlace,
    /// Which key pushes the stick at this index this way: 0 up, 1 down, 2 left, 3 right.
    StickKey(usize, usize),
}

pub struct Editor {
    path: PathBuf,
    map: MapFile,
    waiting: Waiting,
    status: String,
}

impl Editor {
    /// Opens the key map at `path`, or a new one if there is no file there yet.
    pub fn open(path: &Path) -> Result<Self, String> {
        if path.as_os_str().is_empty() {
            return Err(text::KEYMAP_NEEDS_PATH.to_owned());
        }
        let map = match std::fs::read_to_string(path) {
            Ok(text) => MapFile::parse(&text)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => MapFile::default(),
            Err(err) => return Err(crate::copy::file(path, &err)),
        };
        Ok(Self {
            path: path.to_owned(),
            map,
            waiting: Waiting::Nothing,
            status: String::new(),
        })
    }

    /// Whether the next key press is for the editor.
    pub fn wants_key(&self) -> bool {
        matches!(self.waiting, Waiting::TapKey(_) | Waiting::StickKey(..))
    }

    /// A key, by the window system's name for it.
    pub fn key(&mut self, name: &str) {
        match self.waiting {
            Waiting::TapKey(index) => {
                if let Some(tap) = self.map.tap.get_mut(index) {
                    tap.key = name.to_owned();
                }
                self.waiting = Waiting::Nothing;
            }
            Waiting::StickKey(index, way) => {
                if let Some(stick) = self.map.stick.get_mut(index) {
                    *[
                        &mut stick.up,
                        &mut stick.down,
                        &mut stick.left,
                        &mut stick.right,
                    ][way] = name.to_owned();
                }
                self.waiting = if way + 1 < text::STICK_WAYS.len() {
                    Waiting::StickKey(index, way + 1)
                } else {
                    Waiting::Nothing
                };
            }
            _ => {}
        }
    }

    /// A click on the upright guest pixel `at`.
    fn click(&mut self, at: [u32; 2]) {
        match self.waiting {
            Waiting::TapPlace => {
                self.map.tap.push(TapEntry {
                    key: String::new(),
                    at,
                    rest: toml::Table::new(),
                });
                self.waiting = Waiting::TapKey(self.map.tap.len() - 1);
            }
            Waiting::StickPlace => {
                self.map.stick.push(StickEntry {
                    center: at,
                    radius: EDITOR_STICK_RADIUS,
                    ..StickEntry::default()
                });
                self.waiting = Waiting::StickKey(self.map.stick.len() - 1, 0);
            }
            _ => {}
        }
    }

    /// What the editor is waiting for, in words.
    fn prompt(&self) -> String {
        match self.waiting {
            Waiting::Nothing => text::KEYMAP_EDIT_HINT.to_owned(),
            Waiting::TapPlace | Waiting::StickPlace => text::CLICK_PLACE.to_owned(),
            Waiting::TapKey(_) => text::PRESS_KEY.to_owned(),
            Waiting::StickKey(_, way) => text::press_way(text::STICK_WAYS[way]),
        }
    }

    /// Writes the map. A map the emulator would refuse is not written.
    fn save(&self, display: (u32, u32)) -> Result<(), String> {
        let text = self.map.to_text()?;
        crate::keymap::Mapper::parse(&text, display)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| crate::copy::file(dir, &err))?;
        }
        std::fs::write(&self.path, text).map_err(|err| crate::copy::file(&self.path, &err))
    }

    /// The editor's panel beside the picture.
    pub fn panel(&mut self, ui: &mut Ui, effects: &mut Vec<Effect>) {
        ui.heading(text::KEYS);
        ui.label(self.path.display().to_string());
        ui.label(self.prompt());
        ui.horizontal_wrapped(|ui| {
            if ui.button(text::ADD_TAP).clicked() {
                self.waiting = Waiting::TapPlace;
            }
            if ui.button(text::ADD_STICK).clicked() {
                self.waiting = Waiting::StickPlace;
            }
        });
        ui.separator();
        let mut remove_tap = None;
        for (index, tap) in self.map.tap.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(text::tap_line(&tap.key, tap.at));
                if ui.small_button(text::REBIND).clicked() {
                    self.waiting = Waiting::TapKey(index);
                }
                if ui.small_button(text::REMOVE).clicked() {
                    remove_tap = Some(index);
                }
            });
        }
        let mut remove_stick = None;
        for (index, stick) in self.map.stick.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(text::stick_line(
                    [&stick.up, &stick.down, &stick.left, &stick.right],
                    stick.center,
                ));
                if ui.small_button(text::REBIND).clicked() {
                    self.waiting = Waiting::StickKey(index, 0);
                }
                if ui.small_button(text::REMOVE).clicked() {
                    remove_stick = Some(index);
                }
            });
            ui.add(
                egui::Slider::new(&mut stick.radius, crate::config::EDITOR_STICK_RADIUS_RANGE)
                    .text(text::RADIUS),
            );
        }
        if let Some(index) = remove_tap {
            self.map.tap.remove(index);
            self.waiting = Waiting::Nothing;
        }
        if let Some(index) = remove_stick {
            self.map.stick.remove(index);
            self.waiting = Waiting::Nothing;
        }
        ui.separator();
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        ui.horizontal(|ui| {
            if ui.button(text::SAVE_AND_USE).clicked() {
                let display = ui.data(|data| data.get_temp(egui::Id::unique(DISPLAY_KEY)));
                match self.save(display.unwrap_or_default()) {
                    Ok(()) => effects.push(Effect::CloseEditor(Some(self.path.clone()))),
                    Err(why) => self.status = why,
                }
            }
            if ui.button(text::DISCARD).clicked() {
                effects.push(Effect::CloseEditor(None));
            }
        });
    }

    /// Markers over the picture, drawn in `rect` for an upright picture of `picture` pixels; a click
    /// there places what the editor is waiting to place.
    pub fn overlay(&mut self, ui: &mut Ui, rect: Rect, picture: (u32, u32)) {
        // The display size the map is checked against when it is saved: any way up.
        let any = crate::orientation::any_way_up(picture);
        ui.data_mut(|data| data.insert_temp(egui::Id::unique(DISPLAY_KEY), any));
        let scale = rect.width() / picture.0.max(1) as f32;
        let to_screen = |at: [u32; 2]| {
            Pos2::new(
                rect.left() + at[0] as f32 * scale,
                rect.top() + at[1] as f32 * scale,
            )
        };
        let painter = ui.painter_at(rect);
        let marker = Color32::from_rgba_unmultiplied(
            crate::config::EDITOR_MARKER_RGBA[0],
            crate::config::EDITOR_MARKER_RGBA[1],
            crate::config::EDITOR_MARKER_RGBA[2],
            crate::config::EDITOR_MARKER_RGBA[3],
        );
        let stroke = Stroke::new(crate::config::EDITOR_STROKE, Color32::WHITE);
        for tap in &self.map.tap {
            let center = to_screen(tap.at);
            painter.circle(center, EDITOR_MARKER_RADIUS, marker, stroke);
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                short_name(&tap.key),
                egui::FontId::proportional(crate::config::EDITOR_LABEL_SIZE),
                Color32::WHITE,
            );
        }
        for stick in &self.map.stick {
            let center = to_screen(stick.center);
            painter.circle_stroke(center, stick.radius as f32 * scale, stroke);
            painter.circle(center, EDITOR_MARKER_RADIUS, marker, stroke);
        }
        let response = ui.interact(rect, egui::Id::unique(OVERLAY_ID), egui::Sense::click());
        if response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let at = [
                ((pointer.x - rect.left()) / scale) as u32,
                ((pointer.y - rect.top()) / scale) as u32,
            ];
            if at[0] < picture.0 && at[1] < picture.1 {
                self.click(at);
            }
        }
    }
}

const DISPLAY_KEY: &str = "keymap editor display";
const OVERLAY_ID: &str = "keymap editor overlay";
/// Key names shown on markers lose these window-system prefixes ("KeyW" shows as "W").
const NAME_PREFIXES: [&str; 2] = ["Key", "Digit"];

fn short_name(name: &str) -> &str {
    NAME_PREFIXES
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix).filter(|rest| !rest.is_empty()))
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: (u32, u32) = (1280, 1280);

    #[test]
    fn a_map_made_by_clicking_and_pressing_is_one_the_emulator_reads() {
        let mut editor = Editor {
            path: PathBuf::new(),
            map: MapFile::default(),
            waiting: Waiting::TapPlace,
            status: String::new(),
        };
        editor.click([1100, 600]);
        assert!(editor.wants_key());
        editor.key("Space");
        editor.waiting = Waiting::StickPlace;
        editor.click([200, 550]);
        for name in ["KeyW", "KeyS", "KeyA", "KeyD"] {
            editor.key(name);
        }
        assert!(!editor.wants_key());
        let text = editor.map.to_text().unwrap();
        crate::keymap::Mapper::parse(&text, DISPLAY).unwrap();
        assert_eq!(editor.map.stick[0].left, "KeyA");
        assert_eq!(editor.map.tap[0].at, [1100, 600]);
    }

    #[test]
    fn parts_the_editor_does_not_show_are_kept() {
        let original = "toggle = \"F12\"\n\n[[tap]]\nkey = \"Space\"\nat = [10, 20]\n\
                        pointer = true\nthen = [[30, 40]]\n\n\
                        [look]\nkey = \"Backquote\"\ncenter = [900, 300]\nradius = 250\n";
        let map = MapFile::parse(original).unwrap();
        let again = MapFile::parse(&map.to_text().unwrap()).unwrap();
        assert_eq!(again, map);
        assert!(again.rest.contains_key("look"));
        assert!(again.rest.contains_key("toggle"));
        assert_eq!(again.tap[0].rest["pointer"].as_bool(), Some(true));
        assert!(again.tap[0].rest.contains_key("then"));
        crate::keymap::Mapper::parse(&map.to_text().unwrap(), DISPLAY).unwrap();
    }

    #[test]
    fn marker_names_are_short() {
        assert_eq!(short_name("KeyW"), "W");
        assert_eq!(short_name("Digit1"), "1");
        assert_eq!(short_name("Space"), "Space");
        assert_eq!(short_name("Key"), "Key");
    }
}
