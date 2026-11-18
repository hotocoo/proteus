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

## Not checked yet

- Rendering. gfxstream loads a Vulkan driver at run time, and this Mac has none installed.
  MoltenVK and KosmicKrisp are the candidates (blueprint section 2.1); neither has been tried.
- That the function signatures, not only the names, match `rutabaga_gfx`'s declarations.
- Whether the Cuttlefish image's guest side (its gfxstream guest drivers) talks to this build.
- Windows and Linux builds.
