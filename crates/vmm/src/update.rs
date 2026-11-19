//! The launcher's Update button: replaces this program and the files that ship
//! beside it with the newest release of the upstream repository. Only when asked: the emulator never
//! looks for updates on its own. The release is fetched with the GitHub CLI (`gh`), so a private
//! repository works with the user's own login and the program itself makes no HTTP requests.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::UPSTREAM;
use crate::copy;

/// Beside the program: the release tag it came from, so an up-to-date install is left alone.
pub const RELEASE_FILE: &str = "emu.release";
/// Where the running program is moved when it is replaced; removed at the next start.
const OLD_EXTENSION: &str = "old";
/// Scratch folder beside the program for the download.
const WORK_DIR: &str = ".emu-update";

/// The end of this host's release archive name.
#[cfg(windows)]
const ASSET_SUFFIX: Option<&str> = Some("-windows-x86_64.zip");
#[cfg(target_os = "linux")]
const ASSET_SUFFIX: Option<&str> = Some("-linux-x86_64.tar.gz");
// shortcut: the macOS release is an app bundle, which this does not swap; add that when needed.
#[cfg(not(any(windows, target_os = "linux")))]
const ASSET_SUFFIX: Option<&str> = None;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate(String),
    Updated(String),
}

/// Fetches the newest release and installs it over this program's folder.
pub fn update() -> Result<Outcome, String> {
    let suffix = ASSET_SUFFIX.ok_or(copy::UPDATE_UNSUPPORTED)?;
    let program = std::env::current_exe().map_err(|err| err.to_string())?;
    let dir = program
        .parent()
        .ok_or(copy::UPDATE_UNSUPPORTED)?
        .to_path_buf();
    let releases = format!("repos/{UPSTREAM}/releases?per_page=1");
    let tag = gh(&["api", &releases, "--jq", ".[0].tag_name"])?;
    let tag = tag.trim();
    if tag.is_empty() || tag == "null" {
        return Err(copy::UPDATE_NO_RELEASE.to_owned());
    }
    if installed(&dir).as_deref() == Some(tag) {
        return Ok(Outcome::UpToDate(tag.to_owned()));
    }
    let work = dir.join(WORK_DIR);
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|err| err.to_string())?;
    let done = download_and_install(tag, suffix, &work, &dir, &program);
    let _ = std::fs::remove_dir_all(&work);
    done.map(|()| Outcome::Updated(tag.to_owned()))
}

fn download_and_install(
    tag: &str,
    suffix: &str,
    work: &Path,
    dir: &Path,
    program: &Path,
) -> Result<(), String> {
    let pattern = format!("*{suffix}");
    let work_arg = work.to_string_lossy();
    gh(&[
        "release", "download", tag, "-R", UPSTREAM, "-p", &pattern, "-D", &work_arg,
    ])?;
    let archive = std::fs::read_dir(work)
        .map_err(|err| err.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().ends_with(suffix))
        .ok_or(copy::UPDATE_NO_ASSET)?;
    // `tar` ships with Windows 10 and later and reads zip archives too.
    let unpacked = crate::no_console_window(&mut Command::new("tar"))
        .arg("-xf")
        .arg(&archive)
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
    install(&folder, dir, program)?;
    std::fs::write(dir.join(RELEASE_FILE), tag).map_err(|err| err.to_string())
}

/// Copies every file of `from` into `to`. The running `program` cannot be overwritten on
/// Windows, so it is moved aside first and put back if its replacement cannot be copied.
fn install(from: &Path, to: &Path, program: &Path) -> Result<(), String> {
    let old = old_program(program);
    for entry in std::fs::read_dir(from).map_err(|err| err.to_string())? {
        let source = entry.map_err(|err| err.to_string())?.path();
        let Some(name) = source.file_name() else {
            continue;
        };
        if !source.is_file() {
            continue;
        }
        let target = to.join(name);
        if Some(name) == program.file_name() {
            let _ = std::fs::remove_file(&old);
            std::fs::rename(program, &old).map_err(|err| err.to_string())?;
            if let Err(err) = std::fs::copy(&source, &target) {
                let _ = std::fs::rename(&old, program);
                return Err(err.to_string());
            }
        } else {
            std::fs::copy(&source, &target).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

fn old_program(program: &Path) -> PathBuf {
    let mut name = program.as_os_str().to_owned();
    name.push(".");
    name.push(OLD_EXTENSION);
    PathBuf::from(name)
}

/// The tag the program's folder was installed from, if it says.
fn installed(dir: &Path) -> Option<String> {
    let tag = std::fs::read_to_string(dir.join(RELEASE_FILE)).ok()?;
    Some(tag.trim().to_owned())
}

/// Removes the program a previous update moved aside, now that it is no longer running.
pub fn remove_leftovers() {
    if let Ok(program) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_program(&program));
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
        let dir = std::env::temp_dir().join(format!("emu-update-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_replaces_files_and_moves_the_program_aside() {
        let from = scratch("from");
        let to = scratch("to");
        std::fs::write(from.join("emu.exe"), "new program").unwrap();
        std::fs::write(from.join("kernel-x86_64"), "new kernel").unwrap();
        std::fs::create_dir(from.join("skipped")).unwrap();
        let program = to.join("emu.exe");
        std::fs::write(&program, "old program").unwrap();
        std::fs::write(to.join("kernel-x86_64"), "old kernel").unwrap();
        std::fs::write(to.join("mine.txt"), "kept").unwrap();

        install(&from, &to, &program).unwrap();

        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();
        assert_eq!(read(program.clone()), "new program");
        assert_eq!(read(old_program(&program)), "old program");
        assert_eq!(read(to.join("kernel-x86_64")), "new kernel");
        assert_eq!(read(to.join("mine.txt")), "kept");
        assert!(!to.join("skipped").exists());
    }

    #[test]
    fn the_installed_tag_is_read_back_trimmed() {
        let dir = scratch("tag");
        assert_eq!(installed(&dir), None);
        std::fs::write(dir.join(RELEASE_FILE), "v1.2.3\n").unwrap();
        assert_eq!(installed(&dir).as_deref(), Some("v1.2.3"));
    }
}
