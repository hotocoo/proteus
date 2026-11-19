//! The settings form for one instance: its Android image (ROM) and kernel, memory, CPUs, screen,
//! graphics, sound, controls and network, saved to the instance's options and used when it next
//! starts. Shown by the launcher and by the running emulator's Settings panel.

use egui::Ui;

use crate::config::{self, style as tune};
use crate::copy::gui as text;
use crate::instance;
use crate::settings::{Gamepad, Settings, Sound};
use crate::style::{self, Pick};

/// Screen sizes offered, upright.
const DISPLAY_PRESETS: [(&str, (u32, u32)); 4] = [
    (text::DISPLAY_PHONE, (720, 1280)),
    (text::DISPLAY_PHONE_HD, (1080, 1920)),
    (text::DISPLAY_PHONE_TALL, (1080, 2400)),
    (text::DISPLAY_TABLET, (1200, 1920)),
];

/// What the user asked the form to do beyond saving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Saved; start the instance again so the settings apply.
    Restart,
    /// Delete the instance's Android data and start it fresh.
    FactoryReset,
}

pub struct Form {
    /// The folder instances live in, and this one's name.
    root: std::path::PathBuf,
    name: String,
    settings: Settings,
    custom_display: (u32, u32),
    blocklists: String,
    allowlists: String,
    sync: String,
    /// Whether what is shown is what is saved.
    saved: bool,
    confirm_reset: bool,
    pub status: String,
}

impl Form {
    /// Reads the saved options of the instance `name`.
    pub fn open(name: &str) -> Result<Self, String> {
        let root = instance::root().map_err(|err| err.to_string())?;
        Self::open_in(root, name)
    }

    /// As `open`, for instances kept in `root`.
    pub fn open_in(root: std::path::PathBuf, name: &str) -> Result<Self, String> {
        let args = instance::args(&root, name).map_err(|err| err.to_string())?;
        let settings = Settings::from_args(&args);
        Ok(Self {
            root,
            name: name.to_owned(),
            custom_display: settings.display,
            blocklists: settings.blocklists.join("\n"),
            allowlists: settings.allowlists.join("\n"),
            sync: settings.sync.join("\n"),
            settings,
            saved: true,
            confirm_reset: false,
            status: String::new(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn save(&mut self) -> Result<(), String> {
        let lines = |text: &str| -> Vec<String> {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        };
        self.settings.blocklists = lines(&self.blocklists);
        self.settings.allowlists = lines(&self.allowlists);
        self.settings.sync = lines(&self.sync);
        let args = self.settings.to_args();
        // Checked as the emulator will read them, so a mistake shows now and not at start.
        let mut check: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
        check.extend([
            config::DATA_DIR_OPTION.into(),
            config::CHECK_DATA_DIR.into(),
        ]);
        config::parse(check).map_err(|err| err.to_string())?;
        instance::set_args(&self.root, &self.name, &args).map_err(|err| err.to_string())
    }

    /// Shows the form. `running` says whether the instance is running now, which decides whether
    /// saving offers a restart.
    pub fn show(&mut self, ui: &mut Ui, running: bool) -> Option<Action> {
        let mut action = None;
        let mut edited = false;
        let settings = &mut self.settings;

        style::section(ui, text::ROM, |ui| {
            style::hint(ui, text::ROM_HINT);
            style::label(ui, text::IMAGE_FOLDER);
            edited |= style::path(
                ui,
                text::IMAGE_FOLDER,
                &mut settings.android,
                Pick::Folder,
                text::IMAGE_FOLDER_HINT,
            );
            style::label(ui, text::KERNEL);
            edited |= style::path(
                ui,
                text::KERNEL,
                &mut settings.kernel,
                Pick::File,
                text::KERNEL_HINT,
            );
        });

        style::section(ui, text::PERFORMANCE, |ui| {
            edited |= ui
                .add(
                    egui::Slider::new(&mut settings.ram_mib, config::RAM_MIB_RANGE)
                        .step_by(config::RAM_MIB_STEP)
                        .suffix(text::MIB)
                        .text(text::MEMORY),
                )
                .changed();
            edited |= ui
                .add(egui::Slider::new(&mut settings.vcpus, config::VCPUS_RANGE).text(text::CPUS))
                .changed();
            style::hint(ui, &text::host_has(host_cpus(), host_memory_mib()));
        });

        style::section(ui, text::SCREEN, |ui| {
            for (label, size) in DISPLAY_PRESETS {
                edited |= ui
                    .radio_value(&mut settings.display, size, text::preset(label, size))
                    .changed();
            }
            ui.horizontal(|ui| {
                let custom = &mut self.custom_display;
                ui.label(text::CUSTOM);
                ui.add(egui::DragValue::new(&mut custom.0).range(config::DISPLAY_RANGE))
                    .accessible_name(text::CUSTOM_WIDTH);
                ui.label(text::BY);
                ui.add(egui::DragValue::new(&mut custom.1).range(config::DISPLAY_RANGE))
                    .accessible_name(text::CUSTOM_HEIGHT);
                if ui.button(text::USE_SIZE).clicked() {
                    settings.display = *custom;
                    edited = true;
                }
            });
            style::hint(ui, text::SCREEN_HINT);
        });

        style::section(ui, text::GRAPHICS_AND_SOUND, |ui| {
            edited |= ui.checkbox(&mut settings.gpu, text::USE_GPU).changed();
            style::hint(ui, text::GPU_HINT);
            style::row(ui, text::SOUND, |ui| {
                for (value, label) in [
                    (Sound::On, text::SOUND_ON),
                    (Sound::Muted, text::SOUND_MUTED),
                    (Sound::Off, text::SOUND_OFF),
                ] {
                    edited |= ui.radio_value(&mut settings.sound, value, label).changed();
                }
            });
        });

        style::section(ui, text::CONTROLS, |ui| {
            style::row(ui, text::GAMEPAD, |ui| {
                for (value, label) in [
                    (Gamepad::Auto, text::GAMEPAD_AUTO),
                    (Gamepad::Always, text::GAMEPAD_ALWAYS),
                    (Gamepad::Never, text::GAMEPAD_NEVER),
                ] {
                    edited |= ui
                        .radio_value(&mut settings.gamepad, value, label)
                        .changed();
                }
            });
            style::label(ui, text::KEYMAP_FILE);
            edited |= style::path(
                ui,
                text::KEYMAP_FILE,
                &mut settings.keymap,
                Pick::File,
                text::KEYMAP_PATH_HINT,
            );
            style::label(ui, text::KEYMAP_FOLDER);
            edited |= style::path(
                ui,
                text::KEYMAP_FOLDER,
                &mut settings.keymap_dir,
                Pick::Folder,
                text::KEYMAP_FOLDER_HINT,
            );
            style::label(ui, text::SYNC);
            edited |= ui
                .add(
                    egui::TextEdit::multiline(&mut self.sync)
                        .hint_text(text::SYNC_HINT)
                        .desired_rows(config::LIST_ROWS)
                        .desired_width(f32::INFINITY),
                )
                .accessible_name(text::SYNC)
                .changed();
        });

        style::section(ui, text::NETWORK, |ui| {
            edited |= ui
                .checkbox(&mut settings.network, text::NETWORK_ON)
                .changed();
            edited |= ui
                .checkbox(&mut settings.allow_quic, text::ALLOW_QUIC)
                .changed();
            edited |= ui
                .checkbox(&mut settings.host_loopback, text::HOST_LOOPBACK)
                .on_hover_text(text::HOST_LOOPBACK_HINT)
                .changed();
            style::row(ui, text::ADB_PORT, |ui| {
                edited |= ui
                    .add(egui::DragValue::new(&mut settings.adb_port))
                    .accessible_name(text::ADB_PORT)
                    .changed();
            });
            style::label(ui, text::BLOCKLISTS);
            edited |= ui
                .add(
                    egui::TextEdit::multiline(&mut self.blocklists)
                        .hint_text(text::ONE_FILE_PER_LINE)
                        .desired_rows(config::LIST_ROWS)
                        .desired_width(f32::INFINITY),
                )
                .accessible_name(text::BLOCKLISTS)
                .changed();
            style::label(ui, text::ALLOWLISTS);
            edited |= ui
                .add(
                    egui::TextEdit::multiline(&mut self.allowlists)
                        .hint_text(text::ONE_FILE_PER_LINE)
                        .desired_rows(config::LIST_ROWS)
                        .desired_width(f32::INFINITY),
                )
                .accessible_name(text::ALLOWLISTS)
                .changed();
        });

        style::section(ui, text::DATA, |ui| {
            edited |= ui
                .add(
                    egui::Slider::new(&mut settings.userdata_gib, config::USERDATA_GIB_RANGE)
                        .suffix(text::GIB)
                        .text(text::DATA_SIZE),
                )
                .changed();
            style::hint(ui, text::DATA_SIZE_HINT);
            ui.add_space(tune::GAP);
            if self.confirm_reset {
                ui.label(text::RESET_SURE);
                ui.horizontal(|ui| {
                    if style::danger(ui, text::RESET_CONFIRM) {
                        action = Some(Action::FactoryReset);
                        self.confirm_reset = false;
                    }
                    if ui.button(text::KEEP).clicked() {
                        self.confirm_reset = false;
                    }
                });
            } else if style::danger(ui, text::FACTORY_RESET) {
                self.confirm_reset = true;
            }
        });

        if edited {
            self.saved = false;
        }
        ui.add_space(tune::SECTION_GAP);
        ui.horizontal(|ui| {
            let label = if running {
                text::SAVE_AND_RESTART
            } else {
                text::SAVE
            };
            if !self.saved && style::primary(ui, label) {
                match self.save() {
                    Ok(()) => {
                        self.saved = true;
                        self.status = text::SETTINGS_SAVED.to_owned();
                        if running {
                            action = Some(Action::Restart);
                        }
                    }
                    Err(why) => self.status = why,
                }
            }
            if !self.saved && ui.button(text::REVERT).clicked() {
                match Self::open_in(self.root.clone(), &self.name) {
                    Ok(form) => *self = form,
                    Err(why) => self.status = why,
                }
            }
        });
        if !self.status.is_empty() {
            style::hint(ui, &self.status);
        }
        if running {
            style::hint(ui, text::APPLIES_ON_RESTART);
        }
        action
    }
}

fn host_cpus() -> usize {
    std::thread::available_parallelism().map_or(0, usize::from)
}

/// This machine's memory, in MiB; zero if it cannot be read.
fn host_memory_mib() -> u64 {
    const MIB: u64 = 1 << 20;
    // SAFETY: sysconf only reads system values.
    let (pages, page) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    (u64::try_from(pages).unwrap_or(0) * u64::try_from(page).unwrap_or(0)) / MIB
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;

    use super::*;

    #[test]
    fn settings_are_saved_as_options_the_emulator_reads() {
        let root = std::env::temp_dir().join(format!("emu-form-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let args = ["--android", "/images", "--kernel", "/k"].map(str::to_owned);
        instance::create(&root, "phone", &args, std::path::Path::new("/")).unwrap();
        let form = Form::open_in(root.clone(), "phone").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(600.0, 1600.0))
            .build_ui_state(
                |ui, form: &mut Form| {
                    egui::ScrollArea::vertical().show(ui, |ui| form.show(ui, false));
                },
                form,
            );
        harness.get_by_label(text::USE_GPU).click();
        harness.run();
        harness.state_mut().sync = "farm".to_owned();
        harness.get_by_label(text::SAVE).click();
        harness.run();
        assert_eq!(harness.state().status, text::SETTINGS_SAVED);
        let saved = instance::args(&root, "phone").unwrap();
        assert!(saved.contains(&"--gpu".to_owned()), "{saved:?}");
        assert!(
            saved.windows(2).any(|pair| pair == ["--sync", "farm"]),
            "{saved:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
