#!/bin/sh
# Installs Google Play services, the Play Store and Google Services Framework into a running
# Android instance, so Google sign-in works. The image is AOSP and has none of them.
#
# usage: scripts/install-google.sh <instance> [MindTheGapps zip]
#
# The instance must have been started with --writable-system (its own copy of the system
# partitions); the shared image is never changed. Without a zip, the MindTheGapps release for
# Android 17 arm64 is downloaded with the GitHub CLI and its checksum checked. Only what sign-in
# needs is installed: no Google app, TalkBack, speech services, Family Link, Wellbeing, setup
# wizard or sync adapters. The instance reboots up to three times.
set -eu
instance=$1
zip=${2:-}
repo=MindTheGapps/17.0.0-arm64
# Port the Mac serves the package on while the guest fetches it.
port=8766
# Most reboots the overlay setup may ask for.
reboots=3
# How long to wait for Android to boot, in 3-second steps.
boot_steps=120
proteus=${PROTEUS:-proteus}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

shell() { "$proteus" ctl "$instance" shell "$1"; }
booted() {
    sleep 10
    i=0
    while [ "$(shell 'getprop sys.boot_completed' 2>/dev/null)" != 1 ]; do
        i=$((i + 1))
        [ "$i" -lt "$boot_steps" ] || { echo "Android did not finish booting" >&2; exit 1; }
        sleep 3
    done
}

if [ -z "$zip" ]; then
    tag=$(gh release list -R "$repo" -L 1 --json tagName -q '.[0].tagName')
    gh release download "$tag" -R "$repo" -p '*.zip' -p '*.sha256sum' -D "$work"
    (cd "$work" && shasum -a 256 -c ./*.sha256sum)
    zip=$(ls "$work"/*.zip)
fi
mkdir "$work/unpacked"
unzip -q "$zip" 'system/*' -d "$work/unpacked"
cd "$work/unpacked/system"
rm -rf product/app/talkback product/app/SpeechServicesByGoogle product/app/GoogleTTS \
    product/app/GoogleCalendarSyncAdapter product/app/GoogleContactsSyncAdapter \
    product/app/PrebuiltExchange3Google product/priv-app/Velvet product/priv-app/VelvetTitan \
    product/priv-app/Wellbeing product/priv-app/FamilyLinkParentalControls \
    product/priv-app/AndroidAutoStub product/priv-app/GooglePartnerSetup \
    product/priv-app/GoogleRestore system_ext/priv-app/SetupWizard \
    system_ext/priv-app/GoogleFeedback addon.d
COPYFILE_DISABLE=1 tar --no-xattrs -cf "$work/google.tar" product system_ext
paths=$(find product system_ext -mindepth 1 -maxdepth 2 | tr '\n' ' ')

# Overlays over the system partitions: the first remount turns verity off and wants a reboot,
# the next sets the overlays up and may want another.
n=0
until shell "su 0 remount" 2>&1 | grep -q 'Remount succeeded'; do
    n=$((n + 1))
    [ "$n" -le "$reboots" ] || { echo "the system partitions could not be made writable; was the instance started with --writable-system?" >&2; exit 1; }
    shell "su 0 reboot" >/dev/null 2>&1 || true
    booted
done

host=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1)
nc -l "$port" < "$work/google.tar" &
sleep 1
shell "su 0 sh -c 'cd / && nc -w 30 $host $port | tar -xf -'"
# What the MindTheGapps installer does: root-owned, 0755 folders, 0644 files, system labels.
shell "su 0 sh -c 'for p in $paths; do chown -R root:root /\$p; find /\$p -type d -exec chmod 0755 {} +; find /\$p -type f -exec chmod 0644 {} +; restorecon -R /\$p >/dev/null 2>&1; done; sync'"
shell "su 0 reboot" >/dev/null 2>&1 || true
booted
shell "pm list packages com.google.android.gms; pm list packages com.android.vending"
echo "Google apps installed; open the Play Store to sign in."
