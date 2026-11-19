//! The window `emu` opens with no arguments (and when the app is opened from the Finder): the
//! instances, each with Start, Stop, Settings and Delete, and a form to make a new one from an
//! Android image folder.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{RichText, Ui};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::config::{self, style as tune};
use crate::copy::gui as text;
use crate::painter::Painter;
use crate::settings_ui::{Action, Form};
use crate::style::{self, Pick};
use crate::{copy, instance};

/// One instance as the list shows it.
struct Entry {
    name: String,
    running: bool,
    image: String,
}

#[derive(Default)]
struct NewInstance {
    name: String,
    image: String,
    kernel: String,
    gpu: bool,
}

struct Launcher {
    window: Option<(Arc<Window>, Painter)>,
    entries: Vec<Entry>,
    listed_at: Option<Instant>,
    new: NewInstance,
    /// The instance whose settings are open.
    form: Option<Form>,
    /// The instance whose deletion waits for a second click.
    confirm_delete: Option<String>,
    status: String,
    failed: Option<String>,
}

/// The instance names in use, for a new instance's default name.
fn free_name(entries: &[Entry]) -> String {
    (1..)
        .map(|n| {
            if n == 1 {
                config::DEFAULT_INSTANCE.to_owned()
            } else {
                format!("{}{n}", config::DEFAULT_INSTANCE)
            }
        })
        .find(|name| entries.iter().all(|entry| entry.name != *name))
        .unwrap_or_default()
}

/// The options a new instance starts with.
pub fn new_args(image: &str, kernel: &str, gpu: bool) -> Vec<String> {
    let mut args = vec![config::ANDROID_OPTION.to_owned(), image.trim().to_owned()];
    if !kernel.trim().is_empty() {
        args.extend([config::KERNEL_OPTION.to_owned(), kernel.trim().to_owned()]);
    }
    if gpu {
        args.push(config::GPU_OPTION.to_owned());
    }
    args
}

/// Starts the instance `name` as its own program, so it keeps running if the launcher closes.
fn start(name: &str) -> Result<(), String> {
    let program = std::env::current_exe().map_err(|err| err.to_string())?;
    std::process::Command::new(program)
        .args([config::INSTANCE_SUBCOMMAND, config::INSTANCE_START, name])
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|err| err.to_string())
}

impl Launcher {
    fn refresh(&mut self) {
        let fresh = self
            .listed_at
            .is_some_and(|at| at.elapsed() < config::LAUNCHER_REFRESH);
        if fresh {
            return;
        }
        self.listed_at = Some(Instant::now());
        let Ok(root) = instance::root() else {
            return;
        };
        let listed = instance::list(&root).unwrap_or_default();
        self.entries = listed
            .into_iter()
            .map(|(name, running)| {
                let image = instance::args(&root, &name)
                    .map(|args| crate::settings::Settings::from_args(&args).android)
                    .unwrap_or_default();
                Entry {
                    name,
                    running,
                    image,
                }
            })
            .collect();
        if self.new.name.is_empty() {
            self.new.name = free_name(&self.entries);
        }
    }

    fn ui(&mut self, ui: &mut Ui) {
        self.refresh();
        egui::Panel::top(copy::LAUNCHER_HEADER_ID).show(ui, |ui| {
            ui.add_space(tune::GAP);
            ui.heading(text::LAUNCHER_TITLE);
            style::hint(ui, text::LAUNCHER_SUBTITLE);
            ui.add_space(tune::GAP);
        });
        if !self.status.is_empty() {
            egui::Panel::bottom(copy::STATUS_ID).show(ui, |ui| {
                ui.label(&self.status);
            });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(form) = self.form.as_mut() {
                    let mut close = false;
                    ui.horizontal(|ui| {
                        if ui.button(text::BACK_TO_LIST).clicked() {
                            close = true;
                        }
                        ui.heading(text::settings_for(form.name()));
                    });
                    let name = form.name().to_owned();
                    let running = self.entries.iter().any(|e| e.name == name && e.running);
                    match form.show(ui, running) {
                        Some(Action::FactoryReset) => {
                            self.status = factory_reset(&name, running);
                        }
                        Some(Action::Restart) => {
                            self.status = text::RESTART_FROM_WINDOW.to_owned();
                        }
                        None => {}
                    }
                    if close {
                        self.form = None;
                    }
                    return;
                }
                self.instances(ui);
                self.create(ui);
                style::section(ui, text::GET_ANDROID, |ui| {
                    style::hint(ui, text::GET_ANDROID_HINT);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(config::ANDROID_IMAGES_URL).monospace());
                        if ui.button(text::COPY).clicked() {
                            ui.ctx().copy_text(config::ANDROID_IMAGES_URL.to_owned());
                        }
                    });
                });
            });
        });
    }

    fn instances(&mut self, ui: &mut Ui) {
        style::section(ui, text::INSTANCES, |ui| {
            if self.entries.is_empty() {
                style::hint(ui, text::NO_INSTANCES);
            }
            let mut open = None;
            for entry in &self.entries {
                ui.horizontal(|ui| {
                    let word = if entry.running {
                        text::RUNNING
                    } else {
                        text::STOPPED
                    };
                    ui.label(RichText::new(&entry.name).strong());
                    style::state(ui, word, entry.running);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let confirming = self.confirm_delete.as_deref() == Some(&entry.name);
                        if !entry.running {
                            if confirming {
                                if style::danger(ui, text::DELETE_SURE) {
                                    self.status = delete(&entry.name);
                                    self.confirm_delete = None;
                                    self.listed_at = None;
                                }
                            } else if ui.button(text::DELETE).clicked() {
                                self.confirm_delete = Some(entry.name.clone());
                            }
                        }
                        if ui.button(text::SETTINGS).clicked() {
                            open = Some(entry.name.clone());
                        }
                        if entry.running {
                            if ui.button(text::STOP).clicked() {
                                self.status = stop(&entry.name);
                                self.listed_at = None;
                            }
                        } else if style::primary(ui, text::START) {
                            self.status = match start(&entry.name) {
                                Ok(()) => text::starting(&entry.name),
                                Err(why) => why,
                            };
                            self.listed_at = None;
                        }
                    });
                });
                if !entry.image.is_empty() {
                    style::hint(ui, &entry.image);
                }
                ui.separator();
            }
            if let Some(name) = open {
                match Form::open(&name) {
                    Ok(form) => self.form = Some(form),
                    Err(why) => self.status = why,
                }
            }
        });
    }

    fn create(&mut self, ui: &mut Ui) {
        style::section(ui, text::NEW_INSTANCE, |ui| {
            style::row(ui, text::NAME, |ui| {
                ui.text_edit_singleline(&mut self.new.name);
            });
            style::label(ui, text::IMAGE_FOLDER);
            style::path(
                ui,
                &mut self.new.image,
                Pick::Folder,
                text::IMAGE_FOLDER_HINT,
            );
            style::label(ui, text::KERNEL);
            style::path(ui, &mut self.new.kernel, Pick::File, text::KERNEL_HINT);
            ui.checkbox(&mut self.new.gpu, text::USE_GPU);
            if style::primary(ui, text::CREATE_AND_START) {
                self.status = self.make();
                self.listed_at = None;
            }
        });
    }

    /// Makes the new instance and starts it.
    fn make(&mut self) -> String {
        let name = self.new.name.trim().to_owned();
        if self.new.image.trim().is_empty() {
            return text::NEEDS_IMAGE.to_owned();
        }
        let args = new_args(&self.new.image, &self.new.kernel, self.new.gpu);
        let mut check: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
        check.extend([
            config::DATA_DIR_OPTION.into(),
            config::CHECK_DATA_DIR.into(),
        ]);
        if let Err(err) = config::parse(check) {
            return err.to_string();
        }
        let made = instance::root().and_then(|root| {
            let cwd = std::env::current_dir()?;
            instance::create(&root, &name, &args, &cwd)
        });
        if let Err(err) = made {
            return err.to_string();
        }
        self.new = NewInstance::default();
        match start(&name) {
            Ok(()) => text::starting(&name),
            Err(why) => why,
        }
    }
}

fn stop(name: &str) -> String {
    let socket = instance::root().and_then(|root| instance::socket(&root, name));
    let stopped = socket.and_then(|socket| {
        crate::ctl::request(&socket, crate::ctl::WORD_STOP, &mut std::io::sink())
    });
    match stopped {
        Ok(_) => text::stopping(name),
        Err(err) => err.to_string(),
    }
}

fn delete(name: &str) -> String {
    let removed = instance::root().and_then(|root| {
        instance::remove(
            &root,
            name,
            true,
            &mut std::io::sink(),
            &mut std::io::empty(),
        )
    });
    match removed {
        Ok(_) => text::deleted(name),
        Err(err) => err.to_string(),
    }
}

/// Deletes the Android data of a stopped instance, so it starts as a new phone.
fn factory_reset(name: &str, running: bool) -> String {
    if running {
        return text::STOP_FIRST.to_owned();
    }
    let wiped = instance::root().and_then(|root| instance::wipe_data(&root, name));
    match wiped {
        Ok(()) => text::reset_done(name),
        Err(err) => err.to_string(),
    }
}

impl ApplicationHandler for Launcher {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(copy::LAUNCHER_TITLE)
            .with_inner_size(LogicalSize::new(
                config::LAUNCHER_SIZE.0,
                config::LAUNCHER_SIZE.1,
            ));
        let made = event_loop
            .create_window(attributes)
            .map_err(|err| err.to_string())
            .and_then(|window| {
                let window = Arc::new(window);
                let painter = Painter::new(event_loop, &window)?;
                style::apply(&painter.ctx);
                Ok((window, painter))
            });
        match made {
            Ok(window) => self.window = Some(window),
            Err(why) => {
                self.failed = Some(why);
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The list is read again now and then, so instances started or stopped elsewhere show.
        let next = Instant::now() + config::LAUNCHER_REFRESH;
        let at = self
            .window
            .as_ref()
            .and_then(|(_, painter)| painter.repaint_at)
            .map_or(next, |at| at.min(next));
        if at <= Instant::now()
            && let Some((window, _)) = &self.window
        {
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(at.max(Instant::now() + MIN_WAIT)));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some((window, painter)) = self.window.as_mut() else {
            return;
        };
        painter.on_event(window, &event);
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                let window = Arc::clone(window);
                let mut painter = self.window.take().map(|(_, painter)| painter);
                if let Some(painter) = painter.as_mut() {
                    painter.paint(&window, |ui| self.ui(ui));
                }
                self.window = painter.map(|painter| (window, painter));
            }
            _ => {}
        }
    }
}

/// Shortest wait between two looks at the list, so an idle launcher costs nothing.
const MIN_WAIT: Duration = Duration::from_millis(16);

/// Shows the launcher until it is closed.
pub fn run() -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|err| err.to_string())?;
    let mut launcher = Launcher {
        window: None,
        entries: Vec::new(),
        listed_at: None,
        new: NewInstance {
            gpu: true,
            ..NewInstance::default()
        },
        form: None,
        confirm_delete: None,
        status: String::new(),
        failed: None,
    };
    let ran = event_loop.run_app(&mut launcher);
    launcher.window = None;
    match (launcher.failed, ran) {
        (Some(why), _) => Err(why),
        (None, Err(err)) => Err(err.to_string()),
        (None, Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_instance_gets_a_free_name_and_options_the_emulator_reads() {
        let entries = [Entry {
            name: config::DEFAULT_INSTANCE.to_owned(),
            running: false,
            image: String::new(),
        }];
        assert_eq!(
            free_name(&entries),
            format!("{}2", config::DEFAULT_INSTANCE)
        );
        assert_eq!(free_name(&[]), config::DEFAULT_INSTANCE);
        let args = new_args(" /images ", "/k", true);
        assert_eq!(args, ["--android", "/images", "--kernel", "/k", "--gpu"]);
        let mut check: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
        check.extend(["--data-dir".into(), "x".into()]);
        assert!(config::parse(check).is_ok());
        assert_eq!(new_args("/images", "", false), ["--android", "/images"]);
    }
}
