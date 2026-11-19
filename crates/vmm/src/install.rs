//! Installing apps into the guest: one APK, a directory of split APKs, or an `.apks` / `.xapk`
//! archive, with the OBB files an archive carries.
//!
//! A single APK goes in with `cmd package install`; several go in through one package-manager
//! install session, so that a base APK and its splits arrive together. OBB files are copied to
//! the same place in the guest's shared storage that they have in the archive (`Android/obb/...`).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::adb::{self, Input};
use crate::config::{GUEST_SHARED_STORAGE, ZIP_MAX_INFLATED};
use crate::copy;

const APK_EXTENSION: &str = "apk";
/// Where OBB files sit inside an archive, and so in the guest's shared storage.
const OBB_PREFIX: &str = "Android/obb/";
const SUCCESS: &str = "Success";

const ZIP_END_SIGNATURE: u32 = 0x0605_4b50;
const ZIP_CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const ZIP_LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const ZIP_END_LEN: usize = 22;
const ZIP_CENTRAL_LEN: usize = 46;
const ZIP_LOCAL_LEN: usize = 30;
/// The end record can be followed by a comment of up to this many bytes.
const ZIP_MAX_COMMENT: usize = 0xffff;
const ZIP_STORED: u16 = 0;
const ZIP_DEFLATED: u16 = 8;
/// A size this large means the real one is in a ZIP64 record, which is not read here.
const ZIP64_MARK: u32 = u32::MAX;

/// One entry of an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    name: String,
    method: u16,
    compressed: u64,
    size: u64,
    /// Where its local header starts.
    offset: u64,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn bad_archive() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, copy::INSTALL_BAD_ARCHIVE)
}

/// Lists the entries of a zip archive from its central directory.
fn zip_entries<R: Read + Seek>(archive: &mut R) -> io::Result<Vec<Entry>> {
    let length = archive.seek(SeekFrom::End(0))?;
    let tail_len = length.min((ZIP_END_LEN + ZIP_MAX_COMMENT) as u64);
    archive.seek(SeekFrom::Start(length - tail_len))?;
    let mut tail = vec![0; tail_len as usize];
    archive.read_exact(&mut tail)?;
    if tail.len() < ZIP_END_LEN {
        return Err(bad_archive());
    }
    let end = (0..=tail.len().saturating_sub(ZIP_END_LEN))
        .rev()
        .find(|&at| u32_at(&tail, at) == ZIP_END_SIGNATURE)
        .ok_or_else(bad_archive)?;
    let count = u16_at(&tail, end + 10);
    let directory_len = u32_at(&tail, end + 12);
    let directory_at = u32_at(&tail, end + 16);
    if directory_at == ZIP64_MARK || u64::from(directory_at) + u64::from(directory_len) > length {
        return Err(bad_archive());
    }
    archive.seek(SeekFrom::Start(directory_at.into()))?;
    let mut directory = vec![0; directory_len as usize];
    archive.read_exact(&mut directory)?;
    let mut entries = Vec::new();
    let mut at = 0;
    for _ in 0..count {
        let header = directory
            .get(at..at + ZIP_CENTRAL_LEN)
            .ok_or_else(bad_archive)?;
        if u32_at(header, 0) != ZIP_CENTRAL_SIGNATURE {
            return Err(bad_archive());
        }
        let (compressed, size, offset) =
            (u32_at(header, 20), u32_at(header, 24), u32_at(header, 42));
        if [compressed, size, offset].contains(&ZIP64_MARK) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                copy::INSTALL_ZIP64,
            ));
        }
        let name_len = usize::from(u16_at(header, 28));
        let skip = name_len + usize::from(u16_at(header, 30)) + usize::from(u16_at(header, 32));
        let name = directory.get(at + ZIP_CENTRAL_LEN..at + ZIP_CENTRAL_LEN + name_len);
        let name = String::from_utf8_lossy(name.ok_or_else(bad_archive)?).into_owned();
        entries.push(Entry {
            name,
            method: u16_at(header, 10),
            compressed: compressed.into(),
            size: size.into(),
            offset: offset.into(),
        });
        at += ZIP_CENTRAL_LEN + skip;
    }
    Ok(entries)
}

/// Opens one entry of an archive for reading.
fn zip_open<'a, R: Read + Seek>(
    archive: &'a mut R,
    entry: &Entry,
) -> io::Result<Box<dyn Read + 'a>> {
    archive.seek(SeekFrom::Start(entry.offset))?;
    let mut local = [0; ZIP_LOCAL_LEN];
    archive.read_exact(&mut local)?;
    if u32_at(&local, 0) != ZIP_LOCAL_SIGNATURE {
        return Err(bad_archive());
    }
    let skip = i64::from(u16_at(&local, 26)) + i64::from(u16_at(&local, 28));
    archive.seek(SeekFrom::Current(skip))?;
    let mut data = archive.take(entry.compressed);
    match entry.method {
        ZIP_STORED if entry.compressed == entry.size => Ok(Box::new(data)),
        ZIP_DEFLATED => {
            // shortcut: a deflated entry is inflated in memory, up to ZIP_MAX_INFLATED; stream it
            // if archives with large deflated entries turn up (APKs and OBBs are usually stored).
            if entry.size > ZIP_MAX_INFLATED {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    copy::INSTALL_TOO_LARGE,
                ));
            }
            let mut packed = Vec::new();
            data.read_to_end(&mut packed)?;
            let inflated =
                miniz_oxide::inflate::decompress_to_vec_with_limit(&packed, entry.size as usize);
            let inflated = inflated.map_err(|_| bad_archive())?;
            if inflated.len() as u64 != entry.size {
                return Err(bad_archive());
            }
            Ok(Box::new(io::Cursor::new(inflated)))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            copy::INSTALL_BAD_ARCHIVE,
        )),
    }
}

/// Where the parts of an install come from.
#[derive(Debug, PartialEq, Eq)]
enum Parts {
    /// APK files, with their sizes.
    Files(Vec<(PathBuf, u64)>),
    /// An archive, with the entries that are APKs and the entries that are OBB files.
    Archive {
        path: PathBuf,
        apks: Vec<Entry>,
        obbs: Vec<Entry>,
    },
}

fn is_apk(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(APK_EXTENSION))
}

/// Works out what installing `path` means: which APKs, and which OBB files.
fn parts(path: &Path) -> io::Result<Parts> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        let mut apks = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            if entry.file_type()?.is_file() && is_apk(&name.to_string_lossy()) {
                apks.push((entry.path(), entry.metadata()?.len()));
            }
        }
        apks.sort();
        if apks.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                copy::INSTALL_NOTHING,
            ));
        }
        return Ok(Parts::Files(apks));
    }
    if is_apk(&path.to_string_lossy()) {
        return Ok(Parts::Files(vec![(path.to_owned(), metadata.len())]));
    }
    // Anything else has to be an archive of APKs: .apks, .xapk, .apkm or a plain .zip.
    let entries = zip_entries(&mut File::open(path)?)?;
    let (mut apks, mut obbs) = (Vec::new(), Vec::new());
    for entry in entries {
        if entry.name.ends_with('/') || entry.name.split('/').any(|part| part == "..") {
            continue;
        }
        if entry.name.starts_with(OBB_PREFIX) {
            obbs.push(entry);
        } else if is_apk(&entry.name) {
            apks.push(entry);
        }
    }
    if apks.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            copy::INSTALL_NOTHING,
        ));
    }
    Ok(Parts::Archive {
        path: path.to_owned(),
        apks,
        obbs,
    })
}

/// Fails with what the package manager said unless it said it succeeded.
fn succeeded(said: &str) -> io::Result<()> {
    if said.trim_start().starts_with(SUCCESS) {
        Ok(())
    } else {
        Err(io::Error::other(copy::install_refused(said.trim())))
    }
}

/// The session number in "Success: created install session [1234]".
fn session_number(said: &str) -> Option<u32> {
    let (_, rest) = said.split_once('[')?;
    rest.split_once(']')?.0.parse().ok()
}

fn exec_text(port: u16, command: &str, input: Option<Input<'_>>) -> io::Result<String> {
    let mut printed = Vec::new();
    adb::exec(port, command, input, &mut printed)?;
    Ok(String::from_utf8_lossy(&printed).into_owned())
}

/// One APK to install: its name, size, and where to read it.
struct Apk<'a> {
    name: String,
    size: u64,
    reader: Box<dyn Read + 'a>,
}

/// Installs `apks` through one install session, which the package manager commits as a whole.
fn install_session(
    port: u16,
    apks: &mut dyn Iterator<Item = io::Result<Apk<'_>>>,
    total: u64,
) -> io::Result<()> {
    let created = exec_text(
        port,
        &format!("cmd package install-create -S {total}"),
        None,
    )?;
    succeeded(&created)?;
    let session = session_number(&created)
        .ok_or_else(|| io::Error::other(copy::install_refused(created.trim())))?;
    let written = (|| {
        for apk in apks {
            let mut apk = apk?;
            let command = format!(
                "cmd package install-write -S {} {session} {} -",
                apk.size,
                adb::quote(&apk.name)
            );
            let input = Input {
                reader: &mut apk.reader,
                length: apk.size,
            };
            succeeded(&exec_text(port, &command, Some(input))?)?;
        }
        succeeded(&exec_text(
            port,
            &format!("cmd package install-commit {session}"),
            None,
        )?)
    })();
    if written.is_err() {
        // A session left open keeps its files in the guest until it expires.
        let _ = exec_text(
            port,
            &format!("cmd package install-abandon {session}"),
            None,
        );
    }
    written
}

/// Copies one OBB file into the guest's shared storage at the path it has in the archive.
fn push_obb(port: u16, name: &str, size: u64, reader: &mut dyn Read) -> io::Result<()> {
    let target = format!("{GUEST_SHARED_STORAGE}/{name}");
    let directory = target
        .rsplit_once('/')
        .map_or(GUEST_SHARED_STORAGE, |(dir, _)| dir);
    // `head -c` stops after the file's length, because an exec stream has no end-of-input signal.
    let command = format!(
        "mkdir -p {dir} && head -c {size} > {file} && echo {SUCCESS}",
        dir = adb::quote(directory),
        file = adb::quote(&target),
    );
    let said = exec_text(
        port,
        &format!("sh -c {}", adb::quote(&command)),
        Some(Input {
            reader,
            length: size,
        }),
    )?;
    succeeded(&said)
}

/// Installs the app at `path` into the guest whose `adb` is on loopback `port`.
pub fn install(port: u16, path: &Path) -> io::Result<()> {
    match parts(path)? {
        Parts::Files(files) if files.len() == 1 => {
            let (file, size) = &files[0];
            let mut reader = File::open(file)?;
            let input = Input {
                reader: &mut reader,
                length: *size,
            };
            succeeded(&exec_text(
                port,
                &format!("cmd package install -S {size}"),
                Some(input),
            )?)
        }
        Parts::Files(files) => {
            let total = files.iter().map(|(_, size)| size).sum();
            let mut apks = files.iter().map(|(file, size)| {
                Ok(Apk {
                    name: file
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    size: *size,
                    reader: Box::new(File::open(file)?),
                })
            });
            install_session(port, &mut apks, total)
        }
        Parts::Archive { path, apks, obbs } => {
            let total = apks.iter().map(|entry| entry.size).sum();
            let mut archive = File::open(&path)?;
            // Each APK is read whole before the next is opened, so one handle to the archive does.
            let mut readers = apks.iter().map(|entry| {
                let name = entry
                    .name
                    .rsplit('/')
                    .next()
                    .unwrap_or(&entry.name)
                    .to_owned();
                // Read whole, so held to the same size as a deflated entry.
                if entry.size > ZIP_MAX_INFLATED {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        copy::INSTALL_TOO_LARGE,
                    ));
                }
                let mut reader = zip_open(&mut archive, entry)?;
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes)?;
                Ok(Apk {
                    name,
                    size: entry.size,
                    reader: Box::new(io::Cursor::new(bytes)),
                })
            });
            install_session(port, &mut readers, total)?;
            let mut archive = File::open(&path)?;
            for entry in &obbs {
                let mut reader = zip_open(&mut archive, entry)?;
                push_obb(port, &entry.name, entry.size, &mut reader)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// A zip archive with these entries, each stored or deflated as asked.
    fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, data, deflate) in entries {
            let packed = if *deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.to_vec()
            };
            let method = if *deflate { ZIP_DEFLATED } else { ZIP_STORED };
            let offset = out.len() as u32;
            let header = |signature: u32, central: bool| {
                let mut bytes = signature.to_le_bytes().to_vec();
                if central {
                    bytes.extend([20, 0]);
                }
                bytes.extend([20, 0, 0, 0]);
                bytes.extend(method.to_le_bytes());
                bytes.extend([0; 8]);
                bytes.extend((packed.len() as u32).to_le_bytes());
                bytes.extend((data.len() as u32).to_le_bytes());
                bytes.extend((name.len() as u16).to_le_bytes());
                bytes.extend([0, 0]);
                if central {
                    bytes.extend([0; 10]);
                    bytes.extend(offset.to_le_bytes());
                }
                bytes.extend(name.as_bytes());
                bytes
            };
            let local = header(ZIP_LOCAL_SIGNATURE, false);
            let central = header(ZIP_CENTRAL_SIGNATURE, true);
            out.extend(local);
            out.extend(&packed);
            directory.extend(central);
        }
        let directory_at = out.len() as u32;
        out.extend(&directory);
        out.extend(ZIP_END_SIGNATURE.to_le_bytes());
        out.extend([0; 4]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((directory.len() as u32).to_le_bytes());
        out.extend(directory_at.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    #[test]
    fn archive_entries_read_back_stored_or_deflated() {
        let big = vec![7u8; 5000];
        let bytes = zip(&[
            ("base.apk", b"stored apk", false),
            ("split/config.arm64.apk", &big, true),
        ]);
        let mut archive = Cursor::new(bytes);
        let entries = zip_entries(&mut archive).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].name, "split/config.arm64.apk");
        let mut read = |entry: &Entry| {
            let mut out = Vec::new();
            zip_open(&mut archive, entry)
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
            out
        };
        assert_eq!(read(&entries[0]), b"stored apk");
        assert_eq!(read(&entries[1]), big);
        assert!(zip_entries(&mut Cursor::new(b"not a zip".to_vec())).is_err());
    }

    #[test]
    fn an_archive_installs_its_apks_and_copies_its_obb_files() {
        let dir = std::env::temp_dir().join(format!("emu-install-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let xapk = dir.join("game.xapk");
        let bytes = zip(&[
            ("manifest.json", b"{}", false),
            ("com.game.apk", b"base", false),
            ("config.en.apk", b"split", true),
            ("Android/obb/com.game/main.1.com.game.obb", b"data", false),
            ("../escape.apk", b"no", false),
        ]);
        std::fs::write(&xapk, bytes).unwrap();
        let Parts::Archive { apks, obbs, .. } = parts(&xapk).unwrap() else {
            panic!("not read as an archive");
        };
        let names = |entries: &[Entry]| entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&apks), ["com.game.apk", "config.en.apk"]);
        assert_eq!(names(&obbs), ["Android/obb/com.game/main.1.com.game.obb"]);

        // A directory of splits, and a plain APK.
        let splits = dir.join("splits");
        std::fs::create_dir_all(&splits).unwrap();
        std::fs::write(splits.join("b.apk"), b"bb").unwrap();
        std::fs::write(splits.join("a.APK"), b"a").unwrap();
        std::fs::write(splits.join("notes.txt"), b"").unwrap();
        let Parts::Files(files) = parts(&splits).unwrap() else {
            panic!("not read as files");
        };
        assert_eq!(
            files,
            [(splits.join("a.APK"), 1), (splits.join("b.apk"), 2)]
        );
        assert_eq!(
            parts(&splits.join("b.apk")).unwrap(),
            Parts::Files(vec![(splits.join("b.apk"), 2)])
        );
        assert!(parts(&splits.join("notes.txt")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn package_manager_answers_are_read_for_success_and_the_session_number() {
        assert!(succeeded("Success\n").is_ok());
        let refused = succeeded("Failure [INSTALL_FAILED_NO_MATCHING_ABIS]\n").unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("INSTALL_FAILED_NO_MATCHING_ABIS")
        );
        assert_eq!(
            session_number("Success: created install session [1234]\n"),
            Some(1234)
        );
        assert_eq!(session_number("Failure"), None);
    }
}
