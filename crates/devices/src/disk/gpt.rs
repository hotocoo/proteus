//! Assembles a GPT disk in memory around partition contents that live elsewhere.
//!
//! Android finds its partitions by name (`/dev/block/by-name/...`), so several image files have to
//! appear to the guest as named partitions of one disk. Only the partition table is generated; the
//! contents stay where they are.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;

use super::{Disk, Source};
use crate::copy;

pub const SECTOR: u64 = 512;
/// Partitions start on this boundary, and the space before the first one holds the primary table.
const ALIGN: u64 = 1 << 20;
const ENTRY_COUNT: usize = 128;
const ENTRY_LEN: usize = 128;
const ENTRIES_LEN: usize = ENTRY_COUNT * ENTRY_LEN;
const ENTRIES_SECTORS: u64 = ENTRIES_LEN as u64 / SECTOR;
const HEADER_LEN: usize = 92;
const SIGNATURE: &[u8; 8] = b"EFI PART";
const REVISION_1_0: u32 = 0x0001_0000;
/// Partition names are at most this many UTF-16 code units.
const MAX_NAME_UNITS: usize = 36;
const GUID_LEN: usize = 16;
/// "Linux filesystem data", in the mixed-endian byte order GPT stores GUIDs in.
const TYPE_LINUX_DATA: [u8; GUID_LEN] = [
    0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4,
];

// Protective MBR layout.
const MBR_PARTITION_AT: usize = 446;
const MBR_TYPE_GPT_PROTECTIVE: u8 = 0xee;
const MBR_SIGNATURE: [u8; 2] = [0x55, 0xaa];

// Header LBAs.
const PRIMARY_HEADER_LBA: u64 = 1;
const PRIMARY_ENTRIES_LBA: u64 = 2;

pub struct Partition {
    pub name: String,
    /// What the partition holds; its length is the partition's size.
    pub contents: Disk,
}

fn refuse(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

/// CRC-32 as GPT, PNG and zlib use it (IEEE 802.3, reflected).
pub fn crc32(bytes: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0xedb8_8320;
    !bytes.iter().fold(u32::MAX, |crc, byte| {
        (0..u8::BITS).fold(crc ^ u32::from(*byte), |crc, _| {
            if crc & 1 == 1 {
                crc >> 1 ^ POLYNOMIAL
            } else {
                crc >> 1
            }
        })
    })
}

/// A GUID that depends only on `seed`, so a disk assembled twice from the same parts is identical.
fn guid(seed: &str) -> [u8; GUID_LEN] {
    let half = |salt: u8| {
        let mut hasher = DefaultHasher::new();
        (salt, seed).hash(&mut hasher);
        hasher.finish().to_le_bytes()
    };
    let mut guid = [0u8; GUID_LEN];
    guid[..8].copy_from_slice(&half(0));
    guid[8..].copy_from_slice(&half(1));
    // Mark it as a random (version 4, RFC 4122 variant) GUID.
    guid[7] = guid[7] & 0x0f | 0x40;
    guid[8] = guid[8] & 0x3f | 0x80;
    guid
}

fn header(
    entries: &[u8],
    disk_guid: [u8; GUID_LEN],
    own_lba: u64,
    other_lba: u64,
    entries_lba: u64,
    last_usable: u64,
) -> Vec<u8> {
    let mut header = vec![0u8; SECTOR as usize];
    header[..8].copy_from_slice(SIGNATURE);
    header[8..12].copy_from_slice(&REVISION_1_0.to_le_bytes());
    header[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    header[24..32].copy_from_slice(&own_lba.to_le_bytes());
    header[32..40].copy_from_slice(&other_lba.to_le_bytes());
    header[40..48].copy_from_slice(&(ALIGN / SECTOR).to_le_bytes());
    header[48..56].copy_from_slice(&last_usable.to_le_bytes());
    header[56..72].copy_from_slice(&disk_guid);
    header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    header[80..84].copy_from_slice(&(ENTRY_COUNT as u32).to_le_bytes());
    header[84..88].copy_from_slice(&(ENTRY_LEN as u32).to_le_bytes());
    header[88..92].copy_from_slice(&crc32(entries).to_le_bytes());
    // The header checksum is computed with its own field still zero.
    let checksum = crc32(&header[..HEADER_LEN]);
    header[16..20].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn protective_mbr(total_sectors: u64) -> Vec<u8> {
    let mut mbr = vec![0u8; SECTOR as usize];
    let covered = u32::try_from(total_sectors - 1).unwrap_or(u32::MAX);
    let entry = &mut mbr[MBR_PARTITION_AT..MBR_PARTITION_AT + 16];
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    entry[4] = MBR_TYPE_GPT_PROTECTIVE;
    entry[5..8].copy_from_slice(&[0xff, 0xff, 0xff]);
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    entry[12..16].copy_from_slice(&covered.to_le_bytes());
    mbr[SECTOR as usize - MBR_SIGNATURE.len()..].copy_from_slice(&MBR_SIGNATURE);
    mbr
}

/// Lays the partitions out one megabyte apart, in order, and wraps them in primary and backup tables.
pub fn assemble(partitions: Vec<Partition>) -> io::Result<Disk> {
    if partitions.len() > ENTRY_COUNT {
        return Err(refuse(copy::GPT_TOO_MANY));
    }
    let mut entries = vec![0u8; ENTRIES_LEN];
    let mut next = ALIGN;
    let mut names = String::new();
    for (partition, entry) in partitions
        .iter()
        .zip(entries.as_chunks_mut::<ENTRY_LEN>().0)
    {
        let len = partition.contents.len();
        let name: Vec<u16> = partition.name.encode_utf16().collect();
        if len == 0 || !len.is_multiple_of(SECTOR) {
            return Err(refuse(copy::GPT_BAD_SIZE));
        }
        if name.len() > MAX_NAME_UNITS {
            return Err(refuse(copy::GPT_NAME_TOO_LONG));
        }
        entry[..16].copy_from_slice(&TYPE_LINUX_DATA);
        entry[16..32].copy_from_slice(&guid(&partition.name));
        entry[32..40].copy_from_slice(&(next / SECTOR).to_le_bytes());
        entry[40..48].copy_from_slice(&((next + len) / SECTOR - 1).to_le_bytes());
        for (unit, slot) in name.iter().zip(entry[56..].as_chunks_mut::<2>().0) {
            slot.copy_from_slice(&unit.to_le_bytes());
        }
        names.push_str(&partition.name);
        names.push('/');
        next = (next + len).next_multiple_of(ALIGN);
    }
    // `next` is now where the backup entries start; the backup header is the last sector.
    let backup_entries_lba = next / SECTOR;
    let backup_header_lba = backup_entries_lba + ENTRIES_SECTORS;
    let last_usable = backup_entries_lba - 1;
    let disk_guid = guid(&names);

    let mut primary = protective_mbr(backup_header_lba + 1);
    primary.extend(header(
        &entries,
        disk_guid,
        PRIMARY_HEADER_LBA,
        backup_header_lba,
        PRIMARY_ENTRIES_LBA,
        last_usable,
    ));
    primary.extend(&entries);
    let mut backup = entries.clone();
    backup.extend(header(
        &entries,
        disk_guid,
        backup_header_lba,
        PRIMARY_HEADER_LBA,
        backup_entries_lba,
        last_usable,
    ));

    let mut disk = Disk::default();
    disk.push(primary.len() as u64, Source::Memory(primary));
    for partition in partitions {
        disk.pad_to(ALIGN);
        disk.append(partition.contents);
    }
    disk.pad_to(ALIGN);
    disk.push(backup.len() as u64, Source::Memory(backup));
    Ok(disk)
}

#[cfg(test)]
mod tests {
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use super::*;

    fn part(name: &str, contents: &[u8]) -> Partition {
        let mut disk = Disk::default();
        disk.push(contents.len() as u64, Source::Memory(contents.to_vec()));
        Partition {
            name: name.to_owned(),
            contents: disk,
        }
    }

    fn bytes_of(mut disk: Disk) -> Vec<u8> {
        let len = disk.len() as usize;
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), len)]).unwrap();
        disk.read_to_guest(&mem, GuestAddress(0), 0, len as u64)
            .unwrap();
        let mut out = vec![0; len];
        mem.read_slice(&mut out, GuestAddress(0)).unwrap();
        out
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// Checks one header as a GPT reader would: signature, its own checksum, and the entries checksum.
    fn check_header(image: &[u8], lba: u64) -> (u64, u64, u64) {
        let at = (lba * SECTOR) as usize;
        let header = &image[at..at + SECTOR as usize];
        assert_eq!(&header[..8], SIGNATURE);
        assert_eq!(u64_at(header, 24), lba, "header knows where it is");
        let mut zeroed = header[..HEADER_LEN].to_vec();
        zeroed[16..20].fill(0);
        assert_eq!(crc32(&zeroed), u32_at(header, 16), "header checksum");
        let entries_at = (u64_at(header, 72) * SECTOR) as usize;
        assert_eq!(
            crc32(&image[entries_at..entries_at + ENTRIES_LEN]),
            u32_at(header, 88),
            "entries checksum"
        );
        (u64_at(header, 32), u64_at(header, 72), u64_at(header, 48))
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn primary_and_backup_tables_are_valid_and_point_at_each_other() {
        let image = bytes_of(
            assemble(vec![part("misc", &[1; 1024]), part("super", &[2; 3 * 512])]).unwrap(),
        );
        let sectors = image.len() as u64 / SECTOR;
        assert_eq!(image.len() as u64 % SECTOR, 0);

        let (backup_lba, primary_entries, last_usable) = check_header(&image, PRIMARY_HEADER_LBA);
        assert_eq!(
            (backup_lba, primary_entries),
            (sectors - 1, PRIMARY_ENTRIES_LBA)
        );
        let (primary_lba, backup_entries, _) = check_header(&image, backup_lba);
        assert_eq!(
            (primary_lba, backup_entries),
            (PRIMARY_HEADER_LBA, sectors - 1 - ENTRIES_SECTORS)
        );
        assert_eq!(last_usable, backup_entries - 1);

        // Protective MBR covers the whole disk.
        assert_eq!(image[MBR_PARTITION_AT + 4], MBR_TYPE_GPT_PROTECTIVE);
        assert_eq!(u32_at(&image, MBR_PARTITION_AT + 12) as u64, sectors - 1);
        assert_eq!(&image[510..512], MBR_SIGNATURE);
    }

    #[test]
    fn entries_name_and_place_each_partition_and_the_contents_are_there() {
        let image = bytes_of(
            assemble(vec![part("misc", &[1; 1024]), part("super", &[2; 3 * 512])]).unwrap(),
        );
        let entry = |index: usize| {
            let at = (PRIMARY_ENTRIES_LBA * SECTOR) as usize + index * ENTRY_LEN;
            &image[at..at + ENTRY_LEN]
        };
        let name = |entry: &[u8]| {
            let units: Vec<u16> = entry[56..]
                .chunks(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|unit| *unit != 0)
                .collect();
            String::from_utf16(&units).unwrap()
        };
        let megabyte = ALIGN / SECTOR;
        assert_eq!(
            (name(entry(0)), u64_at(entry(0), 32), u64_at(entry(0), 40)),
            ("misc".into(), megabyte, megabyte + 1)
        );
        assert_eq!(
            (name(entry(1)), u64_at(entry(1), 32), u64_at(entry(1), 40)),
            ("super".into(), 2 * megabyte, 2 * megabyte + 2)
        );
        assert_eq!(&entry(0)[..16], TYPE_LINUX_DATA);
        assert_ne!(&entry(0)[16..32], &entry(1)[16..32], "unique GUIDs differ");
        assert!(
            entry(2).iter().all(|byte| *byte == 0),
            "unused entries are empty"
        );

        assert!(
            image[ALIGN as usize..ALIGN as usize + 1024]
                .iter()
                .all(|byte| *byte == 1)
        );
        assert!(
            image[2 * ALIGN as usize..2 * ALIGN as usize + 1536]
                .iter()
                .all(|byte| *byte == 2)
        );
        assert!(
            image[ALIGN as usize + 1024..2 * ALIGN as usize]
                .iter()
                .all(|byte| *byte == 0),
            "gap is zero"
        );
    }

    #[test]
    fn same_parts_give_the_same_disk() {
        let build =
            || bytes_of(assemble(vec![part("a", &[0; 512]), part("b", &[0; 512])]).unwrap());
        assert_eq!(build(), build());
    }

    #[test]
    fn unusable_partitions_are_refused() {
        assert!(assemble(vec![part("odd", &[0; 100])]).is_err());
        assert!(assemble(vec![part("empty", &[])]).is_err());
        assert!(assemble(vec![part(&"n".repeat(MAX_NAME_UNITS + 1), &[0; 512])]).is_err());
        assert!(assemble(vec![part(&"n".repeat(MAX_NAME_UNITS), &[0; 512])]).is_ok());
        let too_many: Vec<Partition> = (0..=ENTRY_COUNT)
            .map(|index| part(&format!("p{index}"), &[0; 512]))
            .collect();
        assert!(assemble(too_many).is_err());
    }
}
