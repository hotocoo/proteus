#!/bin/sh
# Lays out a macOS folder that draws with the GPU and needs no setup: emu, gfxstream's renderer
# beside it, and the Vulkan loader and MoltenVK in lib64/vulkan, where gfxstream looks for them
# (emu --gpu then picks MoltenVK by itself). Keep the folder together; a symbolic link to emu
# elsewhere works.
#
# usage: scripts/bundle-macos.sh <emu built with --features gpu> <gfxstream out folder> <out folder>
#                                [<apps folder>]
#
# <apps folder> (scripts/fetch-apps.sh) holds APKs installed when a data folder first boots.
#
# <gfxstream out folder> is the second argument given to scripts/build-gfxstream.sh. The loader and
# MoltenVK come from Homebrew: brew install vulkan-loader molten-vk.
set -eu

emu=$1
gfx=$2
out=$3
apps=${4:-}
brew=$(brew --prefix)
repo=$(cd "$(dirname "$0")/.." && pwd)

mkdir -p "$out/lib64/vulkan" "$out/licenses"
cp "$emu" "$gfx/libgfxstream_backend.dylib" "$out/"
cp -R "$gfx/licenses/." "$out/licenses/"
cp -L "$brew/opt/vulkan-loader/lib/libvulkan.dylib" "$brew/opt/molten-vk/lib/libMoltenVK.dylib" \
    "$out/lib64/vulkan/"
chmod u+w "$out/lib64/vulkan/"*.dylib
cp "$repo/packaging/macos/MoltenVK_icd.json" "$out/lib64/vulkan/"
# KosmicKrisp (Mesa), the preferred driver, with the Homebrew libraries it links, when this Mac's
# Mesa has it (macOS 26 builds do).
kk_icd="$brew/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json"
if [ -f "$kk_icd" ]; then
    kk=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["ICD"]["library_path"])' "$kk_icd")
    for package in $(python3 "$repo/scripts/bundle-dylib.py" "$kk" "$out/lib64/vulkan"); do
        for licence in "$brew/opt/$package"/LICENSE* "$brew/opt/$package"/COPYING* "$brew/opt/$package"/license*; do
            [ -f "$licence" ] && cp "$licence" "$out/licenses/$package-$(basename "$licence")"
        done
    done
    cp "$repo/packaging/macos/libkosmickrisp_icd.json" "$out/lib64/vulkan/"
fi
cp "$brew/opt/vulkan-loader/LICENSE.txt" "$out/licenses/Vulkan-Loader.txt"
cp "$brew/opt/molten-vk/LICENSE" "$out/licenses/MoltenVK.txt"
if [ -n "$apps" ]; then
    mkdir -p "$out/apps"
    cp "$apps"/*.apk "$out/apps/"
    cat > "$out/licenses/Aurora-Store.txt" <<'NOTE'
Aurora Store is free software under the GNU General Public License, version 3.
Source code: https://gitlab.com/AuroraOSS/AuroraStore (the APK is F-Droid's build of it,
https://f-droid.org/packages/com.aurora.store/, where the matching source tarball is kept).
NOTE
fi
# emu finds the renderer beside itself; signing comes last, as changing the file breaks it.
otool -l "$out/emu" | grep -q @executable_path || install_name_tool -add_rpath @executable_path "$out/emu"
codesign --sign - --force --entitlements "$repo/packaging/macos/hv.entitlements" "$out/emu"
ls -l "$out" "$out/lib64/vulkan"
