#!/bin/sh
# Builds gfxstream's host renderer (libgfxstream_backend) for this machine, outside the repository.
#
# usage: scripts/build-gfxstream.sh <work folder>
#
# gfxstream's GitHub repository carries its third-party code, so one checkout at a pinned revision
# is all it needs. Needs git, cmake and ninja.
set -eu

work=$1
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
