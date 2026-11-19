<p align="center"><img src="assets/logo.png" width="160" alt="Proteus: a bronze coin with a trident over waves"></p>

# Proteus

Named for the Greek sea god who could take any shape: an emulator becomes whatever machine it is asked to be.

A small virtual machine monitor in Rust, being built into a lightweight Android emulator for games.
No QEMU, PCI, UEFI or ACPI: the guest sees a virtio-mmio board and is booted directly from a kernel image.

## Start here (Mac)

1. Download `proteus-<version>-macos-arm64-app.zip` from the latest release, unzip it, and drag
   **Proteus.app** to Applications. The app is not notarised, so the first time macOS refuses it; run
   `xattr -dr com.apple.quarantine /Applications/Proteus.app` once, then open it.
2. Get an Android image (this project may not redistribute Google's): open
   <https://ci.android.com/builds/branches/aosp-android-latest-release/grid>, pick a green build of
   `aosp_cf_arm64_only_phone-userdebug`, download `aosp_cf_arm64_only_phone-img-<build>.zip` from its
   artifacts (about 1.1 GB) and unzip it into a folder.
3. Open Proteus. Under **New instance**, choose that folder as the image, keep "Use this computer's GPU" on,
   and press **Create and start**. Android boots in about 20 seconds; swipe up to unlock.

Each instance is a phone of its own. The launcher starts, stops and deletes them, and **Settings**
changes everything about one: the Android image (ROM) and kernel, memory, CPUs, screen size, frame
rate, GPU, sound, gamepad, key maps, network, storage size, and a factory reset. **Quick setup**
fills memory, CPUs, frame rate and GPU in one click: **Gaming** (8 CPUs, 8 GB, 120 Hz, GPU, capped
to what the Mac can spare), **Balanced** or **Low-end**.

## Gaming

- **Frame rate up to 144 Hz.** Android runs at the rate you pick in Settings (60, 90, 120 or 144 Hz;
  `--refresh-hz` on the command line), not the 60 Hz it used to be stuck at: the display tells
  Android its rate the way a real panel does (an EDID). Measured on an M4 Max: Android's Settings
  scrolled at about 110 frames per second at 120 Hz, against at most 60 before. Not yet measured in
  a game: a game's own frame cap, its 90/120 fps options, and your Mac's screen (60 Hz on most
  displays without ProMotion) still decide what you see. Existing instances keep 60 Hz until you
  pick a rate (or Quick setup, Gaming) and restart them.
- **Lower input lag, by design (not yet measured):** the window shows each frame as soon as Android
  draws it, with no frame queued ahead, and at 120 Hz Android's own frame pipeline is half as long.
- **FPS counter** at the top of the toolbar: frames Android showed in the last second.
- **Any window shape, no black bars.** Drag the window to any size or shape: once you let go, Android
  is told the new shape and lays itself out to fill it, at the screen's full sharpness. Portrait
  stays portrait (at most square) and landscape stays landscape, so a game turning the screen keeps
  a sensible shape. A running game is told its screen changed size, and some games restart for
  that. Limits: screenshots, recordings and `--frame-dump` still show the whole panel, bars
  included; key maps and macros hold screen positions, so a key map made for one shape misses after
  the window changes shape.
- **Render scale (upscaler)** per game, in **Apps**: draw a game at 85%, 70% or 50% of the screen and
  let Android scale it up (Android's own game mode), which lightens a busy GPU. 100% turns it off.
  Only for apps that say they are games (others are refused, with Android's reason shown); the game
  is restarted so it takes effect. Not yet tried on a real game.
- **Trimmed Android.** Every boot turns off what a game machine has no use for: Android's ad
  services and the on-device ad and learning services behind them, remote key provisioning (which
  calls Google's servers), device-lock financing, printing, screensavers, satellite messaging and
  demo apps. `proteus ctl <instance> shell pm enable <package>` brings one back.
- **Blocking ads and trackers** is yours to turn on: add a blocklist file (plain domains, hosts or
  adblock format, such as the lists of the StevenBlack or HaGeZi projects) under **Settings, Network**.
  Blocking a game's own servers can break its login or updates, and nothing here hides the emulator
  from a game's anti-cheat; use it at your own risk.

**Apps:** the release carries **Aurora Store**, installed on first start: Google Play's catalogue,
signed in anonymously, without Google services. Apps that need Google services (Google sign-in, some
games' logins and purchases) do not work, because the image has none (see "What is missing"). APKs,
split sets and `.apks`/`.xapk` files can also be dropped on the window.

**The window:** Android's own Back, Home and Recents are at the bottom of the screen (or the
gesture bar: swipe up for Home, in from a side edge for Back), and also on the toolbar beside the
screen with Power, volume, turning and screenshots. The toolbar's panels do the rest: **Apps**
(install, open, force stop, clear data, uninstall, close background apps), **Location** (coordinates,
map links, saved places, GPX routes, walking), **Sensors** (tilt, shake), **Macros** (record, edit,
play), **Key map** (load, or place taps and sticks on the screen), **Capture** (screenshots and
recordings) and **Settings**. The screen starts upright like a phone; when a game turns it to
landscape, the window turns with it.

Closing the window shuts Android down properly first (about 3 seconds); closing it again cuts the
power at once.

## Status

What works today, on Apple Silicon Macs (macOS 15 or later):

- **Android boots to its home screen**, from Google's prebuilt AOSP virtual-device image and a kernel
  built by this repository, in about 20 seconds.
- **GPU acceleration**: Android draws with this Mac's GPU through gfxstream and KosmicKrisp (Mesa's
  Vulkan driver for Apple GPUs), with MoltenVK as the fallback, all shipped in the release.
  KosmicKrisp needs macOS 26; on macOS 15 the release falls back to MoltenVK (from release 0.4.0 on,
  whose Vulkan loader is built for macOS 15). Where neither starts, the guest's CPU draws: slow, fine
  for looking around but not for games.
- The launcher and the window's controls described above.
- Networking without any host driver or admin rights: DHCP, DNS through the host's resolver, TCP and UDP.
- A sinkhole that blocks domains you list (trackers, ads) at the DNS and TLS server-name level.
- `adb` over a loopback port.
- A control socket: everything the window's controls do, from scripts (`proteus ctl`).
- Key maps for playing with keyboard and mouse, including mouse-wheel pinch zoom.
- A host gamepad shows up in Android as a gamepad.
- Sound, played through this machine's default output.
- Several instances at once, with one able to drive the others.
- Plain Linux kernels boot too, to a shell on the serial console.

Not there yet: a Linux desktop check and an Android image of this project's own
(stripped, with sign-in). See "What is missing" at the end.

## Run Android from the command line

You need three things: the `proteus` program, a guest kernel, and an Android image (see "Start here").
The release's `proteus-<version>-macos-arm64.tar.gz` holds `proteus` with everything it loads; keep its folder
together. To build it yourself, see "Build" below; the kernel is built by the "Guest kernel" workflow
(see `guest/kernel/README.md`).

```sh
proteus                                          # the launcher
proteus --android <image directory> --gpu        # one machine, with the kernel beside proteus
proteus --kernel kernel-arm64 --android <image directory> --gpu
```

Without an instance, the first start creates `proteus-data/` inside the image directory for its storage.
`adb connect 127.0.0.1:5555` works once Android is up.

Window shortcuts (⌘ on a Mac, Ctrl+Alt on Linux):

| Keys | Does |
|---|---|
| ⌘⌫ or ⌘B | Back |
| ⌘⇧H | Home |
| ⌘O | Recent apps (swipe a card away there to close that app) |
| ⌘P | Power button: screen off and on |
| ⌘↑ / ⌘↓ | Volume up / down |
| ⌘← / ⌘→ | Turn the device a quarter turn (Android follows when auto-rotate is on) |
| ⌘S | Screenshot, saved on the Desktop |

Useful options (`proteus --help` lists all of them):

| Option | Effect |
|---|---|
| `--gpu` | Draw with this Mac's GPU; falls back to the CPU and says so if it cannot start |
| `--ram-mib <n>`, `--vcpus <n>` | Guest memory (default 4096) and CPUs (default 4) |
| `--display <WxH>` | Size of the guest's screen, held upright (default 720x1280; landscape apps turn the window) |
| `--refresh-hz <n>` | Frames per second the screen runs at: 60 (default), 90, 120 or 144 |
| `--mute`, `--no-sound` | Keep the sound card silent, or give the guest none |
| `--headless` | No window; use the control socket or `--frame-dump` to see the screen |
| `--ctl <socket>` | Listen for `proteus ctl` commands on this local socket |
| `--logcat <file>` | Save Android's system log |
| `--serial` | A root-capable shell and the kernel log on this terminal |
| `--data-dir <dir>` | Where this instance keeps its data |

### Keeping your settings

Options given once to `proteus instance create` are kept, so one short command starts it every time:

```sh
proteus instance create android --kernel kernel-arm64 --android <image directory> --gpu --ram-mib 4096 --vcpus 4
proteus instance start android
```

The instance gets its own data, control socket (`proteus ctl android ...`) and adb port. See "Several
instances" below.

### Driving a running instance

Start `proteus` with `--ctl /tmp/proteus.sock` (keep the path short; the system limits it to about 100
characters), then:

```sh
proteus ctl /tmp/proteus.sock tap 360 640                  # pixels of the screen as shown, upright
proteus ctl /tmp/proteus.sock swipe 360 1100 360 300
proteus ctl /tmp/proteus.sock key 116                  # Linux key code; 116 is the power button
proteus ctl /tmp/proteus.sock screenshot shot.png
proteus ctl /tmp/proteus.sock screenrecord clip.mp4 30  # seconds, at most 180; no sound
proteus ctl /tmp/proteus.sock install game.xapk        # an APK, a folder of split APKs, or .apks/.xapk
proteus ctl /tmp/proteus.sock location 3.139 101.6869  # latitude, longitude
proteus ctl /tmp/proteus.sock location "https://www.google.com/maps/@48.8584,2.2945,15z"  # or a geo: link
proteus ctl /tmp/proteus.sock route walk.gpx 5         # follow a GPX track at 5 km/h
proteus ctl /tmp/proteus.sock walk 90 5                # walk east at 5 km/h; `walk stop` stops
proteus ctl /tmp/proteus.sock rotate -90 0 0           # degrees about x, y, z; this lays the device flat
proteus ctl /tmp/proteus.sock shake
proteus ctl /tmp/proteus.sock shell am force-stop com.example.game   # any command in Android's shell
proteus ctl /tmp/proteus.sock pad a down               # gamepad: buttons, sticks lx ly rx ry, triggers lt rt
proteus ctl /tmp/proteus.sock window-screenshot win.png    # the window, toolbar and panels included
proteus ctl /tmp/proteus.sock window-click 745 584         # click the window, as the mouse would
proteus ctl /tmp/proteus.sock window-scroll 860 700 -10    # turn the mouse wheel there
proteus ctl /tmp/proteus.sock window-resize 1280 720       # resize the window; Android takes its shape
proteus ctl /tmp/proteus.sock shutdown                     # shut Android down, then switch off
proteus ctl /tmp/proteus.sock stop                         # switch off at once, like pulling the plug
```

The socket is created for your user only. Anyone who can connect to it controls the guest.

### Playing with keyboard and mouse

A key map says which keys and mouse movements stand for which touches: taps, a virtual joystick on
four keys, and a "look" mode in which the mouse drags the view. `assets/keymaps/example.toml` explains
the format; copy it, put your game's button positions in, and start with `--keymap <file>`. Keys the
map does not use still reach Android as a keyboard.

To have the map follow the app in front, put maps in a folder named after the apps' package names
(`com.example.game.toml`), optionally with a `default.toml` for every other app, and start with
`--keymap-dir <folder>`. The emulator checks which app is in front once a second.

### Several instances

```sh
proteus instance create farm --kernel kernel-arm64 --android <image directory> --ram-mib 2048
proteus instance start farm                      # its own data, control socket and adb port
proteus instance list
proteus ctl farm tap 360 640                     # an instance name works where a socket path does
proteus instance rm farm                         # asks first; deletes its Android data for good
```

Instances live in `~/Library/Application Support/proteus/instances` (set `PROTEUS_HOME` to move them). They
share the Android image, read-only. Each takes the next free adb port from 5555 up; `proteus` says which.
Memory is per instance.

To play several at once, start one with `--sync <instance or socket>` (repeatable): its touches,
keys and gamepad moves are sent to the others as well. Give them all the same display size.

### Gamepads

A gamepad plugged into this machine when `proteus` starts becomes a gamepad in Android. Devices are fixed
when the guest boots, so a pad plugged in later needs a restart, or start with `--gamepad` to always
have one (`--no-gamepad` to never). Every host pad drives the same guest pad.

## Build

Needs Rust (the version in `rust-toolchain.toml`).

```sh
cargo run --release -p vmm -- --kernel <Image> --initrd <ramdisk> --serial
```

`cargo run` and `cargo test` sign the binary automatically. To sign a binary you built with `cargo build`:

```sh
codesign --sign - --force --entitlements packaging/macos/hv.entitlements target/release/proteus
```

macOS refuses hypervisor access to unsigned binaries.

With GPU acceleration, as the release is built (needs `cmake`, `ninja` and Homebrew's `molten-vk`
and `vulkan-loader`):

```sh
scripts/build-gfxstream.sh <work folder> <gfxstream folder>
GFXSTREAM_PATH=<gfxstream folder> cargo build --release -p vmm --features gpu
scripts/bundle-macos.sh target/release/proteus <gfxstream folder> <out folder>
<out folder>/proteus --kernel ... --gpu
```

The out folder holds `proteus`, gfxstream's renderer, the Vulkan loader and MoltenVK, and their
licences, and runs as it is. `docs/gfxstream.md` has the details and the other driver, KosmicKrisp.

On Windows, from Git Bash, with Visual Studio 2022 and its C++ Clang tools, and `cmake` and `ninja`
on the path; the GPU's own Vulkan driver is used:

```sh
scripts/build-gfxstream.sh <work folder> <gfxstream folder>
GFXSTREAM_PATH=<work folder>/build/host cargo build --release -p vmm --features gpu
cp <gfxstream folder>/gfxstream_backend.dll target/release/
target/release/proteus.exe --kernel kernel-x86_64 --android <image directory> --gpu
```

A plain Linux kernel must be an uncompressed ARM64 `Image`. To use the serial console it needs the 8250
driver; for disks it needs virtio-mmio and virtio-blk built in.

## Network

The guest gets `10.0.2.15`, with gateway and DNS at `10.0.2.2`. IPv4 only.

| Option | Effect |
|---|---|
| `--blocklist <file>` | Block every domain in the file, and its subdomains. Plain domain lists, hosts files and `||domain^` rules are understood. Repeatable. |
| `--allowlist <file>` | Domains that stay reachable even if a blocklist names them. Repeatable. |
| `--adb-port <n>` | Loopback port for `adb connect 127.0.0.1:<n>`. Default 5555; 0 turns it off. |
| `--allow-quic` | Let QUIC through. By default UDP 443 is dropped so apps fall back to TLS, where the server name can be checked. |
| `--host-loopback` | Let the guest reach this machine's localhost at `10.0.2.2`. Off by default, because every app in the guest could then reach services meant only for this machine. |
| `--no-net` | No network device at all. |

No blocklist is bundled; pass the lists you want. What the sinkhole cannot do: it cannot see inside encrypted
traffic, and it cannot block tracking or ads that a game serves from the same host as its own content.
DNS over TLS is always dropped so that lookups cannot bypass it.

## Tests

```sh
cargo test --workspace
```

Tests under `crates/hv/tests` run small guests on the real hypervisor. Set `PROTEUS_SKIP_HV_TESTS=1` on a
machine that has none, such as a hosted CI runner.

`crates/vmm/tests/linux_boot.rs` boots real kernels when these point at files, and reports itself skipped
otherwise. Its network test binds loopback ports on the host.

| Variable | What it needs |
|---|---|
| `PROTEUS_TEST_INITRD` | A ramdisk whose init mounts `/proc` and `/sys` and ends in a BusyBox shell on the console |
| `PROTEUS_TEST_KERNEL` | An ARM64 `Image` with virtio-mmio, 8250 and PL031 built in |
| `PROTEUS_TEST_BOOTCONFIG_KERNEL` | An ARM64 `Image` built with `CONFIG_BOOT_CONFIG`, such as a stock Android GKI kernel |

## Privacy

The emulator sends nothing anywhere on its own behalf: no analytics, no crash upload, no update check.
The launcher's **Update Proteus** button (Windows and Linux) fetches the newest release only when pressed,
through the GitHub CLI (`gh`, logged in with `gh auth login`), and replaces the program and the
kernel beside it; restart Proteus afterwards.
`deny.toml` bans analytics, crash-upload and HTTP client crates from the dependency tree, and CI enforces it.

## Layout

| Path | Contents |
|---|---|
| `crates/hv` | Hypervisor traits, and the Hypervisor.framework and KVM backends |
| `crates/arch` | Board address map, device tree, kernel loading, PSCI, bootconfig |
| `crates/devices` | MMIO bus, UART, RTC, virtio-mmio transport, virtio block, network, display, input, console and sound devices |
| `crates/net` | Userspace NAT, DHCP, DNS, sinkhole policy, adb forward |
| `crates/vmm` | The `proteus` binary: board wiring, Android image assembly, window, control socket |
| `guest/kernel` | Configuration of the guest kernel |

## What is missing

Honest list, most important first:

- **Google services.** Google's test image has no Play Services, so apps that sign in with Google
  or buy through Play do not work, and some games show "Google Play services are not supported"
  (their e-mail or other logins still work); Aurora Store installs the rest. The planned image with
  microG or user-supplied Google apps needs an Android build machine this project does not have.
  Installing Google's apps into the running image is not done: a first try found no system
  partition the image would remount.
- **GPU acceleration is new.** It has not been measured against Google's emulator yet, and frames
  are read back once on their way to the window (about 1.5 ms each for a 1080x2400 frame, measured
  in Android's own apps, not under a game's load). Under MoltenVK an app's drawing could stall (Aurora
  Store froze on its first tap), which is why KosmicKrisp is tried first; on Macs where KosmicKrisp
  does not load (older than macOS 26), MoltenVK is what runs.
- **No Bluetooth or phone network.** The board has neither, so Bluetooth is switched off at boot.
- **More game controls.** Key maps are written by hand.
- **Not yet tried for real:** a physical gamepad plugged in at start (the guest pad was checked
  with `--gamepad` and the `pad` command), dropping a file on the window (the same install was
  checked through `proteus ctl ... install`), and a split set of several APKs of one app (one-APK
  sessions and refused mixed sets were checked). All host gamepads drive the guest's one gamepad, so two-player games on one
  machine do not work.
- **An image of this project's own.** Building Android needs a large Linux machine this project does
  not have, so it runs Google's unmodified test image: no Google sign-in, no app store, and none of
  the planned stripping. Apps are installed from files (drop them on the window, or `proteus ctl ... install`).
- **Linux and Windows hosts.** On x86_64 Linux, `proteus` runs on KVM: Google's x86_64 Cuttlefish image
  with this repository's kernel boots to Android with adb in about a minute, headless, in CI
  (`scripts/fetch-cuttlefish.sh aosp_cf_x86_64_only_phone-userdebug <dir>` fetches the image). The
  window (X11 or Wayland), sound (ALSA) and gamepads are built there too but have not been tried
  on a Linux desktop; building needs `libasound2-dev` and `libudev-dev`. Plain Linux test kernels
  must find their hardware through the MP table and `virtio_mmio.device=` on the command line; the
  board has no ACPI.
- **Windows hosts (preview).** On Windows x86_64, `proteus.exe` runs on the Windows Hypervisor
  Platform (`crates/hv/src/whp.rs`) with the same x86_64 Cuttlefish image and kernel as Linux.
  Checked on a Ryzen 7 9700X with an RTX 5070: Android boots to the home screen in about a
  minute, adb answers on 127.0.0.1:5555, and with `--gpu` it draws through gfxstream on the GPU's
  Vulkan driver (SurfaceFlinger reports ANGLE on "Virtio-GPU GFXStream (NVIDIA GeForce RTX
  5070)"). An idle home screen costs about a sixth of one host core. It needs the
  HypervisorPlatform feature on (`Enable-WindowsOptionalFeature -Online -FeatureName
  HypervisorPlatform` and `bcdedit /set hypervisorlaunchtype auto`, then a restart). Sound,
  gamepads and a long session have not been tried there yet.

Things to know: closing the window does not shut Android down cleanly. A ping to any address is
answered by the emulator itself, so use a real connection to test the network. Games that detect
emulators may refuse to run, and nothing here hides the emulator from them.
