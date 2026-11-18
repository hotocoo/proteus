//! Turns a directory of AOSP virtual-device images into what the board boots: one GPT disk whose
//! partitions are the image files, a ramdisk made of the images' own ramdisks plus a small overlay,
//! and the bootconfig parameters Android's init needs to find its way around.
//!
//! The images are used as they are, read-only. Everything the guest writes goes to files in a separate
//! data directory, which are created on first use.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};

use arch::aarch64::layout;
use arch::android::bootimg::{self, BOOT_HEADER_LEN, VENDOR_HEADER_LEN};
use arch::android::{cpio, lz4};
use devices::disk::gpt::{self, Partition, SECTOR};
use devices::disk::{Disk, sparse};
use devices::virtio::blk::DiskImage;

use crate::config::AndroidOptions;
use crate::{VmmError, copy};

const MIB: u64 = 1 << 20;

/// Slot the images are presented as. The image set has one copy of everything, so there is only "a".
const SLOT_SUFFIX: &str = "_a";

/// What a partition holds.
enum Contents {
    /// An image file from the images directory, read-only. `required` images must exist.
    Image { file: &'static str, required: bool },
    /// A file in the data directory, created empty at this size if missing. Android formats it itself.
    Scratch { file: &'static str, bytes: u64 },
    /// As `Scratch`, sized by the user.
    Userdata { file: &'static str },
}

/// The disk layout, in order. Names are what Android's fstab and verified boot look up under
/// `/dev/block/by-name/`.
const PARTITIONS: &[(&str, Contents)] = &[
    (
        "misc",
        Contents::Scratch {
            file: "misc.img",
            bytes: MIB,
        },
    ),
    (
        "frp",
        Contents::Scratch {
            file: "frp.img",
            bytes: MIB,
        },
    ),
    (
        "metadata",
        Contents::Scratch {
            file: "metadata.img",
            bytes: 64 * MIB,
        },
    ),
    (
        "boot_a",
        Contents::Image {
            file: "boot.img",
            required: false,
        },
    ),
    (
        "init_boot_a",
        Contents::Image {
            file: INIT_BOOT_IMAGE,
            required: true,
        },
    ),
    (
        "vendor_boot_a",
        Contents::Image {
            file: VENDOR_BOOT_IMAGE,
            required: true,
        },
    ),
    (
        "vbmeta_a",
        Contents::Image {
            file: "vbmeta.img",
            required: true,
        },
    ),
    (
        "vbmeta_system_a",
        Contents::Image {
            file: "vbmeta_system.img",
            required: false,
        },
    ),
    (
        "vbmeta_vendor_dlkm_a",
        Contents::Image {
            file: "vbmeta_vendor_dlkm.img",
            required: false,
        },
    ),
    (
        "vbmeta_system_dlkm_a",
        Contents::Image {
            file: "vbmeta_system_dlkm.img",
            required: false,
        },
    ),
    (
        "super",
        Contents::Image {
            file: "super.img",
            required: true,
        },
    ),
    (
        "userdata",
        Contents::Userdata {
            file: "userdata.img",
        },
    ),
];

const INIT_BOOT_IMAGE: &str = "init_boot.img";
const VENDOR_BOOT_IMAGE: &str = "vendor_boot.img";

/// Bootconfig the VMM sets for every Android boot. The vendor's own bootconfig comes first and these
/// replace any key it also sets.
const BOOTCONFIG: &[(&str, &str)] = &[
    ("androidboot.slot_suffix", SLOT_SUFFIX),
    // The ramdisk also holds the recovery system; this selects the normal one.
    ("androidboot.force_normal_boot", "1"),
    // Unlocked: verified boot still sets up dm-verity, but does not need a bootloader's seal.
    ("androidboot.verifiedbootstate", "orange"),
    ("androidboot.fstab_suffix", "cf.f2fs.hctr2"),
    ("androidboot.serialno", "EMU0001"),
];
/// Tells init which block device holds the named partitions: the disk in virtio slot 0.
const BOOT_DEVICES_KEY: &str = "androidboot.boot_devices";
/// Kernel arguments the vendor image asks for that this board must not pass on.
/// The guest kernel is built without the Rust binder driver.
const CMDLINE_DROPPED: &[&str] = &["binder.impl=rust"];

/// The image set ships kernel modules for its own kernel build. This board's kernel has every driver
/// it needs built in and cannot load those modules, so the list of modules to load is replaced by an
/// empty one.
const MODULE_LOAD_LIST: &str = "lib/modules/modules.load";

/// What the images contribute to the boot, besides the disk.
pub struct AndroidBoot {
    /// Ramdisk archives, in the order the kernel must unpack them.
    pub ramdisks: Vec<(File, Range<u64>)>,
    pub overlay: Vec<u8>,
    pub bootconfig: Vec<(String, String)>,
    pub cmdline: String,
}

fn file_error(path: &Path) -> impl FnOnce(std::io::Error) -> VmmError + '_ {
    move |source| VmmError::File {
        path: path.to_owned(),
        source,
    }
}

fn image(path: &Path) -> Result<Disk, VmmError> {
    let mut file = File::open(path).map_err(file_error(path))?;
    let mut disk = Disk::default();
    if sparse::is_sparse(&mut file).map_err(file_error(path))? {
        sparse::append(&mut disk, file).map_err(file_error(path))?;
    } else {
        let len = file.metadata().map_err(file_error(path))?.len();
        disk = Disk::from_file(file, len, false);
    }
    disk.pad_to(SECTOR);
    Ok(disk)
}

fn scratch(path: &Path, bytes: u64) -> Result<Disk, VmmError> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(file_error(path))?;
    let len = file.metadata().map_err(file_error(path))?.len();
    // A new file gets its size; an existing one keeps whatever size it was created with.
    let len = if len == 0 {
        file.set_len(bytes).map_err(file_error(path))?;
        bytes
    } else {
        len
    };
    let mut disk = Disk::from_file(file, len, true);
    disk.pad_to(SECTOR);
    Ok(disk)
}

fn read_header<const N: usize>(path: &Path) -> Result<(File, [u8; N], u64), VmmError> {
    let mut file = File::open(path).map_err(file_error(path))?;
    let len = file.seek(SeekFrom::End(0)).map_err(file_error(path))?;
    file.rewind().map_err(file_error(path))?;
    let mut header = [0u8; N];
    let read = file.read(&mut header).map_err(file_error(path))?;
    // A short file leaves zeros, which the header check then rejects by its magic.
    let _ = read;
    Ok((file, header, len))
}

/// `key=value` lines as the vendor image stores them. Values may be quoted.
fn parse_bootconfig(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.trim().split_once('='))
        .map(|(key, value)| {
            (
                key.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            )
        })
        .collect()
}

fn set(params: &mut Vec<(String, String)>, key: &str, value: &str) {
    params.retain(|(existing, _)| existing != key);
    params.push((key.to_owned(), value.to_owned()));
}

pub fn assemble(options: &AndroidOptions) -> Result<(DiskImage, AndroidBoot), VmmError> {
    std::fs::create_dir_all(&options.data).map_err(file_error(&options.data))?;
    let image_path = |file: &str| -> PathBuf { options.images.join(file) };

    let mut partitions = Vec::new();
    for (name, contents) in PARTITIONS {
        let disk = match contents {
            Contents::Image { file, required } => {
                let path = image_path(file);
                if !required && !path.exists() {
                    continue;
                }
                image(&path)?
            }
            Contents::Scratch { file, bytes } => scratch(&options.data.join(file), *bytes)?,
            Contents::Userdata { file } => {
                scratch(&options.data.join(file), options.userdata_bytes)?
            }
        };
        partitions.push(Partition {
            name: (*name).to_owned(),
            contents: disk,
        });
    }
    let disk = gpt::assemble(partitions).map_err(|err| VmmError::Device(err.to_string()))?;

    let init_boot_path = image_path(INIT_BOOT_IMAGE);
    let (init_boot, header, len) = read_header::<BOOT_HEADER_LEN>(&init_boot_path)?;
    let generic = bootimg::boot_ramdisk(&header, len)?;

    let vendor_boot_path = image_path(VENDOR_BOOT_IMAGE);
    let (mut vendor_boot, header, len) = read_header::<VENDOR_HEADER_LEN>(&vendor_boot_path)?;
    let vendor = bootimg::vendor_boot(&header, len)?;
    let mut vendor_bootconfig =
        vec![0u8; (vendor.bootconfig.end - vendor.bootconfig.start) as usize];
    vendor_boot
        .seek(SeekFrom::Start(vendor.bootconfig.start))
        .map_err(file_error(&vendor_boot_path))?;
    vendor_boot
        .read_exact(&mut vendor_bootconfig)
        .map_err(file_error(&vendor_boot_path))?;

    let mut bootconfig = parse_bootconfig(&String::from_utf8_lossy(&vendor_bootconfig));
    for (key, value) in BOOTCONFIG {
        set(&mut bootconfig, key, value);
    }
    let boot_device = format!("{:x}.virtio_mmio", layout::virtio_mmio_base(0));
    set(&mut bootconfig, BOOT_DEVICES_KEY, &boot_device);

    let cmdline: Vec<&str> = vendor
        .cmdline
        .split_whitespace()
        .filter(|arg| !CMDLINE_DROPPED.contains(arg))
        .collect();
    let mut overlay = cpio::Archive::default();
    overlay.file(MODULE_LOAD_LIST, b"");

    let boot = AndroidBoot {
        ramdisks: vec![(init_boot, generic), (vendor_boot, vendor.ramdisk)],
        // The image ramdisks before it are LZ4 legacy archives, so this one has to be as well.
        overlay: lz4::stored(&overlay.finish()),
        bootconfig,
        cmdline: cmdline.join(" "),
    };
    Ok((
        DiskImage::from_disk(disk, false, copy::ANDROID_DISK_ID),
        boot,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_bootconfig_lines_are_split_at_the_first_equals_sign() {
        let parsed = parse_bootconfig(
            "androidboot.hardware=cutf_cvm\nkernel.x.y = \"16384\"\n\nnot a pair\na=b=c\n",
        );
        assert_eq!(
            parsed,
            [
                ("androidboot.hardware".to_owned(), "cutf_cvm".to_owned()),
                ("kernel.x.y".to_owned(), "16384".to_owned()),
                ("a".to_owned(), "b=c".to_owned()),
            ]
        );
    }

    #[test]
    fn setting_a_key_replaces_the_vendor_value() {
        let mut params =
            parse_bootconfig("androidboot.serialno=VENDOR\nandroidboot.hardware=cutf_cvm\n");
        set(&mut params, "androidboot.serialno", "EMU0001");
        assert_eq!(
            params,
            [
                ("androidboot.hardware".to_owned(), "cutf_cvm".to_owned()),
                ("androidboot.serialno".to_owned(), "EMU0001".to_owned()),
            ]
        );
    }

    #[test]
    fn every_partition_name_fits_a_gpt_entry_and_is_unique() {
        let names: Vec<&str> = PARTITIONS.iter().map(|(name, _)| *name).collect();
        assert!(names.iter().all(|name| name.len() <= 36));
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
    }
}
