# gfxstream on macOS: build spike

GPU acceleration is planned as gfxstream behind virtio-gpu, reached through `rutabaga_gfx`. This
note records what was checked before any of it is wired in.

## Result (2026-11-19, Apple Silicon, macOS 26.5)

- gfxstream's host renderer builds on macOS with `scripts/build-gfxstream.sh`, unchanged, from
  `github.com/google/gfxstream` at `cf47bd2660e7217f5729c91664d36d81c1b3282a` (2026-11-13). That
  repository carries its third-party code, so it is the only checkout.
- The Android source mirror (`android.googlesource.com/platform/hardware/google/gfxstream`)
  stopped at 2025-03-26 and is not the live source. It also builds, with `aemu`, `flatbuffers`,
  `gfxstream-protocols` and the ANGLE and libdrm headers beside it and one deprecation warning
  allowed, but is not used.
- The library is `libgfxstream_backend.dylib`: 8.6 MB as built, 6.6 MB stripped. With `emu` at
  1.7 MB that is about 8.5 MB together, inside the 30 MB budget.
- It exports every one of the 29 `stream_renderer_*` functions `rutabaga_gfx` 0.1.85 declares
  (compared by name: `nm` against the crate's `extern` block), and links only system frameworks
  and libc++.

## How rutabaga_gfx finds it

`rutabaga_gfx` (crates.io, BSD-3-Clause) with its `gfxstream` feature links `gfxstream_backend`
from `GFXSTREAM_PATH`, or through pkg-config, and links libc++ on macOS. Nothing else is built by
its build script. Its dependencies (`cfg-if`, `libc`, `log`, `magma-gpu`, `remain`, `serde`,
`serde_json`, `thiserror`, `zerocopy`, optional `vulkano`) include nothing `deny.toml` bans; the
full tree has not been run through `cargo deny` yet.

## The two Vulkan drivers (2026-11-19, Apple M4 Max, macOS 26.5)

Both from Homebrew: `molten-vk` 1.4.2 and `mesa` 26.3.0, whose macOS 26 build includes
KosmicKrisp. A small C program that calls `stream_renderer_init` with Vulkan only and then reads
the Vulkan capset (`virtio-gpu` capset 3) was run against each.

| | MoltenVK 1.4.2 | KosmicKrisp (Mesa 26.3.0) |
|---|---|---|
| `vulkaninfo` API version | 1.4.357 | 1.4.363 |
| Conformance version reported | 1.4.4.0 | 1.4.6.2 |
| Device extensions | 153 | 165 |
| `VK_EXT_external_memory_metal` | yes | yes |
| `VK_KHR_portability_subset` | yes (a portability driver) | no |
| `stream_renderer_init` | succeeds | succeeds |
| Device gfxstream picks, and API version it uses | Apple M4 Max, 1.1.357 | Apple M4 Max, 1.4.363 |
| External memory mode gfxstream picks | Metal | Metal |
| Vulkan capset | 116 bytes, blob alignment 16 KiB | the same |

How each is selected matters for packaging:
- gfxstream has a dedicated MoltenVK path (`ANDROID_EMU_VK_ICD=moltenvk`). It then ignores
  `VK_DRIVER_FILES` and loads `lib64/vulkan/MoltenVK_icd.json` from beside the program, and sets
  MoltenVK's configuration variables itself. Shipping MoltenVK therefore means shipping that file
  and `libMoltenVK.dylib` in that layout.
- KosmicKrisp goes through gfxstream's generic path: `ANDROID_EMU_VK_ICD` unset, and the Vulkan
  loader finds the driver through `VK_DRIVER_FILES`. Both need the Vulkan loader
  (`libvulkan.dylib`) as well.
- The 16 KiB blob alignment matches the host page size, as blueprint section 7.1 wants.

Nothing has been drawn yet: the comparison the blueprint asks for (a Vulkan and a GLES test app,
benchmarks against Google's emulator) needs gfxstream wired into virtio-gpu first.

## Not checked yet

- Rendering, on either driver.
- That the function signatures, not only the names, match `rutabaga_gfx`'s declarations.
- Whether the Cuttlefish image's guest side (its gfxstream guest drivers) talks to this build.
- Windows and Linux builds.
