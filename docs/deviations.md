# Where the code differs from the blueprint

The blueprint (approved 2026-11-18, revision 2) is kept outside the repository. These are the places
where what was built differs from it, and why. Each is either temporary or a decision the project
owner may want to revisit.

## Layout

- **No `gfx`, `ui`, `ctl` or `xtask` crates.** The control socket, key maps, macros, window, sound,
  gamepads, instances and the adb client live in `crates/vmm`. They are small enough not to need
  their own crates yet; `gfx` arrives with GPU acceleration.
- **Configuration is command-line flags**, not `assets/default.toml`. Timings and sizes live in
  `crates/vmm/src/config.rs` and the boards' `layout.rs` files.

## Guest

- **Google's prebuilt Cuttlefish images, not the stripped `emu_*` products.** Building AOSP needs a
  large Linux machine the project does not have. So there is no strip manifest, no sign-in flavours,
  no EROFS images, and the idle-RAM and idle-flow gates cannot be measured against the base image.
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
- **The guest cannot switch the machine off.** Without ACPI, `poweroff` halts the CPUs and `emu`
  keeps running; `reboot` reaches the keyboard controller's reset and `emu` boots again. Mapping
  Android's power-off onto something `emu` sees is the blueprint's open milestone-2 item.
- **The kernel is entered through the 64-bit boot protocol with `boot_params`**, as planned; the MP
  table also fills in the BIOS data area words (0x40e, 0x413), which is where Linux looks first.

## Not started, and why

- **Windows (WHPX).** No Windows machine, and `virtio-queue` enables a `vm-memory` feature that does
  not compile for Windows, so the device crates do not build there yet.
- **GPU acceleration.** gfxstream's host renderer builds on macOS (`docs/gfxstream.md`), but it
  needs a Vulkan driver at run time and none is installed. How one gets onto a user's Mac, and
  whether a release bundles it, is the project owner's decision.
- **The GUI (milestone 6)** depends on the milestone-3 overlay check, which needs the same Vulkan
  driver.
