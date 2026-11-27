//! What the emulator does to Android once it has booted: stops the image's own log streaming when
//! nobody reads it (it costs about one guest CPU), switches Bluetooth off (the board has no
//! Bluetooth controller, so the stack crashes and restarts every few seconds), switches UWB off
//! (its failure files bug reports with screenshots), turns off what a game machine does not need
//! (ad services, printing, screensavers), gives Android back the panel's own shape when there is
//! no window to give it one, and installs the apps a release carries beside the program (`apps/`,
//! such as an app store) the first time a data folder boots.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread::JoinHandle;

use crate::adb;
use crate::config::{
    ANON_TRIM_MAX_KIB, ANON_TRIM_MIN_KIB, BUNDLED_APPS_DIR, CACHE_TRIM_PERIOD, CACHE_TRIM_SHARE,
    INSTALLED_APPS_FILE, SETUP_POLL, SETUP_TIMEOUT, SHADER_CACHE_BYTES,
};
use crate::copy;
use crate::session::PowerSwitch;

/// Prints `1` once Android has finished booting.
const BOOT_COMPLETED: &str = "getprop sys.boot_completed";
const BOOTED: &str = "1";
/// Stops Cuttlefish's `seriallogging` service, which copies every log line to a console port.
const STOP_LOG_STREAM: &str = "su 0 stop seriallogging";
/// Switches Bluetooth off and keeps it off: the image's Bluetooth stack aborts with "Can't start
/// HAL" when it finds no controller, and Android starts it again at once.
const BLUETOOTH_OFF: &str = "cmd bluetooth_manager disable; settings put global bluetooth_on 0; \
     settings put global ble_scan_always_enabled 0";
/// Switches UWB off: the board has no UWB radio, the service fails to start, and on that error
/// Android files a bug report (a screenshot and about half a minute of work) and shows a
/// notification for it. Reports it already made are deleted, and their notification goes with
/// the shell app's.
const UWB_OFF: &str = "cmd uwb disable-uwb; \
     device_config put uwb device_error_bugreport_enabled false; \
     su 0 rm -rf /data/user_de/0/com.android.shell/files/bugreports; \
     am force-stop com.android.shell";
/// Turns off parts of the image a game machine has no use for, every boot (an update of the image
/// could bring them back): Android's ad services and the on-device ad and learning services that
/// feed them, remote key provisioning (it calls Google's servers), device-lock financing, printing,
/// screensavers, satellite messaging, demo and tracing apps, and phone extras with no modem or
/// SIM behind them (SMS, emergency broadcasts, carrier calling setup), the stock calendar and
/// camera, work-profile setup, diagnostics and app-link checking. `pm enable <package>` brings one
/// back. Packages the image does not have, or will not let go, are skipped.
const GAMING_TRIM: &str = "for package in \
     com.android.adservices.api com.android.federatedcompute.services \
     com.android.ondevicepersonalization.services com.android.rkpdapp \
     com.android.devicelockcontroller com.android.printspooler com.android.bips \
     com.android.printservice.recommendation com.android.dreams.basic \
     com.android.dreams.phototable com.google.android.telephony.satellite \
     com.android.threadnetwork.demoapp com.android.traceur com.android.wallpaper.livepicker \
     com.android.messaging com.android.cellbroadcastreceiver \
     com.android.cellbroadcastreceiver.module com.android.imsserviceentitlement \
     com.android.calendar com.android.providers.calendar com.android.camera2 \
     com.android.managedprovisioning com.android.devicediagnostics \
     com.android.statementservice; \
     do pm disable-user --user 0 $package >/dev/null 2>&1; done";
/// Stops the image's Thread network services. They are Cuttlefish's, made for its host tools,
/// and exit without its simulated radio, so init restarts them every five seconds for good. The
/// cellular radio service stays: it talks to `crate::modem`, which answers as a modem with no SIM.
const RADIOS_OFF: &str = "su 0 cmd thread_network disable; \
     su 0 cmd thread_network force-stop-ot-daemon enabled; \
     su 0 setprop ctl.stop vendor.threadnetwork_hal";
/// Keeps little unwritten data in the guest. By default Linux holds up to a fifth of RAM (1.6 GB
/// with 8 GB) in its cache before writing it out, and a machine that stops without a clean
/// shutdown loses all of it: a game download went back from 4.8 to 3.6 GB that way. With these,
/// at most a few seconds' worth is ever at risk, and disk writes come in a steady stream.
const DIRTY_LIMITS: &str = "su 0 sh -c 'echo 33554432 > /proc/sys/vm/dirty_background_bytes; \
     echo 268435456 > /proc/sys/vm/dirty_bytes; echo 500 > /proc/sys/vm/dirty_expire_centisecs; \
     echo 100 > /proc/sys/vm/dirty_writeback_centisecs'";
/// Lets go of Android's clean file cache once it holds more than `CACHE_TRIM_SHARE` of the
/// guest's RAM; the emulator runs it every `CACHE_TRIM_PERIOD`. Each byte of guest RAM is host
/// memory, and a game download fills the cache with files nobody reads again (6.4 of 8 GB in one
/// case), which the Mac then swaps out. That cache is a second copy: the Mac caches the disk image
/// itself, in memory it can drop and does not charge to this process, so a dropped file comes back
/// from there quickly. Freed pages go back to the host through the balloon.
///
/// Only the excess goes, coldest pages first (`memory.reclaim`, no swapping), and never more than
/// the inactive part, so files in use stay. Dropping the whole cache also threw out the files a
/// game was streaming from, and the guest stalled for seconds reading them back, once a minute.
/// Shared memory counts as cache but cannot be dropped, so it is left out of the sum. Kernels
/// without `memory.reclaim` still drop the whole cache.
fn cache_trim() -> String {
    format!(
        "su 0 sh -c 'set -- $(grep MemTotal /proc/meminfo); total=$2; \
         set -- $(grep ^Cached /proc/meminfo); cached=$2; \
         set -- $(grep ^Shmem: /proc/meminfo); shmem=$2; \
         set -- $(grep \"^Inactive(file)\" /proc/meminfo); inactive=$2; \
         extra=$((cached - shmem - total / {CACHE_TRIM_SHARE})); \
         [ $extra -le $inactive ] || extra=$inactive; [ $extra -gt 0 ] || exit 0; \
         if [ -e {MEMORY_RECLAIM} ]; then echo \"${{extra}}K swappiness=0\" > {MEMORY_RECLAIM}; \
         else echo 1 > /proc/sys/vm/drop_caches; fi'"
    )
}
/// Moves memory nobody has touched lately into zram, compressed, once the kernel has marked it
/// inactive; the emulator runs it with `cache_trim`. A game keeps gigabytes it loaded once and
/// never reads again (Wuthering Waves: 2.4 of its 3.1 GB), all of it host memory while it sits
/// in the guest uncompressed. Compressed it takes about 2.4 times less, and the guest's freed
/// pages go back to the host: 1.2 GB less on the Mac in that game, at the same 60 fps, with
/// about 1 MB a second read back from zram while walking. Only the inactive list goes, at most
/// `config::ANON_TRIM_MAX_KIB` a time, so a page in use is not pushed out and no single pass
/// stalls the guest long.
fn cold_memory_trim() -> String {
    format!(
        "su 0 sh -c '[ -e {MEMORY_RECLAIM} ] || exit 0; \
         set -- $(grep \"^Inactive(anon)\" /proc/meminfo); cold=$2; \
         [ $cold -le {ANON_TRIM_MAX_KIB} ] || cold={ANON_TRIM_MAX_KIB}; \
         [ $cold -ge {ANON_TRIM_MIN_KIB} ] || exit 0; \
         echo \"${{cold}}K swappiness={ANON_ONLY_SWAPPINESS}\" > {MEMORY_RECLAIM}'"
    )
}
/// `memory.reclaim`'s swappiness that takes anonymous memory only, never file cache.
const ANON_ONLY_SWAPPINESS: u32 = 200;
/// Compresses zram with zstd instead of the image's lz4: in Wuthering Waves 2.3 times smaller
/// rather than 1.9, at a few microseconds more per page read back. The compressor can only be
/// changed on an empty device, so this runs right after boot, when zram holds next to nothing:
/// swap goes off (what is in it comes back into memory), the device is reset with zstd, and swap
/// comes back on at the same size. A kernel without zstd keeps what it has.
const ZRAM_ZSTD: &str = "su 0 sh -c 'z=/sys/block/zram0; \
     grep -q zstd $z/comp_algorithm || exit 0; grep -q \"\\[zstd\\]\" $z/comp_algorithm && exit 0; \
     size=$(cat $z/disksize); swapoff /dev/block/zram0 || exit 0; echo 1 > $z/reset; \
     echo zstd > $z/comp_algorithm; echo $size > $z/disksize; \
     mkswap /dev/block/zram0 >/dev/null && swapon /dev/block/zram0'";
/// The guest kernel's control for reclaiming memory from the coldest pages first.
const MEMORY_RECLAIM: &str = "/sys/fs/cgroup/memory.reclaim";
/// Has apps keep their translated GL shader programs in Android's multi-file cache, with room for
/// `SHADER_CACHE_BYTES`, rather than the 2 MB single-file one, which a game outgrows at once, so
/// it translated its shaders again in every session. Read by each app at its start.
fn shader_cache() -> String {
    format!(
        "su 0 sh -c 'setprop debug.egl.blobcache.multifile true; \
         setprop debug.egl.blobcache.multifile_limit {SHADER_CACHE_BYTES}'"
    )
}
/// Makes apps share memory through memfd. The guest kernel has no `/dev/ashmem`, and without this
/// Android still tries it: every shared bitmap (a notification's icon, say) and database cursor
/// fails, and apps that post notifications crash in `Bitmap_copyAshmem`. Read once per process,
/// so it reaches apps started after setup (every app the user opens).
const SHARED_MEMORY: &str = "su 0 setprop sys.use_memfd true";
/// Keeps ANGLE (Android's GL on Vulkan) from making a Vulkan event for every image and buffer
/// barrier. Each vkCreateEvent is a guest-to-host round trip that waits for the reply; in a game
/// (Wuthering Waves) they took 62% of the render thread's time and froze frames for up to a
/// second. Pipeline barriers do the same job with no round trip. Read by each app at its start.
const ANGLE_NO_EVENTS: &str = "su 0 setprop debug.angle.feature_overrides_disabled \
     useVkEventForImageBarrier:useVkEventForBufferBarrier";
/// Has the guest hand back free memory in pieces as small as 16 KiB (order 2, one Mac page)
/// rather than 2 MiB. Free memory in a running game is fragmented: at 2 MiB none was reported,
/// and at 128 KiB (order 5) about 600 MB of a 4 GB guest's free memory stayed counted against
/// this Mac. At 16 KiB that falls to about 150 MB, with no measurable cost (6 GB of allocate and
/// free in the guest took the same time).
const REPORT_SMALL_FREES: &str =
    "su 0 sh -c 'echo 2 > /sys/module/page_reporting/parameters/page_reporting_order'";
/// Turns off Unreal Engine's hang watchdog in every installed Unreal game. The watchdog aborts the
/// game when a thread misses its heartbeat for `HangDuration` seconds; loading under emulation can
/// stall that long where a phone would not, and Wuthering Waves died that way mid-load
/// (`FThreadHeartBeat::OnHang`, SIGABRT). A duration of 0 skips the check. Each game reads its
/// saved Engine.ini at start; appending keeps the file's owner, and a repeated section merges.
const UNREAL_NO_HANG_ABORT: &str = "su 0 sh -c 'for ini in \
     /data/media/0/Android/data/*/files/UE4Game/*/*/Saved/Config/Android/Engine.ini \
     /data/media/0/Android/data/*/files/UnrealGame/*/*/Saved/Config/Android/Engine.ini; do \
     [ -f \"$ini\" ] || continue; grep -q ^HangDuration=0 \"$ini\" && continue; \
     printf \"\\n[Core.System]\\nHangDuration=0\\nPresentHangDuration=0\\n\" >> \"$ini\"; done'";
/// Has Android manage memory the way a phone does, which this image leaves off: apps in the
/// background have their memory compressed into zram (app compaction), and cached apps are frozen
/// so they stop touching it. The game in front is left alone. Compressed memory is memory the
/// guest frees, and freed memory goes back to the Mac.
const PHONE_MEMORY: &str = "device_config put activity_manager use_compaction true; \
     settings put global cached_apps_freezer enabled";
/// Tells the guest its disks are solid state with no request scheduler. The kernel takes a virtio
/// disk for a spinning one and queues its requests through mq-deadline, which only delays them:
/// the host does its own ordering and caching.
const DISK_QUEUES: &str = "su 0 sh -c 'for queue in /sys/block/vd*/queue; do \
     echo 0 > $queue/rotational; echo none > $queue/scheduler; done'";
/// Bundled apps are files with this ending.
const APK_EXTENSION: &str = "apk";

/// The apps beside the program, by file name, that `installed` (one name per line) does not list.
pub fn to_install(bundled: &[String], installed: &str) -> Vec<String> {
    let done: BTreeSet<&str> = installed.lines().map(str::trim).collect();
    bundled
        .iter()
        .filter(|name| !done.contains(name.as_str()))
        .cloned()
        .collect()
}

/// The `.apk` files in `dir`, by name, sorted.
fn bundled_apps(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry.path().extension().and_then(|ext| ext.to_str()) == Some(APK_EXTENSION)
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The folder of bundled apps: `apps/` beside the program, if it has one.
fn apps_dir() -> Option<PathBuf> {
    crate::config::bundled(BUNDLED_APPS_DIR).map(|(_, path)| path)
}

/// Waits for Android to boot, then sets it up, on its own thread, until `done` is set; again
/// after each restart of the guest, since a restart undoes what was set.
/// `data` is the instance's Android data folder, where installed bundled apps are noted.
pub fn spawn(
    adb_port: Arc<AtomicU16>,
    power: Arc<PowerSwitch>,
    done: Arc<AtomicBool>,
    keep_log_stream: bool,
    reset_shape: bool,
    data: Option<PathBuf>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(copy::SETUP_THREAD.into())
        .spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let boot = power.boot();
                let Some(port) = booted(&adb_port, &done) else {
                    return;
                };
                set_up(port, keep_log_stream, reset_shape, data.as_deref());
                // Until this boot is over; `done` is looked at now and then meanwhile.
                // Booting reads gigabytes the cache keeps; dropped now, not a minute later.
                let mut trimmed = std::time::Instant::now() - CACHE_TRIM_PERIOD;
                while !done.load(Ordering::SeqCst) && !power.wait_for(boot, SETUP_POLL) {
                    if trimmed.elapsed() >= CACHE_TRIM_PERIOD {
                        trimmed = std::time::Instant::now();
                        for trim in [cache_trim(), cold_memory_trim()] {
                            if let Err(err) = adb::output(port, &trim) {
                                log::debug!("{}", copy::setup_failed(&err));
                            }
                        }
                    }
                }
            }
        })
}

/// Waits until Android has booted and gives its adb port, or `None` once `done` is set.
fn booted(adb_port: &AtomicU16, done: &AtomicBool) -> Option<u16> {
    loop {
        if done.load(Ordering::SeqCst) {
            return None;
        }
        let port = adb_port.load(Ordering::SeqCst);
        let booted = port != 0
            && adb::output_within(port, BOOT_COMPLETED, SETUP_TIMEOUT)
                .is_ok_and(|printed| printed.trim() == BOOTED);
        if booted {
            return Some(port);
        }
        std::thread::sleep(SETUP_POLL);
    }
}

fn set_up(port: u16, keep_log_stream: bool, reset_shape: bool, data: Option<&Path>) {
    let shader_cache = shader_cache();
    let mut steps = vec![
        DIRTY_LIMITS,
        SHARED_MEMORY,
        &shader_cache,
        ANGLE_NO_EVENTS,
        DISK_QUEUES,
        REPORT_SMALL_FREES,
        ZRAM_ZSTD,
        UNREAL_NO_HANG_ABORT,
        PHONE_MEMORY,
        BLUETOOTH_OFF,
        UWB_OFF,
        RADIOS_OFF,
        GAMING_TRIM,
    ];
    // Without a window to give Android its shape, a logical size from an earlier run would
    // leave bars in every frame; a window sets (and reads back) the shape itself.
    if reset_shape {
        steps.push(crate::fill::WM_SIZE_RESET);
    }
    if !keep_log_stream {
        steps.push(STOP_LOG_STREAM);
    }
    for step in steps {
        if let Err(err) = adb::output(port, step) {
            log::warn!("{}", copy::setup_failed(&err));
        }
    }
    if let (Some(apps), Some(data)) = (apps_dir(), data) {
        install_bundled(port, &apps, data);
    }
}

fn install_bundled(port: u16, apps: &Path, data: &Path) {
    let noted = data.join(INSTALLED_APPS_FILE);
    let installed = std::fs::read_to_string(&noted).unwrap_or_default();
    let mut list = installed.clone();
    for name in to_install(&bundled_apps(apps), &installed) {
        match crate::install::install(port, &apps.join(&name)) {
            Ok(()) => {
                eprintln!("{}", copy::bundled_installed(&name));
                list.push_str(&name);
                list.push('\n');
            }
            Err(err) => eprintln!("{}", copy::install_failed(&apps.join(&name), &err)),
        }
    }
    if list != installed
        && let Err(err) = std::fs::write(&noted, list)
    {
        log::warn!("{}", copy::file(&noted, &err));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_apps_are_installed_once() {
        let bundled = ["aurora.apk".to_owned(), "fdroid.apk".to_owned()];
        assert_eq!(to_install(&bundled, ""), bundled);
        assert_eq!(to_install(&bundled, "aurora.apk\n"), ["fdroid.apk"]);
        assert!(to_install(&bundled, "fdroid.apk\naurora.apk\n").is_empty());
    }

    #[test]
    fn cache_trim_reclaims_only_the_excess() {
        let trim = cache_trim();
        println!("{trim}");
        assert!(trim.contains(&format!("cached - shmem - total / {CACHE_TRIM_SHARE}")));
        assert!(trim.contains("[ $extra -le $inactive ] || extra=$inactive"));
        assert!(trim.contains(r#"echo "${extra}K swappiness=0" > /sys/fs/cgroup/memory.reclaim"#));
        assert!(trim.contains("else echo 1 > /proc/sys/vm/drop_caches"));
    }

    #[test]
    fn cold_memory_trim_takes_only_inactive_anonymous_memory_capped() {
        let trim = cold_memory_trim();
        assert!(trim.contains(r#"grep "^Inactive(anon)" /proc/meminfo"#));
        assert!(trim.contains(&format!("cold={ANON_TRIM_MAX_KIB}")));
        assert!(trim.contains(&format!("-ge {ANON_TRIM_MIN_KIB} ] || exit 0")));
        assert!(trim.contains(r#"echo "${cold}K swappiness=200" > /sys/fs/cgroup/memory.reclaim"#));
    }
}
