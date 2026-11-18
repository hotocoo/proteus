//! Writes a "newc" cpio archive, the format of a Linux initramfs.
//!
//! The kernel unpacks every archive in the ramdisk in order, and a later file replaces an earlier one
//! with the same path. That lets the VMM adjust a vendor ramdisk without unpacking it: it appends a
//! small archive of its own.

const MAGIC: &str = "070701";
const TRAILER: &str = "TRAILER!!!";
/// Header, name and file data are each padded to this many bytes.
const ALIGN: usize = 4;
const MODE_DIR: u32 = 0o040_755;
const MODE_FILE: u32 = 0o100_644;
/// Fixed part of an entry: the magic and thirteen eight-digit hexadecimal fields.
const HEADER_LEN: usize = 110;

#[derive(Default)]
pub struct Archive {
    bytes: Vec<u8>,
    next_inode: u32,
}

impl Archive {
    fn pad(&mut self) {
        self.bytes
            .resize(self.bytes.len().next_multiple_of(ALIGN), 0);
    }

    fn entry(&mut self, path: &str, mode: u32, links: u32, data: &[u8]) {
        self.next_inode += 1;
        // The name is stored with a terminating NUL, which its length field counts.
        let name_len = path.len() + 1;
        let fields = [
            self.next_inode,
            mode,
            0, // uid
            0, // gid
            links,
            0, // mtime
            data.len() as u32,
            0, // device major
            0, // device minor
            0, // rdev major
            0, // rdev minor
            name_len as u32,
            0, // checksum, unused by this format variant
        ];
        let start = self.bytes.len();
        self.bytes.extend_from_slice(MAGIC.as_bytes());
        for field in fields {
            self.bytes
                .extend_from_slice(format!("{field:08x}").as_bytes());
        }
        debug_assert_eq!(self.bytes.len() - start, HEADER_LEN);
        self.bytes.extend_from_slice(path.as_bytes());
        self.bytes.push(0);
        self.pad();
        self.bytes.extend_from_slice(data);
        self.pad();
    }

    /// Adds a directory. Parents must be added first, as the kernel creates entries in archive order.
    pub fn dir(&mut self, path: &str) -> &mut Self {
        self.entry(path, MODE_DIR, 2, &[]);
        self
    }

    /// Adds a regular file readable by everyone and writable by root.
    pub fn file(&mut self, path: &str, data: &[u8]) -> &mut Self {
        self.entry(path, MODE_FILE, 1, data);
        self
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.entry(TRAILER, 0, 1, &[]);
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    fn sample() -> Vec<u8> {
        let mut archive = Archive::default();
        archive
            .dir("first_stage_ramdisk")
            .file("first_stage_ramdisk/fstab.emu", b"line one\nline two\n")
            .file("empty", b"");
        archive.finish()
    }

    #[test]
    fn layout_matches_the_newc_format() {
        let bytes = sample();
        assert_eq!(bytes.len() % ALIGN, 0);
        assert_eq!(&bytes[..6], b"070701");
        // First entry: a directory with two links and no data.
        assert_eq!(&bytes[14..22], b"000041ed");
        assert_eq!(&bytes[38..46], b"00000002");
        assert_eq!(&bytes[54..62], b"00000000");
        // Its name follows the 110-byte header, NUL-terminated, counted in the name-length field.
        assert_eq!(
            &bytes[94..102],
            format!("{:08x}", "first_stage_ramdisk".len() + 1).as_bytes()
        );
        assert_eq!(
            &bytes[HEADER_LEN..HEADER_LEN + 20],
            b"first_stage_ramdisk\0"
        );
        // The file's data starts on a four-byte boundary after its name and is stored verbatim.
        let data = b"line one\nline two\n";
        let at = bytes
            .windows(data.len())
            .position(|window| window == data)
            .unwrap();
        assert_eq!(at % ALIGN, 0);
        let tail = String::from_utf8_lossy(&bytes[bytes.len() - 16..]).into_owned();
        assert!(tail.contains(TRAILER));
    }

    /// The system's own cpio must accept the archive and see the same entries.
    #[test]
    fn archive_is_readable_by_the_system_cpio() {
        let run = |args: &[&str]| {
            let mut child = Command::new("cpio")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .ok()?;
            std::io::Write::write_all(child.stdin.as_mut()?, &sample()).ok()?;
            child.wait_with_output().ok()
        };
        let Some(listing) = run(&["-it"]) else {
            eprintln!("cpio is not installed; archive compatibility test skipped");
            return;
        };
        assert!(
            listing.status.success(),
            "{}",
            String::from_utf8_lossy(&listing.stderr)
        );
        let names = String::from_utf8_lossy(&listing.stdout);
        assert_eq!(
            names.lines().collect::<Vec<_>>(),
            [
                "first_stage_ramdisk",
                "first_stage_ramdisk/fstab.emu",
                "empty"
            ]
        );
    }
}
