#!/bin/sh
# Downloads Google's prebuilt AOSP Cuttlefish image for a build target and unzips it.
#
# usage: scripts/fetch-cuttlefish.sh <target> <folder> [<build id>]
#   target: aosp_cf_arm64_only_phone-userdebug or aosp_cf_x86_64_only_phone-userdebug
#
# Without a build id the newest build that passed is taken from ci.android.com; the download link
# comes from the artifact's page.
set -eu

target=$1
folder=$2
branch=aosp-android-latest-release
site=https://ci.android.com/builds

build=${3:-}
[ -n "$build" ] || build=$(curl -sSf --retry 3 "$site/branches/$branch/status.json" |
    python3 -I -c 'import json, sys
target = sys.argv[1]
for entry in json.load(sys.stdin)["targets"]:
    if entry["name"] == target:
        print(entry["last_known_good_build"])' "$target")
[ -n "$build" ] || { echo "no build of $target found" >&2; exit 1; }
product=${target%-userdebug}
artifact=${product}-img-$build.zip
url=$(curl -sSf --retry 3 "$site/submitted/$build/$target/latest/$artifact" |
    grep -o 'https://storage.googleapis.com/android-build[^"]*' | head -1 | sed 's/\\u0026/\&/g')
[ -n "$url" ] || { echo "no download link for $artifact" >&2; exit 1; }
mkdir -p "$folder"
echo "$target build $build"
curl -sSf --retry 5 -o "$folder/image.zip" "$url"
(cd "$folder" && unzip -q image.zip && rm image.zip)
ls -l "$folder"
