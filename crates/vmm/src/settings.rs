//! An instance's settings as the settings panel shows them: its saved options (the `args` file),
//! read into named fields and written back. Options the panel does not show (the kernel, the image,
//! disks…) are kept as they are.

use crate::config::{DEFAULT_DISPLAY, DEFAULT_RAM_MIB, DEFAULT_VCPUS};
use net::config::ADB_PORT as DEFAULT_ADB_PORT;

/// What happens to sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    On,
    /// A sound card the guest can use, playing nothing.
    Muted,
    /// No sound card.
    Off,
}

/// Whether the guest has a gamepad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gamepad {
    /// When one is plugged into this machine at start.
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub ram_mib: u64,
    pub vcpus: u32,
    pub display: (u32, u32),
    pub gpu: bool,
    pub sound: Sound,
    pub gamepad: Gamepad,
    pub network: bool,
    pub adb_port: u16,
    pub allow_quic: bool,
    pub host_loopback: bool,
    pub blocklists: Vec<String>,
    pub allowlists: Vec<String>,
    pub keymap: String,
    pub keymap_dir: String,
    /// Every other option, in order, values included.
    pub other: Vec<String>,
}

const RAM: &str = "--ram-mib";
const VCPUS: &str = "--vcpus";
const DISPLAY: &str = "--display";
const GPU: &str = "--gpu";
const MUTE: &str = "--mute";
const NO_SOUND: &str = "--no-sound";
const GAMEPAD: &str = "--gamepad";
const NO_GAMEPAD: &str = "--no-gamepad";
const NO_NET: &str = "--no-net";
const ADB_PORT: &str = "--adb-port";
const ALLOW_QUIC: &str = "--allow-quic";
const HOST_LOOPBACK: &str = "--host-loopback";
const BLOCKLIST: &str = "--blocklist";
const ALLOWLIST: &str = "--allowlist";
const KEYMAP: &str = "--keymap";
const KEYMAP_DIR: &str = "--keymap-dir";
const SIZE_SEPARATOR: char = 'x';

/// Options that take a value, of those the panel shows.
const WITH_VALUE: [&str; 8] = [
    RAM, VCPUS, DISPLAY, ADB_PORT, BLOCKLIST, ALLOWLIST, KEYMAP, KEYMAP_DIR,
];

impl Settings {
    /// Reads saved options. A value that does not parse keeps the default, as the emulator would
    /// refuse it anyway.
    pub fn from_args(args: &[String]) -> Self {
        let mut settings = Self {
            ram_mib: DEFAULT_RAM_MIB,
            vcpus: DEFAULT_VCPUS,
            display: DEFAULT_DISPLAY,
            gpu: false,
            sound: Sound::On,
            gamepad: Gamepad::Auto,
            network: true,
            adb_port: DEFAULT_ADB_PORT,
            allow_quic: false,
            host_loopback: false,
            blocklists: Vec::new(),
            allowlists: Vec::new(),
            keymap: String::new(),
            keymap_dir: String::new(),
            other: Vec::new(),
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let (option, inline) = match arg.split_once('=') {
                Some((option, value)) if option.starts_with("--") => {
                    (option, Some(value.to_owned()))
                }
                _ => (arg.as_str(), None),
            };
            let shown = WITH_VALUE.contains(&option);
            let value = if shown {
                inline.or_else(|| iter.next().cloned()).unwrap_or_default()
            } else {
                String::new()
            };
            match option {
                RAM => settings.ram_mib = value.parse().unwrap_or(settings.ram_mib),
                VCPUS => settings.vcpus = value.parse().unwrap_or(settings.vcpus),
                DISPLAY => settings.display = parse_size(&value).unwrap_or(settings.display),
                ADB_PORT => settings.adb_port = value.parse().unwrap_or(settings.adb_port),
                BLOCKLIST => settings.blocklists.push(value),
                ALLOWLIST => settings.allowlists.push(value),
                KEYMAP => settings.keymap = value,
                KEYMAP_DIR => settings.keymap_dir = value,
                GPU => settings.gpu = true,
                MUTE => settings.sound = Sound::Muted,
                NO_SOUND => settings.sound = Sound::Off,
                GAMEPAD => settings.gamepad = Gamepad::Always,
                NO_GAMEPAD => settings.gamepad = Gamepad::Never,
                NO_NET => settings.network = false,
                ALLOW_QUIC => settings.allow_quic = true,
                HOST_LOOPBACK => settings.host_loopback = true,
                _ => settings.other.push(arg.clone()),
            }
        }
        settings
    }

    /// The options that start the emulator this way. Defaults are left out.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = self.other.clone();
        let mut value = |option: &str, value: String| {
            args.push(option.to_owned());
            args.push(value);
        };
        if self.ram_mib != DEFAULT_RAM_MIB {
            value(RAM, self.ram_mib.to_string());
        }
        if self.vcpus != DEFAULT_VCPUS {
            value(VCPUS, self.vcpus.to_string());
        }
        if self.display != DEFAULT_DISPLAY {
            value(
                DISPLAY,
                format!("{}{SIZE_SEPARATOR}{}", self.display.0, self.display.1),
            );
        }
        if self.adb_port != DEFAULT_ADB_PORT {
            value(ADB_PORT, self.adb_port.to_string());
        }
        for list in self.blocklists.iter().filter(|list| !list.is_empty()) {
            value(BLOCKLIST, list.clone());
        }
        for list in self.allowlists.iter().filter(|list| !list.is_empty()) {
            value(ALLOWLIST, list.clone());
        }
        if !self.keymap.is_empty() {
            value(KEYMAP, self.keymap.clone());
        }
        if !self.keymap_dir.is_empty() {
            value(KEYMAP_DIR, self.keymap_dir.clone());
        }
        let flags = [
            (self.gpu, GPU),
            (self.sound == Sound::Muted, MUTE),
            (self.sound == Sound::Off, NO_SOUND),
            (self.gamepad == Gamepad::Always, GAMEPAD),
            (self.gamepad == Gamepad::Never, NO_GAMEPAD),
            (!self.network, NO_NET),
            (self.allow_quic, ALLOW_QUIC),
            (self.host_loopback, HOST_LOOPBACK),
        ];
        args.extend(
            flags
                .into_iter()
                .filter(|(on, _)| *on)
                .map(|(_, flag)| flag.to_owned()),
        );
        args
    }
}

/// `WxH` as a pair of pixel counts.
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once(SIZE_SEPARATOR)?;
    let size = (width.parse().ok()?, height.parse().ok()?);
    (size.0 > 0 && size.1 > 0).then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn saved_options_come_back_the_same_after_a_trip_through_the_panel() {
        let args = strings(&[
            "--kernel",
            "/k",
            "--android",
            "/img dir",
            "--ram-mib",
            "2048",
            "--display=1080x1920",
            "--gpu",
            "--mute",
            "--no-gamepad",
            "--blocklist",
            "/ads.txt",
            "--keymap-dir",
            "/maps",
        ]);
        let settings = Settings::from_args(&args);
        assert_eq!(settings.ram_mib, 2048);
        assert_eq!(settings.display, (1080, 1920));
        assert!(settings.gpu);
        assert_eq!(settings.sound, Sound::Muted);
        assert_eq!(settings.gamepad, Gamepad::Never);
        assert_eq!(settings.blocklists, ["/ads.txt"]);
        assert_eq!(
            settings.other,
            strings(&["--kernel", "/k", "--android", "/img dir"])
        );
        assert_eq!(Settings::from_args(&settings.to_args()), settings);
        // The emulator reads them back the way the panel meant them.
        let mut check = settings.to_args();
        check.extend(strings(&["--data-dir", "x"]));
        let check: Vec<std::ffi::OsString> = check.into_iter().map(Into::into).collect();
        assert!(crate::config::parse(check).is_ok());
    }

    #[test]
    fn defaults_are_not_written_out() {
        let settings = Settings::from_args(&strings(&["--kernel", "/k"]));
        assert_eq!(settings.to_args(), strings(&["--kernel", "/k"]));
    }

    #[test]
    fn sizes_need_two_positive_numbers() {
        assert_eq!(parse_size("720x1280"), Some((720, 1280)));
        assert_eq!(parse_size("0x1280"), None);
        assert_eq!(parse_size("720"), None);
    }
}
