#!/bin/sh
# Builds Mesa's KosmicKrisp Vulkan driver with this repository's patches (packaging/mesa/*.patch)
# and prints the path of the driver library, for scripts/bundle-macos.sh (KOSMICKRISP=...).
# Homebrew's build has bugs a game hits (see the patches), so releases ship this one.
# usage: scripts/build-kosmickrisp.sh <work folder>
# Needs Homebrew's llvm, spirv-tools, spirv-llvm-translator, libclc, glslang and zstd, and Python.
set -eu
MESA_TAG=mesa-26.3.0
MESA_REPO=https://gitlab.freedesktop.org/mesa/mesa.git
work=$1
repo=$(cd "$(dirname "$0")/.." && pwd)
brew=$(brew --prefix)
mkdir -p "$work"
src="$work/mesa"
if [ ! -d "$src" ]; then
    git clone --quiet --depth 1 --branch "$MESA_TAG" "$MESA_REPO" "$src"
    for patch in "$repo"/packaging/mesa/*.patch; do
        git -C "$src" apply "$patch"
    done
fi
# Meson and Mesa's code generators, kept out of the system Python.
venv="$work/venv"
[ -x "$venv/bin/meson" ] || {
    python3 -m venv "$venv"
    "$venv/bin/pip" install --quiet meson mako pyyaml packaging
}
export PATH="$venv/bin:$brew/opt/llvm/bin:$PATH"
export SDKROOT=$(xcrun --show-sdk-path)
export PKG_CONFIG_PATH="$brew/opt/llvm/lib/pkgconfig:$brew/lib/pkgconfig"
build="$work/build"
[ -f "$build/build.ninja" ] || meson setup "$build" "$src" -Dbuildtype=release -Db_ndebug=true \
    -Dvulkan-drivers=kosmickrisp -Dgallium-drivers= -Dopengl=false -Dgles1=disabled \
    -Dgles2=disabled -Degl=disabled -Dglx=disabled -Dgbm=disabled -Dllvm=enabled \
    -Dplatforms=macos -Dvideo-codecs= -Dtools= -Dvulkan-layers= >&2
ninja -C "$build" src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib >&2
echo "$build/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib"
