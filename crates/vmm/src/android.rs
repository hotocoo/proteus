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
use std::sync::{Mutex, PoisonError};

#[cfg(target_arch = "aarch64")]
use arch::aarch64::layout;
use arch::android::bootimg::{self, BOOT_HEADER_LEN, VENDOR_HEADER_LEN};
use arch::android::{cpio, lz4};
use devices::disk::Disk;
use devices::disk::gpt::{self, Partition, SECTOR};
use devices::virtio::blk::DiskImage;

use crate::config::AndroidOptions;
use crate::{VmmError, copy};

const MIB: u64 = 1 << 20;
/// The file in the data folder that an emulator locks while it runs on that folder.
const DATA_LOCK_FILE: &str = ".lock";

/// Slot the images are presented as. The image set has one copy of everything, so there is only "a".
const SLOT_SUFFIX: &str = "_a";
/// The guest's vsock address; the image's services expect 3, as on Cuttlefish.
pub const GUEST_CID: u64 = 3;

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
/// The data partition's file in an Android data folder.
pub(crate) const USERDATA_FILE: &str = "userdata.img";

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
            file: USERDATA_FILE,
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
    // The image carries several implementations of these services and picks one by name. The ones
    // chosen here run entirely inside the guest; the image's defaults need helper programs on the host.
    (
        "androidboot.vendor.apex.com.android.hardware.keymint",
        "com.android.hardware.keymint.rust_nonsecure",
    ),
    (
        "androidboot.vendor.apex.com.android.hardware.gatekeeper",
        "com.android.hardware.gatekeeper.nonsecure",
    ),
    (
        "androidboot.vendor.apex.com.android.hardware.graphics.composer",
        "com.android.hardware.graphics.composer.ranchu",
    ),
    (
        "androidboot.vendor.apex.com.google.emulated.camera.provider.hal",
        "com.google.emulated.camera.provider.hal",
    ),
    ("androidboot.enable_confirmationui", "0"),
    ("androidboot.cuttlefish_service_bluetooth_checker", "false"),
    // Graphics drawn by the guest's CPU and shown through the display device's 2D commands:
    // ANGLE on top of the SwiftShader Vulkan driver ("pastel"), Vulkan 1.2, OpenGL ES 3.1.
    ("androidboot.hardware.gralloc", "minigbm"),
    ("androidboot.hardware.hwcomposer", "ranchu"),
    ("androidboot.hardware.hwcomposer.display_finder_mode", "drm"),
    (
        "androidboot.hardware.hwcomposer.display_framebuffer_format",
        "rgba",
    ),
    ("androidboot.hardware.egl", "angle"),
    ("androidboot.hardware.vulkan", "pastel"),
    ("androidboot.cpuvulkan.version", "4202496"),
    ("androidboot.opengles.version", "196609"),
    ("androidboot.lcd_density", "320"),
    // The network card is used as it is, as wired Ethernet, which Android brings up by itself.
    // (The image can also wrap it as a Wi-Fi network, "virt_wifi", but then Wi-Fi has to be
    // switched on and joined by hand before anything can reach the guest.)
    ("androidboot.wifi_impl", "none"),
    ("androidboot.wifi_mac_prefix", "5554"),
    ("androidboot.setupwizard_mode", "DISABLED"),
    ("androidboot.enable_bootanimation", "1"),
    // Software rendering is slow; the image scales its watchdog timeouts by this.
    ("androidboot.hw_timeout_multiplier", "3"),
    // Services that refuse to start without these. Nothing on the host listens on the port, and
    // there is no thread radio; the lights service only needs a port it is allowed to bind.
    ("androidboot.vsock_lights_port", "6900"),
    ("androidboot.vsock_lights_cid", "3"),
    ("androidboot.modem_simulator_ports", "9600"),
    ("androidboot.openthread_node_id", "1"),
];
/// Terminals (`/dev/hvc0` and up) the image's hardware services open at start. Most would talk to
/// helper programs that this emulator does not have; the services only need the terminals to exist.
pub const CONSOLE_PORTS: usize = 20;
/// The terminal the image writes its system log to.
pub const LOGCAT_PORT: usize = 2;
/// Amount of RAM as Android's memory tuning reads it, in whole megabytes with this suffix.
const DDR_SIZE_KEY: &str = "androidboot.ddr_size";
const DDR_SIZE_SUFFIX: &str = "MB";
/// With a serial port, init runs a shell on it; the device is the one `SERIAL_CONSOLE_ARG` names.
const CONSOLE_ON: &[(&str, &str)] = &[
    ("androidboot.console", "ttyS0"),
    ("androidboot.serialconsole", "1"),
];
const CONSOLE_OFF: &[(&str, &str)] = &[("androidboot.serialconsole", "0")];
/// Drawing through gfxstream on the host's GPU instead of in software: the image's gfxstream Vulkan
/// driver, ANGLE on top of it, and no CPU Vulkan. These are the values Google's Cuttlefish launcher
/// uses for its `gfxstream_guest_angle` mode; they replace the software ones in `BOOTCONFIG`. The
/// launcher also picks a Vulkan APEX variant, but only when the image has several; the prebuilt image
/// has one, and naming a variant it lacks keeps the APEX from mounting at all.
const GPU_GFXSTREAM: &[(&str, &str)] = &[
    ("androidboot.hardware.vulkan", "ranchu"),
    ("androidboot.hardware.egl", "angle"),
    ("androidboot.cpuvulkan.version", "0"),
    ("androidboot.hardware.gltransport", "virtio-gpu-asg"),
    ("androidboot.hw_timeout_multiplier", "1"),
];
/// The first device `virtio_mmio.device=` creates, as init names it: its path under
/// `/sys/devices/platform/`, where the kernel puts command-line devices under a parent of their own.
#[cfg(not(target_arch = "aarch64"))]
const BOOT_DEVICE_FROM_CMDLINE: &str = "virtio-mmio-cmdline/virtio-mmio.0";
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
    pub ramdisks: Vec<(arch::BootFile, Range<u64>)>,
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

/// Images `--writable-system` gives each instance a copy of: the system partitions, and the
/// verified-boot metadata, where Android records that verity is off so it can overlay them.
const OWN_COPIES: [&str; 5] = [
    "super.img",
    "vbmeta.img",
    "vbmeta_system.img",
    "vbmeta_vendor_dlkm.img",
    "vbmeta_system_dlkm.img",
];
/// Beside the copy: which image it was made from (length and modification time), so a changed
/// image is copied again instead of mixing its system with another version's boot images.
const COPY_SOURCE_SUFFIX: &str = ".source";
/// The copy's name while it is being made, so a stopped copy is never taken for a whole one.
const COPY_PARTIAL_SUFFIX: &str = ".partial";

/// `copy`, writable, made from the image at `original` if it is missing or was made from another
/// version of it. Android's sparse format is unpacked; holes stay holes, so on APFS the copy takes
/// only the space the partitions really use.
fn own_copy(original: &Path, copy: &Path) -> Result<Disk, VmmError> {
    let stamp = |path: &Path| -> Option<String> {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(format!("{} {}", meta.len(), modified.as_secs()))
    };
    let mut marker = copy.as_os_str().to_owned();
    marker.push(COPY_SOURCE_SUFFIX);
    let marker = PathBuf::from(marker);
    let wanted = stamp(original).ok_or_else(|| VmmError::File {
        path: original.to_owned(),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    })?;
    let current = std::fs::read_to_string(&marker).ok();
    if !copy.is_file() || current.as_deref() != Some(wanted.as_str()) {
        log::info!("{}", copy::system_copy_made(copy));
        let (source, _) =
            devices::disk::open_image(original, false).map_err(file_error(original))?;
        let mut partial = copy.as_os_str().to_owned();
        partial.push(COPY_PARTIAL_SUFFIX);
        let partial = PathBuf::from(partial);
        let mut out = File::create(&partial).map_err(file_error(&partial))?;
        source.write_to(&mut out).map_err(file_error(&partial))?;
        out.sync_all().map_err(file_error(&partial))?;
        std::fs::rename(&partial, copy).map_err(file_error(copy))?;
        std::fs::write(&marker, &wanted).map_err(file_error(&marker))?;
    }
    let (mut disk, _) = devices::disk::open_image(copy, true).map_err(file_error(copy))?;
    disk.pad_to(SECTOR);
    Ok(disk)
}

fn image(path: &Path) -> Result<Disk, VmmError> {
    let (mut disk, _) = devices::disk::open_image(path, false).map_err(file_error(path))?;
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
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
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

/// Bootconfig that depends on how this machine is set up rather than on the image.
pub fn machine_bootconfig(serial: bool, ram_bytes: u64, gpu: bool) -> Vec<(String, String)> {
    let console = if serial { CONSOLE_ON } else { CONSOLE_OFF };
    let graphics = if gpu { GPU_GFXSTREAM } else { &[] };
    let ddr = format!("{}{DDR_SIZE_SUFFIX}", ram_bytes / MIB);
    console
        .iter()
        .chain(graphics)
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .chain([(DDR_SIZE_KEY.to_owned(), ddr)])
        .collect()
}

/// Data folders this process has locked. The lock lasts as long as the file is open, so they stay
/// open until the process ends.
static DATA_LOCKS: Mutex<Vec<(PathBuf, File)>> = Mutex::new(Vec::new());

/// Takes the exclusive lock on `data`, so that two emulators never write the same disk images.
fn lock_data(data: &Path) -> Result<(), VmmError> {
    let path = data.join(DATA_LOCK_FILE);
    let mut locks = DATA_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    // A guest reboot assembles again in the same process, which already holds the lock.
    if locks.iter().any(|(held, _)| *held == path) {
        return Ok(());
    }
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(file_error(&path))?;
    if file.try_lock().is_err() {
        return Err(VmmError::Config(copy::DATA_DIR_LOCKED));
    }
    locks.push((path, file));
    Ok(())
}

pub fn assemble(options: &AndroidOptions) -> Result<(DiskImage, AndroidBoot), VmmError> {
    std::fs::create_dir_all(&options.data).map_err(file_error(&options.data))?;
    lock_data(&options.data)?;
    let image_path = |file: &str| -> PathBuf { options.images.join(file) };

    let mut partitions = Vec::new();
    for (name, contents) in PARTITIONS {
        let disk = match contents {
            Contents::Image { file, .. }
                if options.writable_system
                    && OWN_COPIES.contains(file)
                    && image_path(file).exists() =>
            {
                own_copy(&image_path(file), &options.data.join(file))?
            }
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
    // The platform device of the disk in slot 0, as it is named under /sys/devices/platform: by
    // address when a device tree describes it, by number when the command line does.
    #[cfg(target_arch = "aarch64")]
    let boot_device = format!("{:x}.virtio_mmio", layout::virtio_mmio_base(0));
    #[cfg(not(target_arch = "aarch64"))]
    let boot_device = BOOT_DEVICE_FROM_CMDLINE.to_owned();
    set(&mut bootconfig, BOOT_DEVICES_KEY, &boot_device);

    let cmdline: Vec<&str> = vendor
        .cmdline
        .split_whitespace()
        .filter(|arg| !CMDLINE_DROPPED.contains(arg))
        .collect();
    let mut overlay = cpio::Archive::default();
    overlay.file(MODULE_LOAD_LIST, b"");

    let boot = AndroidBoot {
        // `BootFile` is `File` itself on Unix, so the conversion only does something on Windows.
        #[allow(clippy::useless_conversion)]
        ramdisks: vec![
            (init_boot.into(), generic),
            (vendor_boot.into(), vendor.ramdisk),
        ],
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
    fn a_data_folder_is_locked_by_one_emulator_at_a_time() {
        let dir = std::env::temp_dir().join(format!("proteus-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        // Someone else (another file description, as another emulator would have) holds `other`.
        let held = File::create(other.join(DATA_LOCK_FILE)).unwrap();
        held.try_lock().unwrap();
        assert!(matches!(lock_data(&other), Err(VmmError::Config(_))));
        lock_data(&dir).unwrap();
        // A guest reboot assembles again in this process, which already holds the lock.
        lock_data(&dir).unwrap();
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn machine_bootconfig_follows_the_serial_port_and_ram_size() {
        let with = machine_bootconfig(true, 4096 * MIB, false);
        assert!(with.contains(&("androidboot.console".to_owned(), "ttyS0".to_owned())));
        assert!(with.contains(&("androidboot.serialconsole".to_owned(), "1".to_owned())));
        assert!(with.contains(&("androidboot.ddr_size".to_owned(), "4096MB".to_owned())));
        let without = machine_bootconfig(false, 2048 * MIB, false);
        let accelerated = machine_bootconfig(false, 2048 * MIB, true);
        let value = |key: &str| {
            let found = accelerated.iter().find(|(k, _)| k == key);
            found.map(|(_, v)| v.as_str())
        };
        assert_eq!(value("androidboot.hardware.vulkan"), Some("ranchu"));
        assert_eq!(value("androidboot.cpuvulkan.version"), Some("0"));
        assert!(without.contains(&("androidboot.serialconsole".to_owned(), "0".to_owned())));
        assert!(!without.iter().any(|(key, _)| key == "androidboot.console"));
    }

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
    fn bootconfig_comments_and_blank_lines_are_skipped() {
        let parsed = parse_bootconfig("# a = b\n\n  # c = d\nk=v\n");
        assert_eq!(parsed, [("k".to_owned(), "v".to_owned())]);
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
