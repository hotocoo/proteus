//! Key maps that follow the app in front: a folder of maps named after apps' package names
//! (`com.example.game.toml`), with `default.toml` for every other app.
//!
//! Which app is in front is asked of the guest through `adb` every `FOREGROUND_POLL`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread::JoinHandle;

use crate::adb;
use crate::config::{FOREGROUND_POLL, FOREGROUND_TIMEOUT};
use crate::copy;

/// The guest command that prints the app in front, in a line like
/// `ResumedActivity: ActivityRecord{201862590 u0 com.android.launcher3/.Launcher t7}` (older
/// releases call it `mResumedActivity`). Its other error output is dropped: `grep` stopping early
/// makes `dumpsys` complain of a broken pipe.
const FOREGROUND_COMMAND: &str =
    "sh -c 'dumpsys activity activities 2>/dev/null | grep -m1 \"ResumedActivity: \"'";
const FOREGROUND_FIELD: &str = "ResumedActivity: ActivityRecord{";
const DEFAULT_MAP: &str = "default";
const MAP_EXTENSION: &str = "toml";

/// The package name of the app in front, from what `FOREGROUND_COMMAND` printed.
pub fn foreground_package(printed: &str) -> Option<&str> {
    let (_, record) = printed.split_once(FOREGROUND_FIELD)?;
    let component = record.split_whitespace().nth(2)?;
    let (package, _) = component.split_once('/')?;
    // The name comes from the guest and becomes part of a file name, so it is held to the
    // characters package names are made of.
    let valid = !package.is_empty()
        && package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        && !package.starts_with('.');
    valid.then_some(package)
}

/// The map file for `package` in `dir`: its own, else the default, else none.
pub fn map_for(dir: &Path, package: &str) -> Option<PathBuf> {
    [package, DEFAULT_MAP]
        .iter()
        .map(|name| dir.join(format!("{name}.{MAP_EXTENSION}")))
        .find(|path| path.is_file())
}

/// Asks the guest which app is in front every `FOREGROUND_POLL`, and calls `changed` with its
/// package name whenever that changes, until `done` is set.
pub fn watch(
    adb_port: Arc<AtomicU16>,
    done: Arc<AtomicBool>,
    changed: impl Fn(String) + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(copy::FOREGROUND_THREAD.into())
        .spawn(move || {
            let mut last = String::new();
            while !done.load(Ordering::SeqCst) {
                let port = adb_port.load(Ordering::SeqCst);
                // Until Android is up there is no answer; that is not worth reporting.
                if let Ok(printed) =
                    adb::output_within(port, FOREGROUND_COMMAND, FOREGROUND_TIMEOUT)
                    && let Some(package) = foreground_package(&printed)
                    && package != last
                {
                    last = package.to_owned();
                    changed(last.clone());
                }
                std::thread::sleep(FOREGROUND_POLL);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_in_front_is_read_from_the_activity_record() {
        let printed = "  ResumedActivity: ActivityRecord{201862590 u0 \
                       com.android.launcher3/.uioverrides.QuickstepLauncher t7}\n";
        assert_eq!(foreground_package(printed), Some("com.android.launcher3"));
        let older = "mResumedActivity: ActivityRecord{1 u0 com.game/.Main t3}";
        assert_eq!(foreground_package(older), Some("com.game"));
        assert_eq!(foreground_package(""), None);
        // Nothing that could step out of the folder of maps.
        let sneaky = "ResumedActivity: ActivityRecord{1 u0 ../../etc/x t1}";
        assert_eq!(foreground_package(sneaky), None);
        let hidden = "ResumedActivity: ActivityRecord{1 u0 .x/.A t1}";
        assert_eq!(foreground_package(hidden), None);
    }

    #[test]
    fn an_app_gets_its_own_map_else_the_default_else_none() {
        let dir =
            std::env::temp_dir().join(format!("proteus-profiles-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(map_for(&dir, "com.game"), None);
        std::fs::write(dir.join("default.toml"), "").unwrap();
        assert_eq!(map_for(&dir, "com.game"), Some(dir.join("default.toml")));
        std::fs::write(dir.join("com.game.toml"), "").unwrap();
        assert_eq!(map_for(&dir, "com.game"), Some(dir.join("com.game.toml")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
