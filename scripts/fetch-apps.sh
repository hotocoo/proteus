#!/bin/sh
# Downloads the apps a release carries in apps/, each checked against its pinned SHA-256:
# Aurora Store, an open-source client for Google Play's catalogue (GPL-3.0, built by F-Droid from
# https://gitlab.com/AuroraOSS/AuroraStore).
#
# usage: scripts/fetch-apps.sh <apps folder>
set -eu

out=$1
mkdir -p "$out"
fetch() {
    name=$1 url=$2 sum=$3
    if [ ! -f "$out/$name" ]; then
        curl -sSfL --retry 3 -o "$out/$name.part" "$url"
        mv "$out/$name.part" "$out/$name"
    fi
    echo "$sum  $out/$name" | shasum -a 256 -c -
}
fetch aurora-store.apk https://f-droid.org/repo/com.aurora.store_76.apk \
    fd9c75d90d0f4a7c132b9b4a5a2cf1992a45e03b8d8ff988b7dcfbc0db2c4d11
