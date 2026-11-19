#!/bin/sh
# Builds gfxstream's host renderer (libgfxstream_backend) for this machine, outside the repository.
#
# usage: scripts/build-gfxstream.sh <work folder> [<out folder>]
#
# gfxstream's GitHub repository carries its third-party code, so one checkout at a pinned revision
# is all it needs. Needs git, cmake and ninja. Given an out folder, it also gathers what ships with
# a release there: the library, stripped and signed, and the licences of gfxstream and of the
# third-party code it carries, under licenses/gfxstream.
set -eu

work=$1
out=${2:-}
# The revision the build was checked with (see docs/gfxstream.md).
repository=https://github.com/google/gfxstream
revision=cf47bd2660e7217f5729c91664d36d81c1b3282a

mkdir -p "$work"
if [ ! -d "$work/gfxstream" ]; then
    git init -q "$work/gfxstream"
    git -C "$work/gfxstream" fetch -q --depth 1 "$repository" "$revision"
    git -C "$work/gfxstream" checkout -q FETCH_HEAD
fi
cmake -S "$work/gfxstream" -B "$work/build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_GRAPHICS_DETECTOR=OFF \
    -DENABLE_VKCEREAL_TESTS=OFF
ninja -C "$work/build" gfxstream_backend
ls -l "$work/build"/libgfxstream_backend.*

if [ -n "$out" ]; then
    mkdir -p "$out/licenses/gfxstream"
    cp "$work/build/libgfxstream_backend.dylib" "$out/"
    strip -x "$out/libgfxstream_backend.dylib"
    codesign --sign - --force "$out/libgfxstream_backend.dylib"
    rsync -a --prune-empty-dirs --exclude=.git --include='*/' --include='LICENSE*' \
        --include='COPYING*' --include='NOTICE*' --exclude='*' \
        "$work/gfxstream/" "$out/licenses/gfxstream/"
    test -f "$out/licenses/gfxstream/LICENSE"
    ls -l "$out"
fi
