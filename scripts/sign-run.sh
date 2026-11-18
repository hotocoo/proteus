#!/bin/sh
# Cargo runner for macOS: ad-hoc sign the binary with the hypervisor entitlement, then run it.
set -eu
bin="$1"
shift
entitlements="$(dirname "$0")/../packaging/macos/hv.entitlements"
codesign --sign - --force --entitlements "$entitlements" "$bin" 2>/dev/null
exec "$bin" "$@"
