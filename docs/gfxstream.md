# gfxstream on macOS: build spike

GPU acceleration is planned as gfxstream behind virtio-gpu, reached through `rutabaga_gfx`. This
note records what was checked before any of it is wired in.

## Result (2026-11-18, Apple Silicon, macOS 26.5)

- gfxstream's host renderer builds on macOS with `scripts/build-gfxstream.sh`, from
  `platform/hardware/google/gfxstream` at `d047a57228332d995d36600792fa9ccc26cf8ae6` and
  `platform/hardware/google/aemu` at `3c1ced8a369417db591eb7cd083af5bb2c317975`.
- The library is `libgfxstream_backend.dylib`: 8.1 MB as built, 6.1 MB stripped. With `emu` at
  1.7 MB that is about 8 MB together, well inside the 30 MB budget.
- It exports the `stream_renderer_*` interface (33 functions) that `rutabaga_gfx` calls, and links
  only system frameworks and libc++.
- One source change was needed: the macOS window code calls an OpenGL method deprecated since
  macOS 10.14, which the build treats as an error. The script turns that one warning back into a
  warning.

## How rutabaga_gfx finds it

`rutabaga_gfx` (crates.io, BSD-3-Clause) with its `gfxstream` feature links `gfxstream_backend`
from `GFXSTREAM_PATH`, or through pkg-config, and links libc++ on macOS. Nothing else is built by
its build script.

## Not checked yet

- Rendering. gfxstream loads a Vulkan driver at run time, and this Mac has none installed.
  MoltenVK and KosmicKrisp are the candidates (blueprint section 2.1); neither has been tried.
- Whether the Cuttlefish image's guest side (its gfxstream guest drivers) talks to this build.
- `rutabaga_gfx`'s dependency tree against `deny.toml`.
- Windows and Linux builds.
