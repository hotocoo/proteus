//! The launcher's Update button: replaces this program and the files that ship
//! beside it with the newest release of the upstream repository. Only when asked: the emulator never
//! looks for updates on its own. The release is fetched with the GitHub CLI (`gh`), so a private
//! repository works with the user's own login and the program itself makes no HTTP requests.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::UPSTREAM;
use crate::copy;

/// Beside the program: the release tag it came from, so an up-to-date install is left alone.
pub const RELEASE_FILE: &str = "proteus.release";
/// Added to the name of a file an update replaces, while it may still be open; removed at the next start.
const OLD_EXTENSION: &str = "old";
/// Scratch folder beside the program for the download.
const WORK_DIR: &str = ".proteus-update";

/// The end of this host's release archive name.
#[cfg(windows)]
const ASSET_SUFFIX: Option<&str> = Some("-windows-x86_64.zip");
#[cfg(target_os = "linux")]
const ASSET_SUFFIX: Option<&str> = Some("-linux-x86_64.tar.gz");
#[cfg(target_os = "macos")]
const ASSET_SUFFIX: Option<&str> = Some("-macos-arm64-app.zip");
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
const ASSET_SUFFIX: Option<&str> = None;
/// On macOS the release is an app bundle, swapped whole; its tag is inside it, in Resources.
#[cfg(target_os = "macos")]
const BUNDLE_EXTENSION: &str = "app";
#[cfg(target_os = "macos")]
const BUNDLE_RESOURCES: &str = "Contents/Resources";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate(String),
    Updated(String),
}

/// Fetches the newest release and installs it over this program's folder.
pub fn update() -> Result<Outcome, String> {
    let suffix = ASSET_SUFFIX.ok_or(copy::UPDATE_UNSUPPORTED)?;
    let program = std::env::current_exe().map_err(|err| err.to_string())?;
    #[cfg(target_os = "macos")]
    return update_bundle(&program, suffix);
    #[cfg(not(target_os = "macos"))]
    update_folder(&program, suffix)
}

/// The newest release's tag.
fn newest() -> Result<String, String> {
    let releases = format!("repos/{UPSTREAM}/releases?per_page=1");
    let tag = gh(&["api", &releases, "--jq", ".[0].tag_name"])?;
    let tag = tag.trim();
    if tag.is_empty() || tag == "null" {
        return Err(copy::UPDATE_NO_RELEASE.to_owned());
    }
    Ok(tag.to_owned())
}

/// Downloads this host's package of release `tag` into `work`, which it empties first.
fn download(tag: &str, suffix: &str, work: &Path) -> Result<PathBuf, String> {
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work).map_err(|err| err.to_string())?;
    let pattern = format!("*{suffix}");
    let work_arg = work.to_string_lossy();
    gh(&[
        "release", "download", tag, "-R", UPSTREAM, "-p", &pattern, "-D", &work_arg,
    ])?;
    std::fs::read_dir(work)
        .map_err(|err| err.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().ends_with(suffix))
        .ok_or_else(|| copy::UPDATE_NO_ASSET.to_owned())
}

/// The app bundle `program` runs from.
#[cfg(target_os = "macos")]
fn bundle_of(program: &Path) -> Option<PathBuf> {
    program
        .ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext == BUNDLE_EXTENSION))
        .map(Path::to_path_buf)
}

/// Replaces the app bundle this program runs from with the newest release's. The running program
/// keeps working from the old bundle, which is moved aside and removed at the next start.
#[cfg(target_os = "macos")]
fn update_bundle(program: &Path, suffix: &str) -> Result<Outcome, String> {
    let bundle = bundle_of(program).ok_or(copy::UPDATE_UNSUPPORTED)?;
    let tag = newest()?;
    if installed(&bundle.join(BUNDLE_RESOURCES)).as_deref() == Some(tag.as_str()) {
        return Ok(Outcome::UpToDate(tag));
    }
    let parent = bundle.parent().ok_or(copy::UPDATE_UNSUPPORTED)?;
    let work = parent.join(WORK_DIR);
    let done = download(&tag, suffix, &work).and_then(|archive| {
        // `ditto` keeps the bundle's signature and links as they were packed.
        let unpacked = Command::new("ditto")
            .arg("-x")
            .arg("-k")
            .arg(&archive)
            .arg(&work)
            .status()
            .map_err(|err| err.to_string())?;
        if !unpacked.success() {
            return Err(copy::UPDATE_UNPACK_FAILED.to_owned());
        }
        let new = std::fs::read_dir(&work)
            .map_err(|err| err.to_string())?
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == BUNDLE_EXTENSION))
            .ok_or(copy::UPDATE_NO_ASSET)?;
        swap_bundle(&new, &bundle)
    });
    let _ = std::fs::remove_dir_all(&work);
    done.map(|()| Outcome::Updated(tag))
}

/// Puts bundle `new` where `bundle` is, moving `bundle` aside; puts it back if that fails.
#[cfg(target_os = "macos")]
fn swap_bundle(new: &Path, bundle: &Path) -> Result<(), String> {
    let old = moved_aside(bundle);
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(bundle, &old).map_err(|err| err.to_string())?;
    if let Err(err) = std::fs::rename(new, bundle) {
        let _ = std::fs::rename(&old, bundle);
        return Err(err.to_string());
    }
    Ok(())
}

/// Replaces the files of the folder this program runs from with the newest release's.
#[cfg(not(target_os = "macos"))]
fn update_folder(program: &Path, suffix: &str) -> Result<Outcome, String> {
    let dir = program
        .parent()
        .ok_or(copy::UPDATE_UNSUPPORTED)?
        .to_path_buf();
    let tag = newest()?;
    if installed(&dir).as_deref() == Some(tag.as_str()) {
        return Ok(Outcome::UpToDate(tag));
    }
    let work = dir.join(WORK_DIR);
    let done = download(&tag, suffix, &work)
        .and_then(|archive| download_and_install(&tag, &archive, &work, &dir));
    let _ = std::fs::remove_dir_all(&work);
    done.map(|()| Outcome::Updated(tag))
}

#[cfg(not(target_os = "macos"))]
fn download_and_install(tag: &str, archive: &Path, work: &Path, dir: &Path) -> Result<(), String> {
    // `tar` ships with Windows 10 and later and reads zip archives too.
    let unpacked = crate::no_console_window(&mut Command::new("tar"))
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(work)
        .status()
        .map_err(|err| err.to_string())?;
    if !unpacked.success() {
        return Err(copy::UPDATE_UNPACK_FAILED.to_owned());
    }
    let folder = std::fs::read_dir(work)
        .map_err(|err| err.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .ok_or(copy::UPDATE_NO_ASSET)?;
    install(&folder, dir)?;
    std::fs::write(dir.join(RELEASE_FILE), tag).map_err(|err| err.to_string())
}

#[cfg(not(target_os = "macos"))]
/// Copies every file of `from` into `to`. A running program and the libraries it has loaded
/// (gfxstream's renderer) cannot be overwritten on Windows, so each file being replaced is moved
/// aside first and put back if its replacement cannot be copied.
fn install(from: &Path, to: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(from).map_err(|err| err.to_string())? {
        let source = entry.map_err(|err| err.to_string())?.path();
        let Some(name) = source.file_name() else {
            continue;
        };
        if !source.is_file() {
            continue;
        }
        let target = to.join(name);
        let old = moved_aside(&target);
        let moved = target.exists();
        if moved {
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&target, &old).map_err(|err| err.to_string())?;
        }
        if let Err(err) = std::fs::copy(&source, &target) {
            if moved {
                let _ = std::fs::rename(&old, &target);
            }
            return Err(err.to_string());
        }
    }
    Ok(())
}

/// Where `path` is moved while it is replaced.
fn moved_aside(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(OLD_EXTENSION);
    PathBuf::from(name)
}

/// The tag the program's folder was installed from, if it says.
fn installed(dir: &Path) -> Option<String> {
    let tag = std::fs::read_to_string(dir.join(RELEASE_FILE)).ok()?;
    Some(tag.trim().to_owned())
}

/// Removes the files a previous update moved aside, now that nothing has them open.
pub fn remove_leftovers() {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|program| program.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_some_and(|ext| ext == OLD_EXTENSION) {
            let _ = std::fs::remove_file(path);
        }
    }
    // The bundle an update replaced.
    #[cfg(target_os = "macos")]
    if let Some(bundle) = std::env::current_exe().ok().as_deref().and_then(bundle_of) {
        let _ = std::fs::remove_dir_all(moved_aside(&bundle));
    }
}

fn gh(args: &[&str]) -> Result<String, String> {
    let output = crate::no_console_window(&mut Command::new("gh"))
        .args(args)
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                copy::UPDATE_NEEDS_GH.to_owned()
            } else {
                err.to_string()
            }
        })?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(copy::update_gh_failed(&why));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("proteus-update-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn install_replaces_files_and_moves_each_replaced_file_aside() {
        let from = scratch("from");
        let to = scratch("to");
        std::fs::write(from.join("proteus.exe"), "new program").unwrap();
        std::fs::write(from.join("kernel-x86_64"), "new kernel").unwrap();
        std::fs::create_dir(from.join("skipped")).unwrap();
        let program = to.join("proteus.exe");
        std::fs::write(&program, "old program").unwrap();
        std::fs::write(to.join("kernel-x86_64"), "old kernel").unwrap();
        std::fs::write(to.join("mine.txt"), "kept").unwrap();

        install(&from, &to).unwrap();

        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();
        assert_eq!(read(program.clone()), "new program");
        assert_eq!(read(moved_aside(&program)), "old program");
        assert_eq!(read(to.join("kernel-x86_64")), "new kernel");
        assert_eq!(read(moved_aside(&to.join("kernel-x86_64"))), "old kernel");
        assert_eq!(read(to.join("mine.txt")), "kept");
        assert!(!to.join("skipped").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_new_bundle_takes_the_old_ones_place_and_the_old_one_is_kept_aside() {
        let dir = scratch("bundle");
        let bundle = dir.join("Proteus.app");
        let new = dir.join("work").join("Proteus.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(new.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join("Contents/MacOS/proteus"), "old").unwrap();
        std::fs::write(new.join("Contents/MacOS/proteus"), "new").unwrap();
        let program = bundle.join("Contents/MacOS/proteus");
        assert_eq!(bundle_of(&program).as_deref(), Some(bundle.as_path()));

        swap_bundle(&new, &bundle).unwrap();

        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();
        assert_eq!(read(program), "new");
        assert_eq!(
            read(moved_aside(&bundle).join("Contents/MacOS/proteus")),
            "old"
        );
        assert!(!new.exists());
    }

    #[test]
    fn the_installed_tag_is_read_back_trimmed() {
        let dir = scratch("tag");
        assert_eq!(installed(&dir), None);
        std::fs::write(dir.join(RELEASE_FILE), "v1.2.3\n").unwrap();
        assert_eq!(installed(&dir).as_deref(), Some("v1.2.3"));
    }
}
