# emu

A small virtual machine monitor in Rust, being built into a lightweight Android emulator for games.
No QEMU, PCI, UEFI or ACPI: the guest sees a virtio-mmio board and is booted directly from a kernel image.

## Status

Early. What works today, on Apple Silicon Macs (macOS 15 or later):

- Boots Linux kernels directly, including a stock Android GKI 6.12 kernel, to a shell on a serial console.
- Several virtual CPUs, interrupt controller, timer, real-time clock.
- virtio-blk disks, read-write or read-only.
- Networking without any host driver or admin rights: DHCP, DNS through the host's resolver, TCP and UDP.
- A sinkhole that blocks domains you list (trackers, ads) at the DNS and TLS server-name level.
- `adb` forwarding on a loopback port.
- Android bootconfig parameters (`androidboot.*`).

Not there yet: Android itself, graphics, sound, input, Linux and Windows hosts.

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
| `crates/devices` | MMIO bus, UART, RTC, virtio-mmio transport, virtio-blk, virtio-net |
| `crates/net` | Userspace NAT, DHCP, DNS, sinkhole policy, adb forward |
| `crates/vmm` | The `emu` binary |
