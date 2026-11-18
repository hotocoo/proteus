#!/bin/sh
# Builds gfxstream's host renderer (libgfxstream_backend) for this machine, outside the repository.
#
# usage: scripts/build-gfxstream.sh <work folder>
#
# gfxstream's CMake build expects an Android source tree around it; only the few projects it reads
# are fetched, in that layout. ANGLE and libdrm are only needed for their headers, so only their
# include folders are checked out. Needs git, cmake and ninja.
set -eu

work=$1
aosp=https://android.googlesource.com/platform
# Revisions the build was checked with (see docs/gfxstream.md).
gfxstream_rev=d047a57228332d995d36600792fa9ccc26cf8ae6
aemu_rev=3c1ced8a369417db591eb7cd083af5bb2c317975

mkdir -p "$work/tree/hardware/google" "$work/tree/external"
cd "$work/tree"

fetch() { # <project> [<revision>]
    [ -d "$1" ] && return
    git init -q "$1"
    git -C "$1" fetch -q --depth 1 "$aosp/$1" "${2:-HEAD}"
    git -C "$1" checkout -q FETCH_HEAD
}
headers() { # <project>: only its include folder
    [ -d "$1" ] && return
    git clone -q --depth 1 --filter=blob:none --sparse "$aosp/$1" "$1"
    git -C "$1" sparse-checkout set include
}

fetch hardware/google/gfxstream "$gfxstream_rev"
fetch hardware/google/aemu "$aemu_rev"
fetch external/flatbuffers
fetch external/gfxstream-protocols
headers external/angle
headers external/libdrm

# The macOS window code calls an OpenGL method deprecated since 10.14; the build treats that
# warning as an error.
cmake -S hardware/google/gfxstream -B "$work/build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DDEPENDENCY_RESOLUTION=AOSP \
    -DBUILD_GRAPHICS_DETECTOR=OFF \
    -DENABLE_VKCEREAL_TESTS=OFF \
    -DCMAKE_CXX_FLAGS=-Wno-error=deprecated-declarations \
    -DCMAKE_OBJCXX_FLAGS=-Wno-error=deprecated-declarations
ninja -C "$work/build" gfxstream_backend
ls -l "$work/build"/libgfxstream_backend.*
