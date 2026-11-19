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
- The library is `libgfxstream_backend.dylib`: 8.6 MB as built, 6.6 MB stripped. With `proteus` at
  1.7 MB that is about 8.5 MB together, inside the 30 MB budget.
- It exports every one of the 29 `stream_renderer_*` functions `rutabaga_gfx` 0.1.85 declares
  (compared by name: `nm` against the crate's `extern` block), and links only system frameworks
  and libc++.

## How rutabaga_gfx finds it

`rutabaga_gfx` (crates.io, BSD-3-Clause) with its `gfxstream` feature links `gfxstream_backend`
from `GFXSTREAM_PATH`, or through pkg-config, and links libc++ on macOS. Nothing else is built by
its build script. A scratch crate with `rutabaga_gfx` 0.1.85 (`default-features = false`,
`features = ["gfxstream"]`) builds on macOS with `GFXSTREAM_PATH` set, links
`@rpath/libgfxstream_backend.dylib`, and from Rust gives the same result as the C probe below:
Vulkan capset 3, version 0, 116 bytes. Its 34-crate dependency tree passes this repository's
`cargo deny check bans sources`.

## Driver memory in the guest (zero copy)

The blueprint's blob path maps memory the Vulkan driver allocated straight into the guest. A
scratch program checked that on this Mac with each driver: it allocated 64 KiB of
`HOST_VISIBLE | HOST_COHERENT` memory, mapped it with `vkMapMemory`, passed the pointer to
`hv`'s `Vm::map` at the SHM window's address after the vCPU existed, ran a guest that stored a
value there, and read it back on the host.

| | MoltenVK | KosmicKrisp |
|---|---|---|
| Mapped pointer 16 KiB aligned | yes | yes |
| `Vm::map` (HVF) accepts it | yes | yes |
| Guest's store seen by the host | yes | yes |

## The guest's side

Google's Cuttlefish launcher (`github.com/google/android-cuttlefish`,
`base/cvd/cuttlefish/host/libs/vm_manager/crosvm_manager.cpp`) switches the image to gfxstream
with these bootconfig values, where the emulator sets the software ones today:

| Key | gfxstream (`gfxstream_guest_angle`) | Now (software) |
|---|---|---|
| `androidboot.hardware.vulkan` | `ranchu` | `pastel` |
| `androidboot.hardware.egl` | `angle` | `angle` |
| `androidboot.cpuvulkan.version` | `0` | `4202496` (1.2) |
| `androidboot.hardware.gltransport` | `virtio-gpu-asg` | not set |
| `androidboot.vendor.apex.com.google.cf.vulkan` | `com.google.cf.vulkan.gfxstream` | not set |

`gralloc` (`minigbm`), `hwcomposer`, its display finder and framebuffer format, and the OpenGL ES
version stay as they are. The image (build 16644040) has `com.google.cf.vulkan.apex` and the
gfxstream GLES encoders in `/vendor/lib64`.

The image's guest gfxstream and the host renderer built from GitHub's head are not a matched pair.
If their handshake fails, the host revision moves toward the image's build date.

## How the renderer behaves around failure

- When no Vulkan driver works, `stream_renderer_init` logs "Failed to initialize renderer" and
  then never returns. `proteus` cannot call it on its own thread and fall back; it has to find out in a
  separate process with a time limit first.
- `stream_renderer_teardown` followed by `stream_renderer_init` in the same process works (checked
  with KosmicKrisp), so a guest reboot can get a fresh renderer.

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

With `proteus --gpu` (built with `--features gpu`), Google's Cuttlefish image draws its lock and home
screens through gfxstream with either driver, and the screens read back correctly. The comparison
the blueprint asks for (a Vulkan and a GLES test app, benchmarks against Google's emulator, input
and frame latency) has not been run.

## Running it

A release carries two Vulkan drivers, in the layout gfxstream reads by itself, and `proteus --gpu` tries
them best first, each in a child process with a time limit:

```text
lib64/vulkan/libvulkan.dylib                    the Vulkan loader
lib64/vulkan/libkosmickrisp_icd.json            KosmicKrisp (Mesa), with the libraries it links
lib64/vulkan/libvulkan_kosmickrisp.dylib        (zstd, xcb, SPIRV-Tools…), pointed at each other
lib64/vulkan/MoltenVK_icd.json                  MoltenVK, the fallback
lib64/vulkan/libMoltenVK.dylib
licenses/                                       gfxstream and its third-party code, Mesa and the
                                                libraries above, MoltenVK, Vulkan-Loader
```

`lib64` sits beside `proteus`, or in `Contents/Resources` inside Proteus.app; `proteus` points gfxstream there
through `ANDROID_EMULATOR_LAUNCHER_DIR` and picks the driver with `ANDROID_EMU_VK_ICD`
(`kosmickrisp`, then `moltenvk`), unless the user set `ANDROID_EMU_VK_ICD`, `VK_DRIVER_FILES` or
`VK_ICD_FILENAMES`. `scripts/build-gfxstream.sh <work> <out>`, `scripts/bundle-macos.sh` (which uses
`scripts/bundle-dylib.py` for KosmicKrisp's libraries) and `scripts/make-app.sh` build all of it;
the release workflow runs the same scripts on a macOS 26 runner, where Homebrew's Mesa has
KosmicKrisp.

KosmicKrisp is preferred because under MoltenVK an app's render thread could wait for ever in
`vkQueueWaitIdle` (Aurora Store froze on its first tap); the same steps under KosmicKrisp did not.

Known messages that do not stop it: the guest's `DRM_IOCTL_VIRTGPU_GET_CAPS failed with Invalid
argument` and `CONTEXT_INIT failed with File exists`; the driver carries on.

## Not checked yet

- Performance, on either driver, and against the software path or Google's emulator.
- The window: frames reach it by reading the scanout back, one copy per frame. The blueprint's
  present path (gfxstream drawing to a swapchain on the window, no copy) is not built.
- That the function signatures, not only the names, match `rutabaga_gfx`'s declarations.
- Whether the Cuttlefish image's guest side (its gfxstream guest drivers) talks to this build.
- Windows and Linux builds.

## Windows (2026-11-19, Ryzen 7 9700X, RTX 5070, NVIDIA 610.88)

- The renderer builds at the same revision with Visual Studio 2022's clang-cl and Ninja
  (`scripts/build-gfxstream.sh` from Git Bash). Two changes, both in the script: Windows.h is kept
  lean (`WIN32_LEAN_AND_MEAN`) so it does not pull in `winsock.h` ahead of `winsock2.h`, and one
  log call in `host/vulkan/vk_common_operations.cpp` passes a `std::string` to a printf-style
  function, which clang-cl rejects; it gets `.c_str()`.
- `gfxstream_backend.dll` is 6.7 MB and exports all 29 `stream_renderer_*` functions
  `rutabaga_gfx` 0.1.85 declares. `rutabaga_gfx` links its import library from `GFXSTREAM_PATH`
  (`<work>/build/host`) with no other change.
- It loads the system `vulkan-1.dll` and picks the discrete GPU. On Windows it turns on
  `VulkanAllocateHostMemory` itself ("Ensure host allocations are aligned to avoid VMM errors
  when mapping"), and the blobs it maps were accepted by `WHvMapGpaRange` with no error.
- With `--gpu` the x86_64 Cuttlefish image (build 16968706) switches to it: SurfaceFlinger reports
  `ANGLE (NVIDIA, Vulkan 1.4.341 (NVIDIA Virtio-GPU GFXStream (NVIDIA GeForce RTX 5070)))`, OpenGL
  ES 3.2, and boots to the lock screen. The guest encoders and this host revision handshake.
