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
}
