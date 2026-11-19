//! What the emulator does to Android once it has booted: stops the image's own log streaming when
//! nobody reads it (it costs about one guest CPU), switches Bluetooth off (the board has no
//! Bluetooth controller, so the stack crashes and restarts every few seconds), switches UWB off
//! (its failure files bug reports with screenshots), turns off what a game machine does not need
//! (ad services, printing, screensavers), gives Android back the panel's own shape when there is
//! no window to give it one, and installs the apps a release carries beside the program (`apps/`,
//! such as an app store) the first time a data folder boots.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread::JoinHandle;

use crate::adb;
use crate::config::{BUNDLED_APPS_DIR, INSTALLED_APPS_FILE, SETUP_POLL, SETUP_TIMEOUT};
use crate::copy;
use crate::session::PowerSwitch;

/// Prints `1` once Android has finished booting.
const BOOT_COMPLETED: &str = "getprop sys.boot_completed";
const BOOTED: &str = "1";
/// Stops Cuttlefish's `seriallogging` service, which copies every log line to a console port.
const STOP_LOG_STREAM: &str = "su 0 stop seriallogging";
/// Switches Bluetooth off and keeps it off: the image's Bluetooth stack aborts with "Can't start
/// HAL" when it finds no controller, and Android starts it again at once.
const BLUETOOTH_OFF: &str = "cmd bluetooth_manager disable; settings put global bluetooth_on 0; \
     settings put global ble_scan_always_enabled 0";
/// Switches UWB off: the board has no UWB radio, the service fails to start, and on that error
/// Android files a bug report (a screenshot and about half a minute of work) and shows a
/// notification for it. Reports it already made are deleted, and their notification goes with
/// the shell app's.
const UWB_OFF: &str = "cmd uwb disable-uwb; \
     device_config put uwb device_error_bugreport_enabled false; \
     su 0 rm -rf /data/user_de/0/com.android.shell/files/bugreports; \
     am force-stop com.android.shell";
/// Turns off parts of the image a game machine has no use for, every boot (an update of the image
/// could bring them back): Android's ad services and the on-device ad and learning services that
/// feed them, remote key provisioning (it calls Google's servers), device-lock financing, printing,
/// screensavers, satellite messaging and demo and tracing apps. `pm enable <package>` brings one
/// back. Packages the image does not have, or will not let go, are skipped.
const GAMING_TRIM: &str = "for package in \
     com.android.adservices.api com.android.federatedcompute.services \
     com.android.ondevicepersonalization.services com.android.rkpdapp \
     com.android.devicelockcontroller com.android.printspooler com.android.bips \
     com.android.printservice.recommendation com.android.dreams.basic \
     com.android.dreams.phototable com.google.android.telephony.satellite \
     com.android.threadnetwork.demoapp com.android.traceur com.android.wallpaper.livepicker; \
     do pm disable-user --user 0 $package >/dev/null 2>&1; done";
/// Stops the image's radio and Thread network services. They are Cuttlefish's, made for its host
/// tools: the radio service exits at once without Cuttlefish's modem simulator, and the Thread
/// service without its simulated radio, so init restarts each every five seconds for good.
/// `com.android.phone` still waits for the radio and is restarted by Android now and then; that
/// needs a modem, which this board does not have.
const RADIOS_OFF: &str = "su 0 cmd thread_network disable; \
     su 0 cmd thread_network force-stop-ot-daemon enabled; \
     su 0 setprop ctl.stop vendor.threadnetwork_hal; su 0 setprop ctl.stop vendor.ril-daemon";
/// Makes apps share memory through memfd. The guest kernel has no `/dev/ashmem`, and without this
/// Android still tries it: every shared bitmap (a notification's icon, say) and database cursor
/// fails, and apps that post notifications crash in `Bitmap_copyAshmem`. Read once per process,
/// so it reaches apps started after setup (every app the user opens).
const SHARED_MEMORY: &str = "su 0 setprop sys.use_memfd true";
/// Bundled apps are files with this ending.
const APK_EXTENSION: &str = "apk";

/// The apps beside the program, by file name, that `installed` (one name per line) does not list.
pub fn to_install(bundled: &[String], installed: &str) -> Vec<String> {
    let done: BTreeSet<&str> = installed.lines().map(str::trim).collect();
    bundled
        .iter()
        .filter(|name| !done.contains(name.as_str()))
        .cloned()
        .collect()
}

/// The `.apk` files in `dir`, by name, sorted.
fn bundled_apps(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry.path().extension().and_then(|ext| ext.to_str()) == Some(APK_EXTENSION)
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The folder of bundled apps: `apps/` beside the program, if it has one.
fn apps_dir() -> Option<PathBuf> {
    crate::config::bundled(BUNDLED_APPS_DIR).map(|(_, path)| path)
}

/// Waits for Android to boot, then sets it up, on its own thread, until `done` is set; again
/// after each restart of the guest, since a restart undoes what was set.
/// `data` is the instance's Android data folder, where installed bundled apps are noted.
pub fn spawn(
    adb_port: Arc<AtomicU16>,
    power: Arc<PowerSwitch>,
    done: Arc<AtomicBool>,
    keep_log_stream: bool,
    reset_shape: bool,
    data: Option<PathBuf>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(copy::SETUP_THREAD.into())
        .spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let boot = power.boot();
                let Some(port) = booted(&adb_port, &done) else {
                    return;
                };
                set_up(port, keep_log_stream, reset_shape, data.as_deref());
                // Until this boot is over; `done` is looked at now and then meanwhile.
                while !done.load(Ordering::SeqCst) && !power.wait_for(boot, SETUP_POLL) {}
            }
        })
}

/// Waits until Android has booted and gives its adb port, or `None` once `done` is set.
fn booted(adb_port: &AtomicU16, done: &AtomicBool) -> Option<u16> {
    loop {
        if done.load(Ordering::SeqCst) {
            return None;
        }
        let port = adb_port.load(Ordering::SeqCst);
        let booted = port != 0
            && adb::output_within(port, BOOT_COMPLETED, SETUP_TIMEOUT)
                .is_ok_and(|printed| printed.trim() == BOOTED);
        if booted {
            return Some(port);
        }
        std::thread::sleep(SETUP_POLL);
    }
}

fn set_up(port: u16, keep_log_stream: bool, reset_shape: bool, data: Option<&Path>) {
    let mut steps = vec![
        SHARED_MEMORY,
        BLUETOOTH_OFF,
        UWB_OFF,
        RADIOS_OFF,
        GAMING_TRIM,
    ];
    // Without a window to give Android its shape, a logical size from an earlier run would
    // leave bars in every frame; a window sets (and reads back) the shape itself.
    if reset_shape {
        steps.push(crate::fill::WM_SIZE_RESET);
    }
    if !keep_log_stream {
        steps.push(STOP_LOG_STREAM);
    }
    for step in steps {
        if let Err(err) = adb::output(port, step) {
            log::warn!("{}", copy::setup_failed(&err));
        }
    }
    if let (Some(apps), Some(data)) = (apps_dir(), data) {
        install_bundled(port, &apps, data);
    }
}

fn install_bundled(port: u16, apps: &Path, data: &Path) {
    let noted = data.join(INSTALLED_APPS_FILE);
    let installed = std::fs::read_to_string(&noted).unwrap_or_default();
    let mut list = installed.clone();
    for name in to_install(&bundled_apps(apps), &installed) {
        match crate::install::install(port, &apps.join(&name)) {
            Ok(()) => {
                eprintln!("{}", copy::bundled_installed(&name));
                list.push_str(&name);
                list.push('\n');
            }
            Err(err) => eprintln!("{}", copy::install_failed(&apps.join(&name), &err)),
        }
    }
    if list != installed
        && let Err(err) = std::fs::write(&noted, list)
    {
        log::warn!("{}", copy::file(&noted, &err));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_apps_are_installed_once() {
        let bundled = ["aurora.apk".to_owned(), "fdroid.apk".to_owned()];
        assert_eq!(to_install(&bundled, ""), bundled);
        assert_eq!(to_install(&bundled, "aurora.apk\n"), ["fdroid.apk"]);
        assert!(to_install(&bundled, "fdroid.apk\naurora.apk\n").is_empty());
    }
}
