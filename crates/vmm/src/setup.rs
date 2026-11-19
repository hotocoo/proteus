//! What the emulator does to Android once it has booted: stops the image's own log streaming when
//! nobody reads it (it costs about one guest CPU), switches Bluetooth off (the board has no
//! Bluetooth controller, so the stack crashes and restarts every few seconds), and installs the apps a release carries beside
//! the program (`apps/`, such as an app store) the first time a data folder boots.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread::JoinHandle;

use crate::adb;
use crate::config::{BUNDLED_APPS_DIR, INSTALLED_APPS_FILE, SETUP_POLL, SETUP_TIMEOUT};
use crate::copy;

/// Prints `1` once Android has finished booting.
const BOOT_COMPLETED: &str = "getprop sys.boot_completed";
const BOOTED: &str = "1";
/// Stops Cuttlefish's `seriallogging` service, which copies every log line to a console port.
const STOP_LOG_STREAM: &str = "su 0 stop seriallogging";
/// Switches Bluetooth off and keeps it off: the image's Bluetooth stack aborts with "Can't start
/// HAL" when it finds no controller, and Android starts it again at once.
const BLUETOOTH_OFF: &str = "cmd bluetooth_manager disable; settings put global bluetooth_on 0; \
     settings put global ble_scan_always_enabled 0";
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
    let program = std::env::current_exe().ok()?.canonicalize().ok()?;
    Some(program.parent()?.join(BUNDLED_APPS_DIR)).filter(|dir| dir.is_dir())
}

/// Waits for Android to boot, then sets it up, on its own thread, until `done` is set.
/// `data` is the instance's Android data folder, where installed bundled apps are noted.
pub fn spawn(
    adb_port: Arc<AtomicU16>,
    done: Arc<AtomicBool>,
    keep_log_stream: bool,
    data: Option<PathBuf>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(copy::SETUP_THREAD.into())
        .spawn(move || {
            let port = loop {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                let port = adb_port.load(Ordering::SeqCst);
                let booted = port != 0
                    && adb::output_within(port, BOOT_COMPLETED, SETUP_TIMEOUT)
                        .is_ok_and(|printed| printed.trim() == BOOTED);
                if booted {
                    break port;
                }
                std::thread::sleep(SETUP_POLL);
            };
            let mut steps = vec![BLUETOOTH_OFF];
            if !keep_log_stream {
                steps.push(STOP_LOG_STREAM);
            }
            for step in steps {
                if let Err(err) = adb::output(port, step) {
                    log::warn!("{}", copy::setup_failed(&err));
                }
            }
            if let (Some(apps), Some(data)) = (apps_dir(), data) {
                install_bundled(port, &apps, &data);
            }
        })
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
