#!/bin/sh
# Builds the Vulkan loader (libvulkan.dylib) for macOS 15 and later, from Khronos' pinned sources.
# Homebrew's loader is built for the newest macOS only, and gfxstream loads the loader at run time,
# so a release built with it would lose GPU acceleration on older Macs.
#
# usage: scripts/build-vulkan-loader.sh <work folder>    (prints the path of the built library)
# Needs git, cmake and ninja.
set -eu

work=$1
# The release the Homebrew loader of the time was (see docs/gfxstream.md).
version=v1.4.335
minimum_macos=15.0

mkdir -p "$work"
for repository in Vulkan-Headers Vulkan-Loader; do
    if [ ! -d "$work/$repository" ]; then
        git clone -q --depth 1 --branch "$version" \
            "https://github.com/KhronosGroup/$repository.git" "$work/$repository"
    fi
done
cmake -S "$work/Vulkan-Headers" -B "$work/headers-build" -G Ninja \
    -DCMAKE_INSTALL_PREFIX="$work/headers" >/dev/null
cmake --install "$work/headers-build" >/dev/null
cmake -S "$work/Vulkan-Loader" -B "$work/loader-build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_DEPLOYMENT_TARGET="$minimum_macos" \
    -DVULKAN_HEADERS_INSTALL_DIR="$work/headers" \
    -DBUILD_TESTS=OFF >/dev/null
ninja -C "$work/loader-build" >/dev/null
library="$work/loader-build/loader/libvulkan.1.dylib"
otool -l "$library" | grep -A4 LC_BUILD_VERSION | grep -q "minos $minimum_macos"
echo "$library"
