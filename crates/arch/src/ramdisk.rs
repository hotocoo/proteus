//! The ramdisk in guest memory: archives back to back, then the bootconfig trailer. Both boards use
//! it; where it goes is theirs to say.

use std::io::{Seek, SeekFrom};
use std::ops::Range;

use vm_memory::{Bytes, GuestAddress, GuestMemory, ReadVolatile};

use crate::{ArchError, bootconfig};

/// One archive of the ramdisk. The kernel unpacks them in order, and a later file replaces an earlier
/// one with the same path.
pub enum RamdiskPart<'a, F> {
    /// A byte range of a file: a ramdisk inside a boot image, or a whole ramdisk file.
    Stream {
        stream: &'a mut F,
        range: Range<u64>,
    },
    Bytes(&'a [u8]),
}

impl<F> RamdiskPart<'_, F> {
    fn len(&self) -> u64 {
        match self {
            Self::Stream { range, .. } => range.end.saturating_sub(range.start),
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }
}

/// Bytes the parts and the bootconfig trailer take together.
pub fn total_len<F>(
    parts: &[RamdiskPart<'_, F>],
    bootconfig: &[(String, String)],
) -> Result<u64, ArchError> {
    let len: u64 = parts.iter().map(RamdiskPart::len).sum();
    let trailer = if bootconfig.is_empty() {
        0
    } else {
        let host_len =
            usize::try_from(len).map_err(|_| ArchError::Layout(crate::copy::RAM_TOO_SMALL))?;
        bootconfig::trailer(bootconfig, host_len)?.len() as u64
    };
    Ok(len + trailer)
}

/// Copies the parts in at `start`, back to back with nothing between them, and appends the bootconfig
/// trailer when there are parameters. Fails with `too_big` if it all takes more than `room` bytes.
///
/// The parts must be archives the kernel can tell apart on its own. In particular, whatever follows an
/// LZ4 legacy archive must be another one: that format has no end marker, so its decoder reads on.
pub fn write<M, F>(
    mem: &M,
    start: u64,
    room: u64,
    parts: &mut [RamdiskPart<'_, F>],
    bootconfig: &[(String, String)],
    too_big: &'static str,
) -> Result<Range<u64>, ArchError>
where
    M: GuestMemory,
    F: ReadVolatile + Seek,
{
    let too_big = || ArchError::Layout(too_big);
    let mut len = 0u64;
    for part in parts {
        let part_len = part.len();
        if len.checked_add(part_len).is_none_or(|end| end > room) {
            return Err(too_big());
        }
        match part {
            RamdiskPart::Stream { stream, range } => {
                stream.seek(SeekFrom::Start(range.start))?;
                let count = usize::try_from(part_len).map_err(|_| too_big())?;
                mem.read_exact_volatile_from(GuestAddress(start + len), *stream, count)?;
            }
            RamdiskPart::Bytes(bytes) => mem.write_slice(bytes, GuestAddress(start + len))?,
        }
        len += part_len;
    }
    let host_len = usize::try_from(len).map_err(|_| too_big())?;
    let trailer = if bootconfig.is_empty() {
        Vec::new()
    } else {
        bootconfig::trailer(bootconfig, host_len)?
    };
    let total = len + trailer.len() as u64;
    if total > room {
        return Err(too_big());
    }
    mem.write_slice(&trailer, GuestAddress(start + len))?;
    Ok(start..start + total)
}
