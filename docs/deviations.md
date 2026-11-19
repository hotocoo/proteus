# Where the code differs from the blueprint

The blueprint (approved 2026-11-18, revision 2) is kept outside the repository. These are the places
where what was built differs from it, and why. Each is either temporary or a decision the project
owner may want to revisit.

## Layout

- **No `ui`, `ctl` or `xtask` crates.** The control socket, key maps, macros, window, GUI, sound,
  gamepads, instances and the adb client live in `crates/vmm`. `gfx` exists and wraps gfxstream.
- **Configuration is command-line flags**, not `assets/default.toml`. An instance keeps its flags in
  its `args` file, which the settings form reads and writes. Timings, sizes and colours live in
  `crates/vmm/src/config.rs` and the boards' `layout.rs` files.

## GUI

- **egui is drawn with wgpu (Metal on macOS) in the emulator's own window**, not with `ash` in a
  transparent overlay window above a gfxstream swapchain. gfxstream does not present to the window
  here: the guest's frames are read back and uploaded to a texture (one copy per frame), so there is
  no swapchain to keep untouched, and one window is simpler. The overlay design comes back with the
  blueprint's zero-copy present path, which is not built.
- **The launcher** (`proteus` with no arguments, or Proteus.app) is the instance manager the blueprint puts in
  the overlay. Settings are a form over an instance's flags, applied on restart.
- **Macros are the control socket's commands, one per line** (with `wait`), not TOML. Repeating is
  `run <file> <times>`.
- **Closing the window shuts Android down** (`reboot -p` over adb, then power off), where the blueprint
  only said closing switches the machine off.

## Guest

- **Google's prebuilt Cuttlefish images, not the stripped `emu_*` products.** Building AOSP needs a
  large Linux machine the project does not have. So there is no strip manifest, no sign-in flavours,
  no EROFS images, and the idle-RAM and idle-flow gates cannot be measured against the base image.
- **Aurora Store is bundled** (GPL-3.0, F-Droid's build, pinned by SHA-256) and installed on a data
  folder's first boot, so Play's catalogue is reachable without Google's apps. The blueprint's
  sign-in flavours (microG, user-supplied Google apps) need the project's own image.
- **After boot the emulator adjusts the stock image over adb**: it stops the serial copy of logcat
  unless `--logcat` asks for it, and switches Bluetooth off (no controller; the stack aborts and
  restarts). In the project's own image these belong in its build.
- **Android's own boot images are read** (`init_boot.img`, `vendor_boot.img`, `super.img` behind a
  GPT disk assembled in memory), instead of loose files concatenated by `xtask`.
- **adb without a key.** The images' `adbd` does not ask for one (`ro.adb.secure` is unset), so the
  in-tree client sends no `AUTH`; the project key from blueprint section 6 belongs with the
  project's own image.

## x86_64 board

- **`Vcpu` has `system_regs`/`set_system_regs`** on x86_64 (control registers, segments,
  descriptor tables). The blueprint's trait has only `regs`; entering a kernel in long mode needs
  both.
- **bzImage loading** is there besides the blueprint's ELF `vmlinux`, but has only been checked
  against a synthetic header, not a real bzImage.
- **The guest cannot switch the machine off.** Without ACPI, `poweroff` halts the CPUs and `proteus`
  keeps running; `reboot` reaches the keyboard controller's reset and `proteus` boots again. Mapping
  Android's power-off onto something `proteus` sees is the blueprint's open milestone-2 item.
- **The kernel is entered through the 64-bit boot protocol with `boot_params`**, as planned; the MP
  table also fills in the BIOS data area words (0x40e, 0x413), which is where Linux looks first.

## Not started, and why

- **Windows (WHPX) is built but has not booted a guest.** The backend shows the guest Hyper-V's
  enlightenments (reference TSC, frequency MSRs) where KVM gives kvmclock, and emulates the IOAPIC
  in the VMM, edge-triggered only. `third_party/virtio-queue` is a copy of the crate with
  vm-memory's default `rawfd` feature dropped, which does not build for Windows; Unix hosts turn it
  back on. On Windows, disk and kernel reads go through a buffer instead of straight into guest
  memory.
- **The stripped image, both sign-in flavours, and ARM translation on x86_64 hosts.** All need AOSP
  builds, so a Linux build machine.
- **The zero-copy present path** (gfxstream drawing straight to the window's swapchain), and the
  benchmarks against Google's emulator.
