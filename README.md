# emu

A small virtual machine monitor in Rust, being built into a lightweight Android emulator for games.
No QEMU, PCI, UEFI or ACPI: the guest sees a virtio-mmio board and is booted directly from a kernel image.

## Status

Early. What works today, on Apple Silicon Macs (macOS 15 or later):

- **Android boots to its home screen**, from Google's prebuilt AOSP virtual-device image and a kernel
  built by this repository, in about 20 seconds. Graphics are drawn by the guest's CPU for now, so it
  is slow: fine for looking around, not yet for games.
- A window that shows the guest's display, with the mouse as a finger and the keyboard as a keyboard.
- Networking without any host driver or admin rights: DHCP, DNS through the host's resolver, TCP and UDP.
- A sinkhole that blocks domains you list (trackers, ads) at the DNS and TLS server-name level.
- `adb` over a loopback port.
- A control socket: taps, swipes, keys, screenshots, a location, device rotation and shaking.
- Several virtual CPUs, disks, a sound card (silent on the host so far), a serial console.
- Plain Linux kernels boot too, to a shell on the serial console.

Not there yet: GPU acceleration, audible sound, key mapping for games, macros, several instances,
Linux and Windows hosts, and an Android image of this project's own (stripped, with sign-in). See
"What is missing" at the end.

## Run Android

You need three things: the `emu` program, a guest kernel, and an Android image.

1. **`emu` and the kernel** are in each release: `emu` and `kernel-arm64`. To build `emu` yourself, see
   "Build" below; the kernel is built by the "Guest kernel" workflow (see `guest/kernel/README.md`).
2. **The Android image** is Google's: open
   <https://ci.android.com/builds/branches/aosp-android-latest-release/grid>, pick a green build of
   `aosp_cf_arm64_only_phone-userdebug`, and download `aosp_cf_arm64_only_phone-img-<build>.zip` from
   its artifacts (about 1.1 GB). Unzip it into a directory. This project does not redistribute it.
3. Start it:

```sh
emu --kernel kernel-arm64 --android <image directory>
```

The first start creates `emu-data/` inside the image directory for this instance's own storage.
Swipe up to unlock. `adb connect 127.0.0.1:5555` works once Android is up. Close the window to switch
the machine off; that is like pulling the plug, so shut Android down from inside when you can
(`adb shell reboot -p`).

Useful options (`emu --help` lists all of them):

| Option | Effect |
|---|---|
| `--ram-mib <n>`, `--vcpus <n>` | Guest memory (default 4096) and CPUs (default 4) |
| `--display <WxH>` | Size of the guest's screen (default 1280x720) |
| `--headless` | No window; use the control socket or `--frame-dump` to see the screen |
| `--ctl <socket>` | Listen for `emu ctl` commands on this local socket |
| `--logcat <file>` | Save Android's system log |
| `--serial` | A root-capable shell and the kernel log on this terminal |
| `--data-dir <dir>` | Where this instance keeps its data |

### Driving a running instance

Start `emu` with `--ctl /tmp/emu.sock` (keep the path short; the system limits it to about 100
characters), then:

```sh
emu ctl /tmp/emu.sock tap 640 360
emu ctl /tmp/emu.sock swipe 640 650 640 150
emu ctl /tmp/emu.sock key 116                  # Linux key code; 116 is the power button
emu ctl /tmp/emu.sock screenshot shot.png
emu ctl /tmp/emu.sock location 3.139 101.6869  # latitude, longitude
emu ctl /tmp/emu.sock rotate -90 0 0           # degrees about x, y, z; this lays the device flat
emu ctl /tmp/emu.sock shake
emu ctl /tmp/emu.sock stop
```

The socket is created for your user only. Anyone who can connect to it controls the guest.

## Build

Needs Rust (the version in `rust-toolchain.toml`).

```sh
cargo run --release -p vmm -- --kernel <Image> --initrd <ramdisk> --serial
```

`cargo run` and `cargo test` sign the binary automatically. To sign a binary you built with `cargo build`:

```sh
codesign --sign - --force --entitlements packaging/macos/hv.entitlements target/release/emu
```

macOS refuses hypervisor access to unsigned binaries.

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

Tests under `crates/hv/tests` run small guests on the real hypervisor. Set `EMU_SKIP_HV_TESTS=1` on a
machine that has none, such as a hosted CI runner.

`crates/vmm/tests/linux_boot.rs` boots real kernels when these point at files, and reports itself skipped
otherwise. Its network test binds loopback ports on the host.

| Variable | What it needs |
|---|---|
| `EMU_TEST_INITRD` | A ramdisk whose init mounts `/proc` and `/sys` and ends in a BusyBox shell on the console |
| `EMU_TEST_KERNEL` | An ARM64 `Image` with virtio-mmio, 8250 and PL031 built in |
| `EMU_TEST_BOOTCONFIG_KERNEL` | An ARM64 `Image` built with `CONFIG_BOOT_CONFIG`, such as a stock Android GKI kernel |

## Privacy

The emulator sends nothing anywhere on its own behalf: no analytics, no crash upload, no update check.
`deny.toml` bans analytics, crash-upload and HTTP client crates from the dependency tree, and CI enforces it.

## Layout

| Path | Contents |
|---|---|
| `crates/hv` | Hypervisor traits and the Hypervisor.framework backend |
| `crates/arch` | Board address map, device tree, kernel loading, PSCI, bootconfig |
| `crates/devices` | MMIO bus, UART, RTC, virtio-mmio transport, virtio block, network, display, input, console and sound devices |
| `crates/net` | Userspace NAT, DHCP, DNS, sinkhole policy, adb forward |
| `crates/vmm` | The `emu` binary: board wiring, Android image assembly, window, control socket |
| `guest/kernel` | Configuration of the guest kernel |

## What is missing

Honest list, most important first:

- **GPU acceleration.** Android draws with the guest's CPU. Games need the host's GPU; that work
  (gfxstream over virtio-gpu) has not started.
- **Sound you can hear.** The guest has a sound card; the host plays nothing yet.
- **Game controls.** Keys reach Android as a keyboard. Mapping keys and mouse to touch controls,
  gamepads and macros are not built.
- **An image of this project's own.** Building Android needs a large Linux machine this project does
  not have, so it runs Google's unmodified test image: no Google sign-in, no app store, and none of
  the planned stripping. Apps are installed with `adb install`.
- **Linux and Windows hosts.** Only the macOS backend exists.
- **Several instances**, an app installer, screen recording and a settings window.

Things to know: closing the window does not shut Android down cleanly. A ping to any address is
answered by the emulator itself, so use a real connection to test the network. Games that detect
emulators may refuse to run, and nothing here hides the emulator from them.
