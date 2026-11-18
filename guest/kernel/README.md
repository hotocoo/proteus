# Guest kernel

The emulator boots an Android common kernel built from Google's unmodified source
(`https://android.googlesource.com/kernel/common`, branch `android16-6.12`) with the configuration
`gki_defconfig` plus the fragments in this directory. It is not a certified GKI binary: stock GKI has
no virtio-mmio driver, which is the only bus this board has.

| File | Purpose |
|---|---|
| `emu.fragment` | Options for every architecture |
| `emu-arm64.fragment`, `emu-x86_64.fragment` | Per-architecture options |
| `required.config` | Options the build is checked for before it is accepted |

The "Guest kernel" workflow builds both architectures on GitHub's runners and attaches the images, the
final `.config` and the exact source commit as artifacts. The kernel is licensed under GPL-2.0; the
source for a given image is that commit of the repository above, configured with that `.config`.
