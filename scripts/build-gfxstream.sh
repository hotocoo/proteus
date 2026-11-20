#!/bin/sh
# Builds gfxstream's host renderer (libgfxstream_backend) for this machine, outside the repository.
#
# usage: scripts/build-gfxstream.sh <work folder> [<out folder>]
#
# gfxstream's GitHub repository carries its third-party code, so one checkout at a pinned revision
# is all it needs. Needs git, cmake and ninja. Given an out folder, it also gathers what ships with
# a release there: the library (on macOS stripped and signed), and the licences of gfxstream and
# of the third-party code it carries, under licenses/gfxstream.
#
# On Windows, run it from Git Bash with Visual Studio 2022 and its "C++ Clang tools" installed: it
# builds with clang-cl inside the Visual Studio environment. Link proteus against it with
# GFXSTREAM_PATH=<work folder>/build/host, and put gfxstream_backend.dll beside proteus.exe.
set -eu

work=$1
out=${2:-}
# The revision the build was checked with (see docs/gfxstream.md).
repository=https://github.com/google/gfxstream
revision=cf47bd2660e7217f5729c91664d36d81c1b3282a

case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN*) windows=1 ;;
*) windows= ;;
esac

mkdir -p "$work"
if [ ! -d "$work/gfxstream" ]; then
    git init -q "$work/gfxstream"
    git -C "$work/gfxstream" fetch -q --depth 1 "$repository" "$revision"
    git -C "$work/gfxstream" checkout -q FETCH_HEAD
    # This repository's fixes (packaging/gfxstream), applied once to the fresh checkout.
    repo=$(cd "$(dirname "$0")/.." && pwd)
    for patch in "$repo"/packaging/gfxstream/*.patch; do
        git -C "$work/gfxstream" apply "$patch"
    done
fi

if [ -n "$windows" ]; then
    # clang-cl refuses a std::string passed to a printf-style log call; the other compilers let it by.
    sed -i 's/VulkanAllocateHostMemory\.getReason());/VulkanAllocateHostMemory.getReason().c_str());/' \
        "$work/gfxstream/host/vulkan/vk_common_operations.cpp"
    vs=$("/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe" -latest -property installationPath)
    # WIN32_LEAN_AND_MEAN keeps Windows.h from pulling in winsock.h ahead of winsock2.h.
    w=$(cygpath -m "$work")
    cat > "$work/build.bat" <<EOF
@echo off
call "$vs\\VC\\Auxiliary\\Build\\vcvars64.bat" >nul || exit /b 1
cmake -S "$w/gfxstream" -B "$w/build" -G Ninja -DCMAKE_BUILD_TYPE=Release ^
    -DCMAKE_C_COMPILER=clang-cl -DCMAKE_CXX_COMPILER=clang-cl ^
    "-DCMAKE_C_FLAGS=/DWIN32 /D_WINDOWS /DWIN32_LEAN_AND_MEAN" ^
    "-DCMAKE_CXX_FLAGS=/DWIN32 /D_WINDOWS /EHsc /GR /DWIN32_LEAN_AND_MEAN" ^
    -DBUILD_GRAPHICS_DETECTOR=OFF -DENABLE_VKCEREAL_TESTS=OFF || exit /b 1
ninja -C "$w/build" gfxstream_backend || exit /b 1
EOF
    cmd //c "$(cygpath -w "$work/build.bat")"
    ls -l "$work/build/gfxstream_backend.dll" "$work/build/host/gfxstream_backend.lib"
    library="$work/build/gfxstream_backend.dll"
else
    cmake -S "$work/gfxstream" -B "$work/build" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_GRAPHICS_DETECTOR=OFF \
        -DENABLE_VKCEREAL_TESTS=OFF
    ninja -C "$work/build" gfxstream_backend
    ls -l "$work/build"/libgfxstream_backend.*
    library="$work/build/libgfxstream_backend.dylib"
fi

if [ -n "$out" ]; then
    mkdir -p "$out/licenses/gfxstream"
    cp "$library" "$out/"
    if [ -z "$windows" ]; then
        strip -x "$out/libgfxstream_backend.dylib"
        codesign --sign - --force "$out/libgfxstream_backend.dylib"
    fi
    # Every licence file, at its path in the checkout (Git for Windows has no rsync).
    (cd "$work/gfxstream" && find . -path ./.git -prune -o -type f \
        \( -name 'LICENSE*' -o -name 'COPYING*' -o -name 'NOTICE*' \) -print) |
        while read -r file; do
            mkdir -p "$out/licenses/gfxstream/$(dirname "$file")"
            cp "$work/gfxstream/$file" "$out/licenses/gfxstream/$file"
        done
    test -f "$out/licenses/gfxstream/LICENSE"
    ls -l "$out"
fi
