#!/bin/sh
# Lays out a macOS folder that draws with the GPU and needs no setup: proteus, gfxstream's renderer
# beside it, and the Vulkan loader and MoltenVK in lib64/vulkan, where gfxstream looks for them
# (proteus --gpu then picks MoltenVK by itself). Keep the folder together; a symbolic link to proteus
# elsewhere works.
#
# usage: scripts/bundle-macos.sh <proteus built with --features gpu> <gfxstream out folder> <out folder>
#                                [<apps folder>]
#
# <apps folder> (scripts/fetch-apps.sh) holds APKs installed when a data folder first boots.
#
# <gfxstream out folder> is the second argument given to scripts/build-gfxstream.sh. The loader and
# MoltenVK come from Homebrew: brew install vulkan-loader molten-vk.
set -eu

proteus=$1
gfx=$2
out=$3
apps=${4:-}
brew=$(brew --prefix)
repo=$(cd "$(dirname "$0")/.." && pwd)

mkdir -p "$out/lib64/vulkan" "$out/licenses"
cp "$proteus" "$gfx/libgfxstream_backend.dylib" "$out/"
cp -R "$gfx/licenses/." "$out/licenses/"
# The loader: one built for older Macs when VULKAN_LOADER names it, else Homebrew's.
loader=${VULKAN_LOADER:-$brew/opt/vulkan-loader/lib/libvulkan.dylib}
cp -L "$loader" "$out/lib64/vulkan/libvulkan.dylib"
cp -L "$brew/opt/molten-vk/lib/libMoltenVK.dylib" "$out/lib64/vulkan/"
chmod u+w "$out/lib64/vulkan/"*.dylib
cp "$repo/packaging/macos/MoltenVK_icd.json" "$out/lib64/vulkan/"
# KosmicKrisp (Mesa), the preferred driver, with the Homebrew libraries it links, when this Mac's
# Mesa has it (macOS 26 builds do).
# KOSMICKRISP names a patched build (scripts/build-kosmickrisp.sh), which releases use.
kk_icd="$brew/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json"
if [ -n "${KOSMICKRISP:-}" ] || [ -f "$kk_icd" ]; then
    kk=${KOSMICKRISP:-$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["ICD"]["library_path"])' "$kk_icd")}
    for package in $(python3 "$repo/scripts/bundle-dylib.py" "$kk" "$out/lib64/vulkan"); do
        for licence in "$brew/opt/$package"/LICENSE* "$brew/opt/$package"/COPYING* "$brew/opt/$package"/license*; do
            [ -f "$licence" ] && cp "$licence" "$out/licenses/$package-$(basename "$licence")"
        done
    done
    cp "$repo/packaging/macos/libkosmickrisp_icd.json" "$out/lib64/vulkan/"
    # A patched build is Mesa too; its licence is the same as Homebrew's copy of it.
    if [ -n "${KOSMICKRISP:-}" ]; then
        for licence in "$brew/opt/mesa"/LICENSE* "$brew/opt/mesa"/COPYING* "$brew/opt/mesa"/license*; do
            [ -f "$licence" ] && cp "$licence" "$out/licenses/mesa-$(basename "$licence")"
        done
    fi
fi
# Key maps for particular games, copied into an instance's key map folder when it has none.
mkdir -p "$out/keymaps"
cp "$repo/assets/keymaps/presets/"*.toml "$out/keymaps/"
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
# proteus finds the renderer beside itself; signing comes last, as changing the file breaks it.
otool -l "$out/proteus" | grep -q @executable_path || install_name_tool -add_rpath @executable_path "$out/proteus"
codesign --sign - --force --entitlements "$repo/packaging/macos/hv.entitlements" "$out/proteus"
ls -l "$out" "$out/lib64/vulkan"
