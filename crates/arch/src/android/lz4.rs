//! Writes an LZ4 "legacy" frame that stores its data without compressing it.
//!
//! Android ramdisks are LZ4 legacy frames. The kernel's decompressor for that format has no way to know
//! where a frame ends, so it keeps reading: whatever follows must be another LZ4 legacy frame. The
//! archive the VMM appends therefore has to be one too. It is tiny, so it is stored as literals.

/// Magic number that opens a legacy frame.
const LEGACY_MAGIC: u32 = 0x184c_2102;
/// Most data one block may hold. The format allows 8 MiB; staying below keeps the size field simple.
const MAX_BLOCK_DATA: usize = 4 << 20;
/// A token's upper four bits hold the literal length, with this value meaning "more length bytes follow".
const TOKEN_LENGTH_ESCAPE: usize = 15;
const TOKEN_LITERAL_SHIFT: u32 = 4;
/// Each extra length byte adds up to this much; a smaller byte ends the length.
const LENGTH_BYTE_MAX: usize = 255;

/// One block holding `data` as a single run of literals, the form every LZ4 block may end with.
fn literal_block(data: &[u8]) -> Vec<u8> {
    let mut block = Vec::with_capacity(data.len() + data.len() / LENGTH_BYTE_MAX + 2);
    block.push((data.len().min(TOKEN_LENGTH_ESCAPE) << TOKEN_LITERAL_SHIFT) as u8);
    if let Some(mut rest) = data.len().checked_sub(TOKEN_LENGTH_ESCAPE) {
        while rest >= LENGTH_BYTE_MAX {
            block.push(LENGTH_BYTE_MAX as u8);
            rest -= LENGTH_BYTE_MAX;
        }
        block.push(rest as u8);
    }
    block.extend_from_slice(data);
    block
}

/// `data` as an LZ4 legacy frame of uncompressed blocks.
pub fn stored(data: &[u8]) -> Vec<u8> {
    let mut frame = LEGACY_MAGIC.to_le_bytes().to_vec();
    for chunk in data.chunks(MAX_BLOCK_DATA) {
        let block = literal_block(chunk);
        frame.extend_from_slice(&(block.len() as u32).to_le_bytes());
        frame.extend_from_slice(&block);
    }
    frame
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn frame_starts_with_the_magic_android_ramdisks_have() {
        assert_eq!(&stored(b"x")[..4], [0x02, 0x21, 0x4c, 0x18]);
    }

    #[test]
    fn literal_lengths_are_encoded_at_every_boundary() {
        assert_eq!(literal_block(b"abc"), [0x30, b'a', b'b', b'c']);
        // Exactly the escape value needs a zero length byte after the token.
        let fifteen = literal_block(&[7; 15]);
        assert_eq!(&fifteen[..2], [0xf0, 0]);
        assert_eq!(fifteen.len(), 2 + 15);
        // 15 + 255 needs one full length byte and a terminating zero.
        let long = literal_block(&[7; 270]);
        assert_eq!(&long[..3], [0xf0, 255, 0]);
        assert_eq!(literal_block(&[7; 271])[..3], [0xf0, 255, 1]);
    }

    /// The reference `lz4` tool must decode what we wrote back to the original bytes.
    #[test]
    fn reference_decoder_returns_the_original_bytes() {
        let data: Vec<u8> = (0..(MAX_BLOCK_DATA + 70_000))
            .map(|i| (i % 251) as u8)
            .collect();
        for sample in [&b"small"[..], &data[..300], &data] {
            let child = Command::new("lz4")
                .args(["-d", "-c"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                eprintln!("lz4 is not installed; decoder compatibility test skipped");
                return;
            };
            let frame = stored(sample);
            let mut stdin = child.stdin.take().unwrap();
            let writer = std::thread::spawn(move || stdin.write_all(&frame));
            let output = child.wait_with_output().unwrap();
            writer.join().unwrap().unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, sample);
        }
    }
}
