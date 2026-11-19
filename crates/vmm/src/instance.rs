//! Named instances: several emulators, each with its own data and control socket.
//!
//! An instance is a folder `instances/<name>/` under the emulator's home folder, holding the
//! options it was created with (`args`, one per line), its Android data (`data/`) and, while it
//! runs, its control socket (`ctl.sock`). System and vendor images are read from wherever the
//! options point, read-only, so instances share them. Each running instance gets the next free adb
//! port.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::config::{
    EMU_HOME_VARIABLE, INSTANCE_NAME_MAX, INSTANCES_DIR, SOCKET_PATH_MAX, home_default,
};
use crate::copy;

const ARGS_FILE: &str = "args";
const DATA_DIR: &str = "data";
const SOCKET_FILE: &str = "ctl.sock";
/// Options an instance sets for itself; they cannot be given when it is created.
const OWN_OPTIONS: [&str; 2] = ["--data-dir", "--ctl"];
const YES: &str = "y";
/// Options whose value is a file or folder. They are made absolute when an instance is created, so
/// that it starts the same from any folder.
const PATH_OPTIONS: [&str; 11] = [
    "--kernel",
    "--initrd",
    "--android",
    "--disk",
    "--ro-disk",
    "--keymap",
    "--keymap-dir",
    "--frame-dump",
    "--logcat",
    "--blocklist",
    "--allowlist",
];
/// `--sync` names an instance or a socket; only a socket that exists is a path.
const SYNC_OPTION: &str = "--sync";

/// `args` with every path-valued option's relative value made relative to `cwd` instead.
fn absolute(args: &[String], cwd: &Path) -> Vec<String> {
    let fix = |value: &str| cwd.join(value).to_string_lossy().into_owned();
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (option, inline) = match arg.split_once('=') {
            Some((option, value)) if option.starts_with("--") => (option, Some(value)),
            _ => (arg.as_str(), None),
        };
        let is_path = |value: &str| {
            let relative = Path::new(value).is_relative();
            relative
                && (PATH_OPTIONS.contains(&option)
                    || (option == SYNC_OPTION && cwd.join(value).exists()))
        };
        match inline {
            Some(value) if is_path(value) => out.push(format!("{option}={}", fix(value))),
            Some(_) => out.push(arg.clone()),
            None => {
                out.push(arg.clone());
                let takes_path = PATH_OPTIONS.contains(&option) || option == SYNC_OPTION;
                if let Some(value) = takes_path.then(|| iter.next()).flatten() {
                    out.push(if is_path(value) {
                        fix(value)
                    } else {
                        value.clone()
                    });
                }
            }
        }
    }
    out
}

/// The folder all instances live in.
pub fn root() -> io::Result<PathBuf> {
    let home = std::env::var_os(EMU_HOME_VARIABLE).map(PathBuf::from);
    let home = home.or_else(home_default);
    let home =
        home.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, copy::INSTANCE_NO_HOME))?;
    Ok(home.join(INSTANCES_DIR))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= INSTANCE_NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The folder of the instance called `name` under `root`.
fn folder(root: &Path, name: &str) -> io::Result<PathBuf> {
    if !valid_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            copy::INSTANCE_BAD_NAME,
        ));
    }
    Ok(root.join(name))
}

/// The control socket of an instance.
pub fn socket(root: &Path, name: &str) -> io::Result<PathBuf> {
    Ok(folder(root, name)?.join(SOCKET_FILE))
}

/// What `emu ctl <target>` means: the socket of an instance with that name, if there is one and
/// `target` is not a path itself; otherwise `target` as a socket path.
pub fn resolve(target: &Path) -> PathBuf {
    let name = target.to_str().filter(|name| valid_name(name));
    let named = name.and_then(|name| {
        let root = root().ok()?;
        folder(&root, name).ok().filter(|dir| dir.is_dir())?;
        socket(&root, name).ok()
    });
    match named {
        Some(socket) if !target.exists() => socket,
        _ => target.to_owned(),
    }
}

/// Whether an instance is running: something answers on its socket.
pub fn running(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Makes a new instance that starts with `args`, whose relative paths are relative to `cwd`.
pub fn create(root: &Path, name: &str, args: &[String], cwd: &Path) -> io::Result<()> {
    let dir = folder(root, name)?;
    let args = &absolute(args, cwd);
    check_args(args)?;
    // The socket's path has to fit the system's limit, which is short.
    if socket(root, name)?.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            copy::INSTANCE_PATH_TOO_LONG,
        ));
    }
    std::fs::create_dir_all(root)?;
    // Fails if it exists, so an instance is never overwritten.
    std::fs::create_dir(&dir).map_err(|err| match err.kind() {
        io::ErrorKind::AlreadyExists => io::Error::new(err.kind(), copy::INSTANCE_EXISTS),
        _ => err,
    })?;
    let mut text = args.join("\n");
    text.push('\n');
    std::fs::write(dir.join(ARGS_FILE), text)
}

/// The folder of the instance called `name`, where it keeps its data and the panel its files.
pub fn dir(root: &Path, name: &str) -> io::Result<PathBuf> {
    folder(root, name)
}

/// The options an instance was created with, or last saved with.
pub fn args(root: &Path, name: &str) -> io::Result<Vec<String>> {
    let text =
        std::fs::read_to_string(folder(root, name)?.join(ARGS_FILE)).map_err(|err| {
            match err.kind() {
                io::ErrorKind::NotFound => io::Error::new(err.kind(), copy::INSTANCE_UNKNOWN),
                _ => err,
            }
        })?;
    Ok(text.lines().map(str::to_owned).collect())
}

/// Saves new options for an instance that exists; it starts with them next time.
pub fn set_args(root: &Path, name: &str, args: &[String]) -> io::Result<()> {
    let dir = folder(root, name)?;
    if !dir.join(ARGS_FILE).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            copy::INSTANCE_UNKNOWN,
        ));
    }
    check_args(args)?;
    let mut text = args.join("\n");
    text.push('\n');
    std::fs::write(dir.join(ARGS_FILE), text)
}

/// Refuses options an instance cannot keep: its own, and any with a line break.
fn check_args(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| OWN_OPTIONS.contains(&arg.as_str())) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            copy::INSTANCE_OWN_OPTIONS,
        ));
    }
    if args.iter().any(|arg| arg.contains('\n')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            copy::INSTANCE_BAD_ARGUMENT,
        ));
    }
    Ok(())
}

/// The size of an instance's Android data partition as it is now, if it has been made.
pub fn userdata_bytes(root: &Path, name: &str) -> Option<u64> {
    let file = folder(root, name)
        .ok()?
        .join(DATA_DIR)
        .join(crate::android::USERDATA_FILE);
    std::fs::metadata(file).ok().map(|meta| meta.len())
}

/// Deletes a stopped instance's Android data, so it next starts as a new phone. Its options stay.
pub fn wipe_data(root: &Path, name: &str) -> io::Result<()> {
    let dir = folder(root, name)?;
    if running(&dir.join(SOCKET_FILE)) {
        return Err(io::Error::other(copy::INSTANCE_RUNNING));
    }
    match std::fs::remove_dir_all(dir.join(DATA_DIR)) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// The arguments that start an instance: the ones it was created with, its own data folder and
/// socket, then `extra`.
pub fn start_args(root: &Path, name: &str, extra: &[OsString]) -> io::Result<Vec<OsString>> {
    let dir = folder(root, name)?;
    let text = std::fs::read_to_string(dir.join(ARGS_FILE)).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => io::Error::new(err.kind(), copy::INSTANCE_UNKNOWN),
        _ => err,
    })?;
    let mut args: Vec<OsString> = text.lines().map(OsString::from).collect();
    args.extend([
        OWN_OPTIONS[0].into(),
        dir.join(DATA_DIR).into(),
        OWN_OPTIONS[1].into(),
        dir.join(SOCKET_FILE).into(),
    ]);
    args.extend(extra.iter().cloned());
    Ok(args)
}

/// The instances there are, by name, and whether each is running.
pub fn list(root: &Path) -> io::Result<Vec<(String, bool)>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if valid_name(&name) && entry.path().join(ARGS_FILE).is_file() {
            let up = running(&entry.path().join(SOCKET_FILE));
            found.push((name, up));
        }
    }
    found.sort();
    Ok(found)
}

/// Deletes an instance and its Android data, for good. Asks on `ask`/`answer` first unless
/// `confirmed`. A running instance is not deleted.
pub fn remove(
    root: &Path,
    name: &str,
    confirmed: bool,
    ask: &mut dyn Write,
    answer: &mut dyn BufRead,
) -> io::Result<bool> {
    let dir = folder(root, name)?;
    if !dir.join(ARGS_FILE).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            copy::INSTANCE_UNKNOWN,
        ));
    }
    if running(&dir.join(SOCKET_FILE)) {
        return Err(io::Error::other(copy::INSTANCE_RUNNING));
    }
    if !confirmed {
        write!(ask, "{}", copy::instance_confirm_delete(name))?;
        ask.flush()?;
        let mut reply = String::new();
        answer.read_line(&mut reply)?;
        if !reply.trim().eq_ignore_ascii_case(YES) {
            return Ok(false);
        }
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("emu-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn an_instance_starts_with_its_own_options_data_and_socket() {
        let root = scratch("an_instance_starts_w");
        let args = [
            "--kernel".to_owned(),
            "k".to_owned(),
            "--android".to_owned(),
            "a b".to_owned(),
        ];
        create(&root, "farm-1", &args, Path::new("/")).unwrap();
        assert_eq!(
            create(&root, "farm-1", &args, Path::new("/"))
                .unwrap_err()
                .to_string(),
            copy::INSTANCE_EXISTS
        );
        let started = start_args(&root, "farm-1", &["--headless".into()]).unwrap();
        let dir = root.join("farm-1");
        let expected: Vec<OsString> = vec![
            "--kernel".into(),
            "/k".into(),
            "--android".into(),
            "/a b".into(),
            "--data-dir".into(),
            dir.join("data").into(),
            "--ctl".into(),
            dir.join("ctl.sock").into(),
            "--headless".into(),
        ];
        assert_eq!(started, expected);
        assert_eq!(list(&root).unwrap(), [("farm-1".to_owned(), false)]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn relative_paths_are_made_absolute_and_nothing_else_is_touched() {
        let words = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let cwd = Path::new("/home/me");
        let given = words(&[
            "--kernel",
            "kernel-arm64",
            "--android=img",
            "--ram-mib",
            "2048",
            "--logcat",
            "/abs/log.txt",
            "--sync",
            "farm",
            "--headless",
        ]);
        let expected = words(&[
            "--kernel",
            "/home/me/kernel-arm64",
            "--android=/home/me/img",
            "--ram-mib",
            "2048",
            "--logcat",
            "/abs/log.txt",
            "--sync",
            "farm",
            "--headless",
        ]);
        assert_eq!(absolute(&given, cwd), expected);
    }

    #[test]
    fn names_and_options_that_would_go_wrong_are_refused() {
        let root = scratch("names_and_options_th");
        let refused = |name: &str, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            create(&root, name, &args, Path::new("/"))
                .unwrap_err()
                .to_string()
        };
        assert_eq!(refused("../x", &[]), copy::INSTANCE_BAD_NAME);
        assert_eq!(refused("", &[]), copy::INSTANCE_BAD_NAME);
        assert_eq!(refused("a", &["--ctl", "s"]), copy::INSTANCE_OWN_OPTIONS);
        assert_eq!(refused("a", &["x\ny"]), copy::INSTANCE_BAD_ARGUMENT);
        assert!(start_args(&root, "missing", &[]).is_err());
        assert!(list(&root).unwrap().is_empty());
    }

    #[test]
    fn removing_asks_first_and_only_yes_deletes() {
        let root = scratch("removing_asks_first_");
        let args = ["--kernel".to_owned(), "k".to_owned()];
        create(&root, "a", &args, Path::new("/")).unwrap();
        let mut asked = Vec::new();
        assert!(!remove(&root, "a", false, &mut asked, &mut &b"\n"[..]).unwrap());
        assert_eq!(
            String::from_utf8(asked).unwrap(),
            copy::instance_confirm_delete("a")
        );
        assert!(root.join("a").is_dir());
        assert!(remove(&root, "a", false, &mut Vec::new(), &mut &b"y\n"[..]).unwrap());
        assert!(!root.join("a").exists());
        assert!(remove(&root, "a", true, &mut Vec::new(), &mut &b""[..]).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
