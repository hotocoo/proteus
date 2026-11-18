# Measurements against the blueprint's budgets

Recorded by hand; each line says where and how. The budgets (blueprint section 1) are an `emu`
executable under 30 MB, cold boot under 500 ms to the guest's first-stage `init`, and guest idle
RAM under 400 MB on the project's stripped base image.

| What | Result | Where and how |
|---|---|---|
| `emu` executable | 1.7 MB | CI release build, macOS arm64; CI fails above 30 MB |
| gfxstream renderer, if linked | +6.6 MB stripped | `scripts/build-gfxstream.sh`, see `gfxstream.md` |
| Process start to Android first-stage `init` | 123-128 ms (3 runs) | Apple M-series Mac, HVF, `--boot-timing --headless`, Google's `aosp_cf_arm64_only_phone` 16644040 with this repository's kernel |
| Looking for a host gamepad at start, when there is none | +130 ms | Same Mac, plain Linux guest: 102 ms without, 232 ms with |
| Guest RAM in use, idle at the home screen | 1.30 GB (960 MB PSS + 344 MB kernel) | Same Mac and image, 4 GiB guest, 2 minutes after `sys.boot_completed`, `dumpsys meminfo` |

The RAM figure is for Google's unmodified Cuttlefish image, which draws with the CPU and keeps
telephony and every other service the stripped image removes. The 400 MB budget applies to that
stripped image, which needs a Linux machine to build and has not been built.
