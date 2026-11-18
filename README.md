# emu

A small virtual machine monitor in Rust, being built into a lightweight Android emulator for games.
No QEMU, PCI, UEFI or ACPI: the guest sees a virtio-mmio board and is booted directly from a kernel image.

## Status

Early. What works today, on Apple Silicon Macs (macOS 15 or later):

- Boots Linux kernels directly, including a stock Android GKI 6.12 kernel, to a shell on a serial console.
- Several virtual CPUs, interrupt controller, timer, real-time clock.
- virtio-blk disks, read-write or read-only.
- Android bootconfig parameters (`androidboot.*`).

Not there yet: Android itself, graphics, sound, input, networking, Linux and Windows hosts.

## Build and run

Needs Rust (the version in `rust-toolchain.toml`).

```sh
cargo run --release -p vmm -- --kernel <Image> --initrd <ramdisk> --serial
```

`cargo run` and `cargo test` sign the binary automatically. To sign a binary you built with `cargo build`:

```sh
codesign --sign - --force --entitlements packaging/macos/hv.entitlements target/release/emu
```

macOS refuses hypervisor access to unsigned binaries. `emu --help` lists every option.

The kernel must be an uncompressed ARM64 `Image`. To use the serial console it needs the 8250 driver;
for disks it needs virtio-mmio and virtio-blk built in.

## Tests

```sh
cargo test --workspace
```

Tests under `crates/hv/tests` run small guests on the real hypervisor. Set `EMU_SKIP_HV_TESTS=1` on a
machine that has none, such as a hosted CI runner.

`crates/vmm/tests/linux_boot.rs` boots real kernels when these point at files, and reports itself skipped
otherwise:

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
| `crates/devices` | MMIO bus, UART, RTC, virtio-mmio transport, virtio-blk |
| `crates/vmm` | The `emu` binary |
