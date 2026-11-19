//! What outlives one boot of the machine.
//!
//! A guest that restarts gets a new machine: new memory, new devices. Whoever drives the guest from
//! outside (the control socket now, a window later) must not notice, so the things they hold are
//! made once per process here and handed to each machine in turn.

use std::sync::{Arc, Condvar, Mutex, PoisonError};

use devices::virtio::input::InputHandle;

use crate::VmmError;
use crate::config::{Config, GamepadMode};
use crate::ctl::{self, Controller};
use crate::frames::{self, Screen};
use crate::input::Touch;
use crate::sensors::{self, Motion};

#[derive(Default)]
struct Switch {
    off_requested: bool,
    /// How many boots of the machine have ended.
    boots_ended: u64,
}

/// The host's power button: turns the machine off without the guest's cooperation.
#[derive(Default)]
pub struct PowerSwitch {
    switch: Mutex<Switch>,
    changed: Condvar,
}

impl PowerSwitch {
    /// Turns the machine off, now or as soon as there is one. The guest gets no warning, as if the
    /// plug were pulled; anything it had not written to disk yet is lost.
    pub fn off(&self) {
        let mut switch = self.switch.lock().unwrap_or_else(PoisonError::into_inner);
        switch.off_requested = true;
        self.changed.notify_all();
    }

    /// The number of the boot that is running, or about to.
    pub fn boot(&self) -> u64 {
        let switch = self.switch.lock().unwrap_or_else(PoisonError::into_inner);
        switch.boots_ended
    }

    /// Blocks until the machine is to be turned off (true) or boot number `boot` is over (false).
    pub fn wait(&self, boot: u64) -> bool {
        let switch = self.switch.lock().unwrap_or_else(PoisonError::into_inner);
        let waiting = |switch: &mut Switch| !switch.off_requested && switch.boots_ended == boot;
        let switch = self.changed.wait_while(switch, waiting);
        switch.unwrap_or_else(PoisonError::into_inner).off_requested
    }

    /// As `wait`, giving up after `limit`. True if the machine is off or the boot is over.
    pub fn wait_for(&self, boot: u64, limit: std::time::Duration) -> bool {
        let switch = self.switch.lock().unwrap_or_else(PoisonError::into_inner);
        let waiting = |switch: &mut Switch| !switch.off_requested && switch.boots_ended == boot;
        let (_switch, timeout) = self
            .changed
            .wait_timeout_while(switch, limit, waiting)
            .unwrap_or_else(PoisonError::into_inner);
        !timeout.timed_out()
    }

    /// Marks the running boot as over, which releases whoever waits on it.
    pub fn boot_ended(&self) {
        let mut switch = self.switch.lock().unwrap_or_else(PoisonError::into_inner);
        switch.boots_ended += 1;
        self.changed.notify_all();
    }
}

pub struct Session {
    /// The host ends of the guest's devices, for whatever drives the guest from outside.
    pub controller: Arc<Controller>,
    /// The host's sound output. It opens when the guest first plays something.
    #[cfg(host_board)]
    pub speaker: crate::audio::Speaker,
    _ctl: Option<ctl::Server>,
    gamepad_forced: bool,
    /// Answers, once, whether a host pad was plugged in at start.
    pad_found: Option<Mutex<Option<std::sync::mpsc::Receiver<bool>>>>,
    pad_seen: std::sync::OnceLock<bool>,
}

impl Session {
    /// Whether the guest gets a gamepad: forced on, or a host pad was plugged in at start. Waits
    /// for the host's answer the first time.
    pub fn gamepad_attached(&self) -> bool {
        if self.controller.gamepad.is_none() {
            return false;
        }
        if self.gamepad_forced {
            return true;
        }
        *self.pad_seen.get_or_init(|| {
            let found = self.pad_found.as_ref().and_then(|found| {
                let receiver = found
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()?;
                receiver.recv().ok()
            });
            found.unwrap_or(false)
        })
    }

    /// Makes the host ends of the devices `config` asks for, and starts listening for commands.
    pub fn new(config: &Config) -> Result<Self, VmmError> {
        let mut controller = Controller::default();
        if let Some((width, height)) = config.display {
            let touch = Touch::new(width, height);
            controller.touch = Some((InputHandle::default(), Mutex::new(touch)));
            controller.keyboard = Some(InputHandle::default());
            // Frames are only kept if something can show them or ask for them.
            if config.window || config.frame_dump.is_some() || config.ctl.is_some() {
                let screen = Arc::new(Screen::default());
                if let Some(path) = &config.frame_dump {
                    frames::spawn_dump(&screen, path.clone()).map_err(VmmError::Thread)?;
                }
                controller.screen = Some(screen);
            }
        }
        if config.android.is_some() {
            let motion = Arc::new(Motion::default());
            sensors::spawn_readings(&motion).map_err(VmmError::Thread)?;
            controller.motion = Some(motion);
            controller.location = Some(Arc::default());
        }
        #[cfg(host_board)]
        let pads = match config.gamepad {
            GamepadMode::Never => None,
            GamepadMode::Auto if !config.window || config.display.is_none() => None,
            GamepadMode::Auto | GamepadMode::Always => {
                Some(crate::gamepad::HostPads::watch().map_err(VmmError::Thread)?)
            }
        };
        // Whether a host pad is there is found out while the machine is being built; the guest
        // only gets the device if one is.
        #[cfg(host_board)]
        let (pads, pad_found) = match pads {
            Some((pads, found)) => (Some(pads), Some(Mutex::new(Some(found)))),
            None => (None, None),
        };
        #[cfg(not(host_board))]
        let pad_found = None;
        if config.gamepad == GamepadMode::Always || pad_found.is_some() {
            controller.gamepad = Some(InputHandle::default());
        }
        let targets: Vec<_> = config
            .sync
            .iter()
            .map(|target| crate::instance::resolve(target))
            .collect();
        controller.followers = ctl::Followers::start(&targets).map_err(VmmError::Thread)?;
        let controller = Arc::new(controller);
        #[cfg(host_board)]
        if let Some(pads) = pads {
            pads.forward_to(&controller);
        }
        let ctl = match &config.ctl {
            Some(path) => {
                let server = ctl::Server::start(path, Arc::clone(&controller));
                Some(server.map_err(|source| VmmError::File {
                    path: path.clone(),
                    source,
                })?)
            }
            None => None,
        };
        Ok(Self {
            controller,
            gamepad_forced: config.gamepad == GamepadMode::Always,
            pad_found,
            pad_seen: std::sync::OnceLock::new(),
            #[cfg(host_board)]
            speaker: crate::audio::Speaker::default(),
            _ctl: ctl,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_timed_wait_gives_up_or_sees_the_boot_end() {
        let power = PowerSwitch::default();
        let boot = power.boot();
        assert!(!power.wait_for(boot, Duration::from_millis(10)));
        power.boot_ended();
        assert!(power.wait_for(boot, Duration::from_millis(10)));
    }

    #[test]
    fn waiting_ends_when_the_boot_does_and_reports_no_power_off() {
        let power = PowerSwitch::default();
        let boot = power.boot();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| power.wait(boot));
            power.boot_ended();
            assert!(!waiter.join().unwrap());
        });
        assert_eq!(power.boot(), boot + 1);
    }

    #[test]
    fn power_off_releases_the_waiter_and_stays_on_for_the_next_boot() {
        let power = PowerSwitch::default();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| power.wait(power.boot()));
            power.off();
            assert!(waiter.join().unwrap());
        });
        power.boot_ended();
        // A machine started after the request is turned off at once.
        assert!(power.wait(power.boot()));
    }

    #[test]
    fn session_has_the_host_ends_of_the_devices_the_configuration_asks_for() {
        let mut config = Config::new("kernel".into());
        config.display = None;
        let bare = Session::new(&config).unwrap();
        assert!(bare.controller.touch.is_none() && bare.controller.keyboard.is_none());
        assert!(bare.controller.screen.is_none() && bare.controller.motion.is_none());

        config.window = false;
        assert!(Session::new(&config).unwrap().controller.screen.is_none());

        config.display = Some((64, 48));
        assert!(Session::new(&config).unwrap().controller.screen.is_none());
        config.frame_dump = Some(std::env::temp_dir().join("emu-session-test.png"));
        let shown = Session::new(&config).unwrap();
        assert!(shown.controller.touch.is_some() && shown.controller.keyboard.is_some());
        assert!(shown.controller.screen.is_some());
        assert!(shown.controller.gamepad.is_none());
        config.gamepad = GamepadMode::Always;
        let forced = Session::new(&config).unwrap();
        assert!(forced.controller.gamepad.is_some() && forced.gamepad_attached());
        // Location and motion belong to Android; a plain kernel has neither.
        assert!(shown.controller.location.is_none());
    }
}
