# Measurements against the blueprint's budgets

Recorded by hand; each line says where and how. The budgets (blueprint section 1) are an `proteus`
executable under 30 MB, cold boot under 500 ms to the guest's first-stage `init`, and guest idle
RAM under 400 MB on the project's stripped base image.

| What | Result | Where and how |
|---|---|---|
| `proteus` executable | 1.7 MB (8.4 MB with the GUI) | CI release build, macOS arm64; CI fails above 30 MB counting `proteus` and the gfxstream renderer |
| gfxstream renderer, if linked | +6.6 MB stripped | `scripts/build-gfxstream.sh`, see `gfxstream.md` |
| Process start to Android first-stage `init` | 123-128 ms (3 runs) | Apple M-series Mac, HVF, `--boot-timing --headless`, Google's `aosp_cf_arm64_only_phone` 16644040 with this repository's kernel |
| Looking for a host gamepad at start, when there is none | +130 ms | Same Mac, plain Linux guest: 102 ms without, 232 ms with |
| Process start to Android `sys.boot_completed`, GPU | 14.2 s, first boot of a new data folder | Apple M4 Max, macOS 26.5, Proteus.app 0.3.0 with KosmicKrisp, headless, polled every 0.5 s over adb |
| Host CPU of a running instance, idle | about 5% headless, about 20% with the window; 50-60% for the first minute after boot (Android's own start-up work) | Same Mac, `top`, home screen |
| Guest RAM in use, GPU, 20 s after boot | 1.27 GB (923 MB PSS + 342 MB kernel) | Same Mac, 4 GiB guest, `dumpsys meminfo` |
| Guest RAM in use, idle at the home screen | 1.30 GB (960 MB PSS + 344 MB kernel) | Same Mac and image, 4 GiB guest, 2 minutes after `sys.boot_completed`, `dumpsys meminfo` |

The RAM figure is for Google's unmodified Cuttlefish image, which draws with the CPU and keeps
telephony and every other service the stripped image removes. The 400 MB budget applies to that
stripped image, which needs a Linux machine to build and has not been built.

Not measured: frame rates and input latency in games, and any comparison with Google's emulator
on the same Mac (the blueprint's benchmark); they need a game test app and Google's emulator
installed, which this machine does not have.
