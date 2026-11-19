#!/bin/sh
# Lays out a macOS folder that draws with the GPU and needs no setup: emu, gfxstream's renderer
# beside it, and the Vulkan loader and MoltenVK in lib64/vulkan, where gfxstream looks for them
# (emu --gpu then picks MoltenVK by itself). Keep the folder together; a symbolic link to emu
# elsewhere works.
#
# usage: scripts/bundle-macos.sh <emu built with --features gpu> <gfxstream out folder> <out folder>
#
# <gfxstream out folder> is the second argument given to scripts/build-gfxstream.sh. The loader and
# MoltenVK come from Homebrew: brew install vulkan-loader molten-vk.
set -eu

emu=$1
gfx=$2
out=$3
brew=$(brew --prefix)
repo=$(cd "$(dirname "$0")/.." && pwd)

mkdir -p "$out/lib64/vulkan" "$out/licenses"
cp "$emu" "$gfx/libgfxstream_backend.dylib" "$out/"
cp -R "$gfx/licenses/." "$out/licenses/"
cp -L "$brew/opt/vulkan-loader/lib/libvulkan.dylib" "$brew/opt/molten-vk/lib/libMoltenVK.dylib" \
    "$out/lib64/vulkan/"
chmod u+w "$out/lib64/vulkan/"*.dylib
cp "$repo/packaging/macos/MoltenVK_icd.json" "$out/lib64/vulkan/"
cp "$brew/opt/vulkan-loader/LICENSE.txt" "$out/licenses/Vulkan-Loader.txt"
cp "$brew/opt/molten-vk/LICENSE" "$out/licenses/MoltenVK.txt"
# emu finds the renderer beside itself; signing comes last, as changing the file breaks it.
otool -l "$out/emu" | grep -q @executable_path || install_name_tool -add_rpath @executable_path "$out/emu"
codesign --sign - --force --entitlements "$repo/packaging/macos/hv.entitlements" "$out/emu"
ls -l "$out" "$out/lib64/vulkan"
