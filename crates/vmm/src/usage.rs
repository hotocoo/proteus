//! What this process costs the host right now: CPU time used so far and memory held, for the
//! toolbar's meters. Memory is the figure Activity Monitor shows (the physical footprint on a
//! Mac), so it includes the guest's RAM that Android has touched and the GPU memory its apps use.

use std::time::{Duration, Instant};

/// One reading of the process's cost.
#[derive(Clone, Copy)]
pub struct Reading {
    at: Instant,
    cpu: Duration,
    pub memory_bytes: Option<u64>,
}

/// Reads the process's cost now.
pub fn read() -> Reading {
    Reading {
        at: Instant::now(),
        cpu: cpu_time(),
        memory_bytes: memory(),
    }
}

/// The share of one host core the process used between `earlier` and `later`, in percent (above
/// 100 when it used several cores).
pub fn cpu_percent(earlier: &Reading, later: &Reading) -> Option<f64> {
    let wall = later.at.checked_duration_since(earlier.at)?.as_secs_f64();
    let used = later.cpu.checked_sub(earlier.cpu)?.as_secs_f64();
    (wall > 0.0).then(|| used / wall * 100.0)
}

/// Records a frame stamp tagged `tag` when `config::FRAME_LOG_VARIABLE` names a file.
pub fn log_frame(tag: char) {
    use std::io::Write;
    type Log = std::sync::Mutex<(Instant, std::fs::File)>;
    static LOG: std::sync::OnceLock<Option<Log>> = std::sync::OnceLock::new();
    let log = LOG.get_or_init(|| {
        let path = std::env::var_os(crate::config::FRAME_LOG_VARIABLE)?;
        let file = std::fs::File::create(path).ok()?;
        Some(std::sync::Mutex::new((Instant::now(), file)))
    });
    if let Some(Ok(mut log)) = log.as_ref().map(|log| log.lock()) {
        let micros = log.0.elapsed().as_micros();
        let _ = writeln!(log.1, "{tag} {micros}");
    }
}

/// Recent waits between the guest's frames, for the 1% low.
#[derive(Default)]
pub struct FrameGaps {
    last: Option<Instant>,
    gaps: std::collections::VecDeque<Duration>,
}

impl FrameGaps {
    /// A frame arrived at `now`.
    pub fn frame(&mut self, now: Instant) {
        let gap = self.last.and_then(|last| now.checked_duration_since(last));
        self.last = Some(now);
        let Some(gap) = gap.filter(|gap| *gap <= crate::config::LOW_FPS_MAX_GAP) else {
            return;
        };
        if self.gaps.len() == crate::config::LOW_FPS_FRAMES {
            self.gaps.pop_front();
        }
        self.gaps.push_back(gap);
    }

    /// The frame rate of the slowest 1% of the recent frames (at least one frame).
    pub fn one_percent_low(&self) -> Option<f64> {
        const SHARE: usize = 100;
        let mut gaps: Vec<Duration> = self.gaps.iter().copied().collect();
        gaps.sort_unstable_by(|a, b| b.cmp(a));
        let worst = &gaps[..gaps.len().div_ceil(SHARE)];
        let total: Duration = worst.iter().sum();
        (!worst.is_empty() && !total.is_zero()).then(|| worst.len() as f64 / total.as_secs_f64())
    }
}

/// How busy the host's GPU is, in percent, counting every process. macOS reports it in the
/// accelerator's `PerformanceStatistics`.
#[cfg(target_os = "macos")]
pub fn gpu_percent() -> Option<f64> {
    use std::ffi::{CStr, c_char, c_void};
    type CfRef = *const c_void;
    const MAIN_PORT_DEFAULT: u32 = 0;
    const UTF8: u32 = 0x0800_0100;
    const SINT64: isize = 4;
    const ACCELERATOR: &CStr = c"IOAccelerator";
    const STATISTICS: &CStr = c"PerformanceStatistics";
    const UTILIZATION: &CStr = c"Device Utilization %";
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> CfRef;
        fn IOServiceGetMatchingServices(port: u32, matching: CfRef, found: *mut u32) -> i32;
        fn IOIteratorNext(iterator: u32) -> u32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IORegistryEntryCreateCFProperty(entry: u32, key: CfRef, a: CfRef, o: u32) -> CfRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(a: CfRef, text: *const c_char, encoding: u32) -> CfRef;
        fn CFDictionaryGetValue(dict: CfRef, key: CfRef) -> CfRef;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFNumberGetValue(number: CfRef, kind: isize, value: *mut c_void) -> bool;
        fn CFNumberGetTypeID() -> usize;
        fn CFGetTypeID(object: CfRef) -> usize;
        fn CFRelease(object: CfRef);
    }
    // SAFETY: plain IOKit/CoreFoundation calls; every object created here is released here, and
    // values read out of a dictionary are only used while the dictionary is held.
    unsafe {
        let mut found = 0;
        let matching = IOServiceMatching(ACCELERATOR.as_ptr());
        // Takes ownership of `matching`.
        if matching.is_null()
            || IOServiceGetMatchingServices(MAIN_PORT_DEFAULT, matching, &mut found) != 0
        {
            return None;
        }
        let statistics = CFStringCreateWithCString(std::ptr::null(), STATISTICS.as_ptr(), UTF8);
        let utilization = CFStringCreateWithCString(std::ptr::null(), UTILIZATION.as_ptr(), UTF8);
        let mut busy = None;
        loop {
            let service = IOIteratorNext(found);
            if service == 0 {
                break;
            }
            let dict = IORegistryEntryCreateCFProperty(service, statistics, std::ptr::null(), 0);
            IOObjectRelease(service);
            if dict.is_null() {
                continue;
            }
            if busy.is_none() && CFGetTypeID(dict) == CFDictionaryGetTypeID() {
                let number = CFDictionaryGetValue(dict, utilization);
                let mut value: i64 = 0;
                if !number.is_null()
                    && CFGetTypeID(number) == CFNumberGetTypeID()
                    && CFNumberGetValue(number, SINT64, (&raw mut value).cast())
                {
                    busy = Some(value as f64);
                }
            }
            CFRelease(dict);
        }
        IOObjectRelease(found);
        for key in [statistics, utilization] {
            if !key.is_null() {
                CFRelease(key);
            }
        }
        busy
    }
}

/// Other hosts do not say how busy their GPU is in a way read cheaply here.
#[cfg(not(target_os = "macos"))]
pub fn gpu_percent() -> Option<f64> {
    None
}

#[cfg(windows)]
fn cpu_time() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: GetProcessTimes only writes the four FILETIMEs it is given, which live for the call.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Duration::ZERO;
    }
    // FILETIME counts 100 ns ticks.
    let ticks = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

#[cfg(windows)]
fn memory() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: zeroed counters are valid; the call writes at most `cb` bytes into them.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: as above; the handle is this process's own pseudo-handle.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.WorkingSetSize as u64)
}

#[cfg(unix)]
fn cpu_time() -> Duration {
    // SAFETY: `getrusage` only writes the `rusage` it is given, which lives for the call.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above; RUSAGE_SELF is always a valid target.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return Duration::ZERO;
    }
    let time = |tv: libc::timeval| {
        Duration::new(
            u64::try_from(tv.tv_sec).unwrap_or(0),
            u32::try_from(tv.tv_usec).unwrap_or(0) * 1_000,
        )
    };
    time(usage.ru_utime) + time(usage.ru_stime)
}

#[cfg(target_os = "macos")]
fn memory() -> Option<u64> {
    // SAFETY: `proc_pid_rusage` with RUSAGE_INFO_V4 writes one `rusage_info_v4` into the buffer
    // it is given, which lives for the call.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let buffer = std::ptr::addr_of_mut!(info).cast::<libc::rusage_info_t>();
    // SAFETY: as above; the pid is this process's own.
    let done =
        unsafe { libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V4, buffer) };
    (done == 0).then_some(info.ri_phys_footprint)
}

#[cfg(target_os = "linux")]
fn memory() -> Option<u64> {
    // `/proc/self/statm`: sizes in pages; the second is what is resident.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf only reads a system setting.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    Some(pages * page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_process_shows_cpu_and_memory() {
        let before = read();
        let spin = Instant::now();
        while spin.elapsed() < Duration::from_millis(50) {
            std::hint::black_box(0u64);
        }
        let after = read();
        let cpu = cpu_percent(&before, &after).unwrap();
        assert!(cpu > 0.0, "{cpu}");
        assert!(after.memory_bytes.unwrap() > 0);
    }

    #[test]
    fn one_percent_low_is_the_slowest_frames_and_skips_still_screens() {
        let mut gaps = FrameGaps::default();
        assert_eq!(gaps.one_percent_low(), None);
        let start = Instant::now();
        let mut at = start;
        // 198 frames 10 ms apart, two 50 ms stutters, and a still screen of 5 s.
        for index in 0..200 {
            at += Duration::from_millis(if index % 100 == 50 { 50 } else { 10 });
            gaps.frame(at);
        }
        at += Duration::from_secs(5);
        gaps.frame(at);
        let low = gaps.one_percent_low().unwrap();
        assert!((low - 20.0).abs() < 0.01, "{low}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gpu_busy_share_is_a_percentage() {
        if let Some(busy) = gpu_percent() {
            assert!((0.0..=100.0).contains(&busy), "{busy}");
        }
    }
}
