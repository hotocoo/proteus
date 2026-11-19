#!/bin/sh
# Wraps a folder made by scripts/bundle-macos.sh (with kernel-arm64 in it) into Emu.app, which
# opens the launcher when double-clicked. Everything stays beside the program in Contents/MacOS,
# where gfxstream and emu look for their libraries, the kernel and the bundled apps.
#
# usage: scripts/make-app.sh <bundle folder> <version> <out folder>   (makes <out folder>/Emu.app)
set -eu

from=$1
version=$2
app="$3/Emu.app"
repo=$(cd "$(dirname "$0")/.." && pwd)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# Code beside the program; everything else in Resources, where emu also looks.
cp "$from/emu" "$from/libgfxstream_backend.dylib" "$app/Contents/MacOS/"
for item in lib64 apps licenses kernel-arm64 kernel-arm64.config kernel-arm64.source.txt KERNEL.md \
    README.md; do
    if [ -e "$from/$item" ]; then cp -R "$from/$item" "$app/Contents/Resources/"; fi
done
sed "s/VERSION/$version/g" "$repo/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
# Inside out: libraries, then the program with the hypervisor entitlement, then the bundle.
find "$app/Contents" -name '*.dylib' -exec codesign --sign - --force {} \;
codesign --sign - --force --entitlements "$repo/packaging/macos/hv.entitlements" \
    "$app/Contents/MacOS/emu"
codesign --sign - --force --entitlements "$repo/packaging/macos/hv.entitlements" "$app"
codesign --verify --deep "$app"
echo "$app"
