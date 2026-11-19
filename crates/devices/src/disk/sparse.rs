//! Android sparse images (`system/core/libsparse/sparse_format.h`), mapped in place.
//!
//! A sparse image stores only the blocks that hold data; the rest is described as a repeated pattern
//! or as "don't care". Instead of expanding the file on the host, each chunk becomes one piece of a
//! `Disk`: data chunks point into the image file, the others are filler. The result is read-only.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use super::{Disk, FILL_LEN, Source};
use crate::copy;

const MAGIC: u32 = 0xed26_ff3a;
const MAJOR_VERSION: u16 = 1;
const FILE_HEADER_LEN: usize = 28;
const CHUNK_HEADER_LEN: usize = 12;

const CHUNK_RAW: u16 = 0xcac1;
const CHUNK_FILL: u16 = 0xcac2;
const CHUNK_DONT_CARE: u16 = 0xcac3;
const CHUNK_CRC32: u16 = 0xcac4;
/// Payload of a checksum chunk, which describes no blocks.
const CRC_LEN: u64 = 4;

fn bad(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Whether the file starts with the sparse image magic. Leaves the file position at the start.
pub fn is_sparse(file: &mut File) -> io::Result<bool> {
    let mut magic = [0u8; size_of::<u32>()];
    file.rewind()?;
    let found = match file.read_exact(&mut magic) {
        Ok(()) => u32::from_le_bytes(magic) == MAGIC,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => false,
        Err(err) => return Err(err),
    };
    file.rewind()?;
    Ok(found)
}

/// Appends the expanded contents of a sparse image to `disk`, read-only. Returns the expanded length.
pub fn append(disk: &mut Disk, mut file: File) -> io::Result<u64> {
    let file_len = file.seek(SeekFrom::End(0))?;
    file.rewind()?;
    let mut header = [0u8; FILE_HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|_| bad(copy::SPARSE_BAD_HEADER))?;
    let file_header_len = u64::from(u16_at(&header, 8));
    let chunk_header_len = u64::from(u16_at(&header, 10));
    let block_len = u64::from(u32_at(&header, 12));
    let (total_blocks, total_chunks) = (u64::from(u32_at(&header, 16)), u32_at(&header, 20));
    let understood = u32_at(&header, 0) == MAGIC
        && u16_at(&header, 4) == MAJOR_VERSION
        && file_header_len >= FILE_HEADER_LEN as u64
        && chunk_header_len >= CHUNK_HEADER_LEN as u64
        && block_len != 0
        && block_len.is_multiple_of(FILL_LEN as u64);
    if !understood {
        return Err(bad(copy::SPARSE_BAD_HEADER));
    }

    let before = disk.len();
    // Not pre-sized: the count comes from the file's header and may claim billions of chunks.
    let mut chunk_headers = Vec::new();
    let mut at = file_header_len;
    let mut blocks = 0u64;
    for _ in 0..total_chunks {
        let mut chunk = [0u8; CHUNK_HEADER_LEN];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut chunk)
            .map_err(|_| bad(copy::SPARSE_TRUNCATED))?;
        let kind = u16_at(&chunk, 0);
        let out_len = u64::from(u32_at(&chunk, 4)) * block_len;
        let total_len = u64::from(u32_at(&chunk, 8));
        let payload_at = at + chunk_header_len;
        let payload_len = total_len
            .checked_sub(chunk_header_len)
            .ok_or_else(|| bad(copy::SPARSE_BAD_CHUNK))?;
        if payload_at + payload_len > file_len {
            return Err(bad(copy::SPARSE_TRUNCATED));
        }
        match kind {
            CHUNK_RAW if payload_len == out_len => {
                chunk_headers.push((out_len, Some(payload_at), [0; FILL_LEN]))
            }
            CHUNK_FILL if payload_len == FILL_LEN as u64 => {
                let mut pattern = [0u8; FILL_LEN];
                file.seek(SeekFrom::Start(payload_at))?;
                file.read_exact(&mut pattern)?;
                chunk_headers.push((out_len, None, pattern));
            }
            CHUNK_DONT_CARE if payload_len == 0 => {
                chunk_headers.push((out_len, None, [0; FILL_LEN]))
            }
            CHUNK_CRC32 if payload_len == CRC_LEN => {}
            _ => return Err(bad(copy::SPARSE_BAD_CHUNK)),
        }
        if kind != CHUNK_CRC32 {
            blocks += u64::from(u32_at(&chunk, 4));
        }
        at = payload_at + payload_len;
    }
    if blocks != total_blocks {
        return Err(bad(copy::SPARSE_SIZE_MISMATCH));
    }

    let file = Arc::new(file);
    for (len, data_at, pattern) in chunk_headers {
        let source = match data_at {
            Some(offset) => Source::File {
                file: Arc::clone(&file),
                offset,
                writable: false,
            },
            None => Source::Fill(pattern),
        };
        disk.push(len, source);
    }
    Ok(disk.len() - before)
}

#[cfg(test)]
pub(crate) mod tests {
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use super::super::testing::TempFile;
    use super::*;

    const BLOCK: u32 = 4096;

    pub(crate) enum Chunk<'a> {
        Raw(&'a [u8]),
        Fill([u8; 4], u32),
        Skip(u32),
        Crc,
    }

    /// Builds a sparse image with the given chunks and a header that matches them.
    pub(crate) fn image(chunks: &[Chunk<'_>]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut blocks = 0u32;
        for chunk in chunks {
            let (kind, count, payload): (u16, u32, Vec<u8>) = match chunk {
                Chunk::Raw(data) => (CHUNK_RAW, data.len() as u32 / BLOCK, data.to_vec()),
                Chunk::Fill(pattern, count) => (CHUNK_FILL, *count, pattern.to_vec()),
                Chunk::Skip(count) => (CHUNK_DONT_CARE, *count, Vec::new()),
                Chunk::Crc => (CHUNK_CRC32, 0, vec![0; 4]),
            };
            blocks += count;
            body.extend(kind.to_le_bytes());
            body.extend([0, 0]);
            body.extend(count.to_le_bytes());
            body.extend((CHUNK_HEADER_LEN as u32 + payload.len() as u32).to_le_bytes());
            body.extend(payload);
        }
        let mut out = Vec::new();
        out.extend(MAGIC.to_le_bytes());
        out.extend(MAJOR_VERSION.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend((FILE_HEADER_LEN as u16).to_le_bytes());
        out.extend((CHUNK_HEADER_LEN as u16).to_le_bytes());
        out.extend(BLOCK.to_le_bytes());
        out.extend(blocks.to_le_bytes());
        out.extend((chunks.len() as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(body);
        out
    }

    fn expand(bytes: &[u8]) -> io::Result<Vec<u8>> {
        let file = TempFile::new(bytes);
        let mut disk = Disk::default();
        let len = append(&mut disk, file.open(false))? as usize;
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), len.max(1))]).unwrap();
        disk.read_to_guest(&mem, GuestAddress(0), 0, len as u64)?;
        let mut out = vec![0; len];
        mem.read_slice(&mut out, GuestAddress(0)).unwrap();
        Ok(out)
    }

    #[test]
    fn every_chunk_kind_expands_to_the_right_bytes() {
        let data: Vec<u8> = (0..2 * BLOCK).map(|i| (i % 251) as u8).collect();
        let sparse = image(&[
            Chunk::Raw(&data),
            Chunk::Fill([0xde, 0xad, 0xbe, 0xef], 1),
            Chunk::Crc,
            Chunk::Skip(3),
            Chunk::Raw(&data[..BLOCK as usize]),
        ]);
        let expanded = expand(&sparse).unwrap();
        let block = BLOCK as usize;
        assert_eq!(expanded.len(), 7 * block);
        assert_eq!(&expanded[..2 * block], data);
        assert!(
            expanded[2 * block..3 * block]
                .chunks(4)
                .all(|word| word == [0xde, 0xad, 0xbe, 0xef])
        );
        assert!(expanded[3 * block..6 * block].iter().all(|byte| *byte == 0));
        assert_eq!(&expanded[6 * block..], &data[..block]);
    }

    #[test]
    fn sparse_images_are_recognised_by_their_magic() {
        let sparse = TempFile::new(&image(&[Chunk::Skip(1)]));
        let raw = TempFile::new(b"plain data");
        let empty = TempFile::new(b"");
        assert!(is_sparse(&mut sparse.open(false)).unwrap());
        assert!(!is_sparse(&mut raw.open(false)).unwrap());
        assert!(!is_sparse(&mut empty.open(false)).unwrap());
    }

    #[test]
    fn mapped_image_is_read_only() {
        let data = vec![7u8; BLOCK as usize];
        let file = TempFile::new(&image(&[Chunk::Raw(&data)]));
        let mut disk = Disk::default();
        append(&mut disk, file.open(false)).unwrap();
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), BLOCK as usize)]).unwrap();
        assert!(
            disk.write_from_guest(&mem, GuestAddress(0), 0, 512)
                .is_err()
        );
    }

    #[test]
    fn a_header_claiming_billions_of_chunks_is_refused_without_allocating_for_them() {
        let mut lying = image(&[Chunk::Skip(1)]);
        lying[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(expand(&lying).is_err());
    }

    #[test]
    fn damaged_images_are_refused() {
        let data = vec![1u8; BLOCK as usize];
        let good = image(&[Chunk::Raw(&data), Chunk::Fill([1, 2, 3, 4], 2)]);
        assert!(expand(&good).is_ok());
        // Every truncation.
        for cut in [
            0,
            10,
            FILE_HEADER_LEN - 1,
            FILE_HEADER_LEN + 5,
            good.len() - 1,
            good.len() - 20,
        ] {
            assert!(expand(&good[..cut]).is_err(), "cut at {cut}");
        }
        let corrupt = |at: usize, value: u8| {
            let mut bad = good.clone();
            bad[at] = value;
            expand(&bad)
        };
        assert!(corrupt(0, 0).is_err(), "magic");
        assert!(corrupt(4, 2).is_err(), "major version");
        assert!(
            corrupt(12, 1).is_err(),
            "block size not a multiple of the pattern size"
        );
        assert!(
            corrupt(16, 9).is_err(),
            "declared size disagrees with the chunks"
        );
        assert!(corrupt(FILE_HEADER_LEN, 0).is_err(), "unknown chunk type");
        assert!(
            corrupt(FILE_HEADER_LEN + 4, 2).is_err(),
            "raw chunk shorter than its block count"
        );
        assert!(
            corrupt(FILE_HEADER_LEN + 8, 3).is_err(),
            "chunk smaller than its own header"
        );
    }
}
