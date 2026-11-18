//! A disk as the guest sees it: a run of bytes stitched together from files, memory and filler.
//!
//! One plain file is the simple case. The same type also presents several image files as the partitions
//! of one GPT disk, and maps an Android sparse image in place, without copying either.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::sync::Arc;

use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

pub mod gpt;
pub mod sparse;

/// Width of a fill pattern, as Android's sparse format defines it.
pub const FILL_LEN: usize = 4;
/// How much filler is written to the guest per step.
const FILL_CHUNK: usize = 4096;

/// Where the bytes of one piece of a disk come from.
#[derive(Debug)]
pub enum Source {
    /// A range of a host file, starting at `offset`. Many pieces may share one open file.
    File {
        file: Arc<File>,
        offset: u64,
        writable: bool,
    },
    /// Bytes held in memory. Writable, but what the guest writes is gone when the VMM exits.
    Memory(Vec<u8>),
    /// A four-byte pattern repeated over the whole piece; all zeros for a hole. Never writable.
    Fill([u8; FILL_LEN]),
}

#[derive(Debug)]
struct Segment {
    start: u64,
    len: u64,
    source: Source,
}

#[derive(Debug, Default)]
pub struct Disk {
    /// Sorted by `start`, contiguous from zero.
    segments: Vec<Segment>,
    len: u64,
}

fn refuse(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

enum Direction {
    DiskToGuest,
    GuestToDisk,
}

impl Disk {
    /// A disk backed by the first `len` bytes of one file.
    pub fn from_file(file: File, len: u64, writable: bool) -> Self {
        let mut disk = Self::default();
        disk.push(
            len,
            Source::File {
                file: Arc::new(file),
                offset: 0,
                writable,
            },
        );
        disk
    }

    /// Appends a piece. Zero-length pieces are dropped.
    pub fn push(&mut self, len: u64, source: Source) {
        if len > 0 {
            self.segments.push(Segment {
                start: self.len,
                len,
                source,
            });
            self.len += len;
        }
    }

    /// Appends every piece of another disk.
    pub fn append(&mut self, other: Disk) {
        for segment in other.segments {
            self.push(segment.len, segment.source);
        }
    }

    /// Appends zero filler up to the next multiple of `align`.
    pub fn pad_to(&mut self, align: u64) {
        self.push(
            self.len.next_multiple_of(align) - self.len,
            Source::Fill([0; FILL_LEN]),
        );
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn transfer(
        &mut self,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        offset: u64,
        len: u64,
        direction: &Direction,
    ) -> io::Result<()> {
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= self.len)
            .ok_or_else(|| refuse(crate::copy::DISK_OUT_OF_RANGE))?;
        let mut index = self
            .segments
            .partition_point(|segment| segment.start + segment.len <= offset);
        let mut at = offset;
        while at < end {
            let segment = &mut self.segments[index];
            let within = at - segment.start;
            let step = (segment.len - within).min(end - at);
            let guest = addr
                .checked_add(at - offset)
                .ok_or_else(|| refuse(crate::copy::DISK_BAD_GUEST_ADDRESS))?;
            let count =
                usize::try_from(step).map_err(|_| refuse(crate::copy::DISK_OUT_OF_RANGE))?;
            match (&mut segment.source, direction) {
                (Source::File { file, offset, .. }, Direction::DiskToGuest) => {
                    let mut file: &File = file;
                    file.seek(SeekFrom::Start(*offset + within))?;
                    mem.read_exact_volatile_from(guest, &mut file, count)
                        .map_err(io::Error::other)?;
                }
                (
                    Source::File {
                        file,
                        offset,
                        writable: true,
                    },
                    Direction::GuestToDisk,
                ) => {
                    let mut file: &File = file;
                    file.seek(SeekFrom::Start(*offset + within))?;
                    mem.write_all_volatile_to(guest, &mut file, count)
                        .map_err(io::Error::other)?;
                }
                (Source::Memory(bytes), Direction::DiskToGuest) => {
                    let from = within as usize;
                    mem.write_slice(&bytes[from..from + count], guest)
                        .map_err(io::Error::other)?;
                }
                (Source::Memory(bytes), Direction::GuestToDisk) => {
                    let from = within as usize;
                    mem.read_slice(&mut bytes[from..from + count], guest)
                        .map_err(io::Error::other)?;
                }
                (Source::Fill(pattern), Direction::DiskToGuest) => {
                    // The pattern repeats from the start of the piece, so a read in the middle starts mid-pattern.
                    let phase = (within % FILL_LEN as u64) as usize;
                    let chunk: Vec<u8> = pattern
                        .iter()
                        .cycle()
                        .skip(phase)
                        .take(FILL_CHUNK.min(count))
                        .copied()
                        .collect();
                    let mut done = 0;
                    while done < count {
                        let take = chunk.len().min(count - done);
                        // A chunk is a whole number of patterns unless it is the only one, so each starts in phase.
                        let target = guest
                            .checked_add(done as u64)
                            .ok_or_else(|| refuse(crate::copy::DISK_BAD_GUEST_ADDRESS))?;
                        mem.write_slice(&chunk[..take], target)
                            .map_err(io::Error::other)?;
                        done += take;
                    }
                }
                (
                    Source::File {
                        writable: false, ..
                    }
                    | Source::Fill(_),
                    Direction::GuestToDisk,
                ) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        crate::copy::DISK_READ_ONLY_AREA,
                    ));
                }
            }
            at += step;
            index += 1;
        }
        Ok(())
    }

    /// Copies `len` bytes at disk `offset` into guest memory at `addr`.
    pub fn read_to_guest(
        &mut self,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        offset: u64,
        len: u64,
    ) -> io::Result<()> {
        self.transfer(mem, addr, offset, len, &Direction::DiskToGuest)
    }

    /// Copies `len` bytes of guest memory at `addr` to disk `offset`.
    pub fn write_from_guest(
        &mut self,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        offset: u64,
        len: u64,
    ) -> io::Result<()> {
        self.transfer(mem, addr, offset, len, &Direction::GuestToDisk)
    }

    /// Asks the host to make every writable file's data durable.
    pub fn flush(&mut self) -> io::Result<()> {
        for segment in &self.segments {
            if let Source::File {
                file,
                writable: true,
                ..
            } = &segment.source
            {
                file.sync_data()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A scratch file with the given contents, deleted on drop.
    pub struct TempFile(pub PathBuf);

    impl TempFile {
        pub fn new(contents: &[u8]) -> Self {
            let name = format!(
                "emu-disk-{}-{}.img",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            );
            let path = std::env::temp_dir().join(name);
            File::create(&path).unwrap().write_all(contents).unwrap();
            Self(path)
        }

        pub fn open(&self, writable: bool) -> File {
            File::options()
                .read(true)
                .write(writable)
                .open(&self.0)
                .unwrap()
        }

        pub fn contents(&self) -> Vec<u8> {
            std::fs::read(&self.0).unwrap()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempFile;
    use super::*;

    const MEM_SIZE: usize = 1 << 20;
    const BUFFER: GuestAddress = GuestAddress(0x1000);

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap()
    }

    fn read(
        disk: &mut Disk,
        mem: &GuestMemoryMmap,
        offset: u64,
        len: usize,
    ) -> io::Result<Vec<u8>> {
        disk.read_to_guest(mem, BUFFER, offset, len as u64)?;
        let mut bytes = vec![0; len];
        mem.read_slice(&mut bytes, BUFFER).unwrap();
        Ok(bytes)
    }

    fn write(disk: &mut Disk, mem: &GuestMemoryMmap, offset: u64, bytes: &[u8]) -> io::Result<()> {
        mem.write_slice(bytes, BUFFER).unwrap();
        disk.write_from_guest(mem, BUFFER, offset, bytes.len() as u64)
    }

    /// file A (bytes 100..110 of it), memory, a patterned fill, then read-only file B.
    fn patchwork(a: &TempFile, b: &TempFile) -> Disk {
        let mut disk = Disk::default();
        disk.push(
            10,
            Source::File {
                file: Arc::new(a.open(true)),
                offset: 100,
                writable: true,
            },
        );
        disk.push(4, Source::Memory(vec![0xa1, 0xa2, 0xa3, 0xa4]));
        disk.push(0, Source::Memory(Vec::new()));
        disk.push(6, Source::Fill([1, 2, 3, 4]));
        disk.push(
            8,
            Source::File {
                file: Arc::new(b.open(false)),
                offset: 0,
                writable: false,
            },
        );
        disk
    }

    #[test]
    fn a_read_across_every_kind_of_piece_returns_the_stitched_bytes() {
        let a = TempFile::new(&(0..=255u8).collect::<Vec<_>>());
        let b = TempFile::new(b"readonly");
        let mut disk = patchwork(&a, &b);
        let mem = memory();
        assert_eq!(disk.len(), 28);
        let all = read(&mut disk, &mem, 0, 28).unwrap();
        assert_eq!(&all[..10], (100..110u8).collect::<Vec<_>>());
        assert_eq!(&all[10..14], [0xa1, 0xa2, 0xa3, 0xa4]);
        assert_eq!(&all[14..20], [1, 2, 3, 4, 1, 2]);
        assert_eq!(&all[20..], b"readonly");
        // Starting in the middle of a piece, and in the middle of the fill pattern.
        assert_eq!(read(&mut disk, &mem, 8, 4).unwrap(), [108, 109, 0xa1, 0xa2]);
        assert_eq!(read(&mut disk, &mem, 15, 4).unwrap(), [2, 3, 4, 1]);
    }

    #[test]
    fn long_fill_keeps_its_phase_across_chunks() {
        let mut disk = Disk::default();
        disk.push(3 * FILL_CHUNK as u64 + 2, Source::Fill([9, 8, 7, 6]));
        let mem = memory();
        let bytes = read(&mut disk, &mem, 1, 2 * FILL_CHUNK + 5).unwrap();
        let expected: Vec<u8> = [9u8, 8, 7, 6]
            .iter()
            .cycle()
            .skip(1)
            .take(bytes.len())
            .copied()
            .collect();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn writes_land_in_writable_pieces_and_are_refused_elsewhere() {
        let a = TempFile::new(&[0u8; 256]);
        let b = TempFile::new(b"readonly");
        let mut disk = patchwork(&a, &b);
        let mem = memory();
        // Spanning the writable file and the memory piece.
        write(&mut disk, &mem, 8, &[0x11, 0x22, 0x33, 0x44]).unwrap();
        disk.flush().unwrap();
        assert_eq!(&a.contents()[108..110], [0x11, 0x22]);
        assert_eq!(
            read(&mut disk, &mem, 10, 4).unwrap(),
            [0x33, 0x44, 0xa3, 0xa4]
        );
        // Filler and read-only files reject writes and keep their contents.
        assert!(write(&mut disk, &mem, 14, &[0xff]).is_err());
        assert!(write(&mut disk, &mem, 20, &[0xff]).is_err());
        assert_eq!(b.contents(), b"readonly");
    }

    #[test]
    fn accesses_outside_the_disk_or_guest_memory_are_refused() {
        let a = TempFile::new(&[0u8; 256]);
        let b = TempFile::new(b"readonly");
        let mut disk = patchwork(&a, &b);
        let mem = memory();
        assert!(read(&mut disk, &mem, 28, 1).is_err());
        assert!(read(&mut disk, &mem, 20, 9).is_err());
        assert!(disk.read_to_guest(&mem, BUFFER, u64::MAX, 2).is_err());
        assert!(
            disk.read_to_guest(&mem, GuestAddress(MEM_SIZE as u64), 0, 4)
                .is_err()
        );
        assert!(
            read(&mut disk, &mem, 28, 0).is_ok(),
            "an empty read at the very end is fine"
        );
    }

    #[test]
    fn padding_rounds_the_length_up() {
        let mut disk = Disk::default();
        disk.push(5, Source::Memory(vec![1; 5]));
        disk.pad_to(8);
        assert_eq!(disk.len(), 8);
        disk.pad_to(8);
        assert_eq!(disk.len(), 8);
        assert!(!disk.is_empty() && Disk::default().is_empty());
    }
}
