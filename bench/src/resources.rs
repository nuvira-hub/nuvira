//! Process resources (plan section 6.3): RSS, private/commit bytes, threads,
//! handles or open fds, UDP/TCP sockets and CPU time, of this process or of
//! another one by pid (two-process variants sample the agent binary).
//!
//! Per OS:
//! - Windows: `GetProcessMemoryInfo` (WorkingSetSize, PrivateUsage),
//!   `NtQuerySystemInformation` (thread count), `GetProcessHandleCount`, `GetExtendedUdpTable` /
//!   `GetExtendedTcpTable` filtered by pid (IPv4 + IPv6), `GetProcessTimes`.
//! - Linux: `/proc/<pid>/status` (VmRSS, RssAnon, Threads), `/proc/<pid>/fd` (socket
//!   inodes matched against `/proc/<pid>/net/{udp,udp6,tcp,tcp6}`), `/proc/<pid>/stat`.
//! - macOS: `proc_pidinfo(PROC_PIDTASKINFO)`, `proc_pid_rusage` (phys_footprint as
//!   private), `proc_pidinfo(PROC_PIDLISTFDS)` (sockets counted, not split by
//!   protocol: `udp_sockets`/`tcp_sockets` stay None).
//!
//! A field the OS cannot give is None, never 0.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// One sample of one process.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcSample {
    pub pid: u32,
    /// Milliseconds since 1970 (UTC) when the sample was taken.
    pub unix_ms: u64,
    /// Resident set / working set.
    pub rss_bytes: u64,
    /// Private bytes: Windows PrivateUsage (commit), Linux RssAnon, macOS phys_footprint.
    pub private_bytes: Option<u64>,
    pub threads: Option<u32>,
    /// Windows handle count.
    pub handles: Option<u32>,
    /// Unix open file descriptors.
    pub fds: Option<u32>,
    /// All sockets (Unix: socket fds; Windows: UDP + TCP endpoints).
    pub sockets: Option<u32>,
    pub udp_sockets: Option<u32>,
    pub tcp_sockets: Option<u32>,
    pub cpu_user_ms: f64,
    pub cpu_kernel_ms: f64,
}

impl ProcSample {
    /// This process.
    pub fn of_self() -> ProcSample {
        Self::of_pid(std::process::id()).unwrap_or_else(|_| ProcSample {
            pid: std::process::id(),
            unix_ms: unix_ms(),
            ..Default::default()
        })
    }

    /// Another process (or this one) by pid.
    pub fn of_pid(pid: u32) -> Result<ProcSample> {
        let mut s = ProcSample { pid, unix_ms: unix_ms(), ..Default::default() };
        imp::fill(pid, &mut s)?;
        Ok(s)
    }

    pub fn cpu_total_ms(&self) -> f64 {
        self.cpu_user_ms + self.cpu_kernel_ms
    }

    /// Handles on Windows, fds on Unix: the "open things" count used by leak gates.
    pub fn open_objects(&self) -> Option<u32> {
        self.handles.or(self.fds)
    }
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(windows)]
mod imp {
    use super::ProcSample;
    use anyhow::{Context, Result};
    use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        MIB_UDP6TABLE_OWNER_PID, MIB_UDPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessHandleCount, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    struct Proc {
        h: HANDLE,
        own: bool,
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            if self.own {
                // SAFETY: handle opened by OpenProcess below.
                unsafe {
                    let _ = CloseHandle(self.h);
                }
            }
        }
    }

    pub fn fill(pid: u32, s: &mut ProcSample) -> Result<()> {
        let p = if pid == std::process::id() {
            // SAFETY: pseudo handle, never closed.
            Proc { h: unsafe { GetCurrentProcess() }, own: false }
        } else {
            // SAFETY: plain call; the handle is closed by Proc's Drop.
            let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, false, pid) }
                .with_context(|| format!("OpenProcess {pid}"))?;
            Proc { h, own: true }
        };
        let mut mc = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: the _EX struct starts with PROCESS_MEMORY_COUNTERS; cb gives its size.
        unsafe {
            GetProcessMemoryInfo(p.h, &mut mc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, mc.cb)
                .context("GetProcessMemoryInfo")?;
        }
        s.rss_bytes = mc.WorkingSetSize as u64;
        s.private_bytes = Some(mc.PrivateUsage as u64);
        let mut handles = 0u32;
        // SAFETY: valid out pointer.
        if unsafe { GetProcessHandleCount(p.h, &mut handles) }.is_ok() {
            s.handles = Some(handles);
        }
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: valid out pointers.
        if unsafe { GetProcessTimes(p.h, &mut c, &mut e, &mut k, &mut u) }.is_ok() {
            s.cpu_kernel_ms = crate::measure::filetime(k).as_secs_f64() * 1e3;
            s.cpu_user_ms = crate::measure::filetime(u).as_secs_f64() * 1e3;
        }
        s.threads = threads(pid);
        let udp = udp(pid);
        let tcp = tcp(pid);
        s.udp_sockets = udp;
        s.tcp_sockets = tcp;
        s.sockets = match (udp, tcp) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        Ok(())
    }

    /// Thread count from one `NtQuerySystemInformation(SystemProcessInformation)`
    /// (a Toolhelp32 thread snapshot took ~65 ms; this takes a few).
    fn threads(pid: u32) -> Option<u32> {
        let mut size = 1u32 << 20;
        for _ in 0..6 {
            let mut buf = vec![0u64; size as usize / 8];
            let mut needed = 0u32;
            // SAFETY: buffer of `size` bytes, 8-aligned; entries are walked by their
            // NextEntryOffset inside the returned length.
            let status = unsafe {
                NtQuerySystemInformation(SystemProcessInformation, buf.as_mut_ptr().cast(), size, &mut needed)
            };
            if status.0 == STATUS_INFO_LENGTH_MISMATCH {
                size = needed.max(size) + (256 << 10);
                continue;
            }
            if status.0 < 0 {
                return None;
            }
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            loop {
                // SAFETY: `off` stays inside the filled part of the buffer.
                let e = unsafe { &*(base.add(off) as *const SYSTEM_PROCESS_INFORMATION) };
                if e.UniqueProcessId.0 as usize == pid as usize {
                    return Some(e.NumberOfThreads);
                }
                if e.NextEntryOffset == 0 {
                    return None;
                }
                off += e.NextEntryOffset as usize;
            }
        }
        None
    }

    /// Reads an IP Helper table into a u64-aligned buffer (size asked first).
    fn table(get: impl Fn(Option<*mut core::ffi::c_void>, *mut u32) -> u32) -> Option<Vec<u64>> {
        let mut size = 0u32;
        let _ = get(None, &mut size);
        for _ in 0..4 {
            let mut buf = vec![0u64; (size as usize).div_ceil(8) + 64];
            let mut len = (buf.len() * 8) as u32;
            let r = get(Some(buf.as_mut_ptr().cast()), &mut len);
            if r == 0 {
                return Some(buf);
            }
            size = len;
        }
        None
    }

    fn udp(pid: u32) -> Option<u32> {
        let get4 =
            |p, n| unsafe { GetExtendedUdpTable(p, n, false, AF_INET.0 as u32, UDP_TABLE_OWNER_PID, 0) };
        let get6 =
            |p, n| unsafe { GetExtendedUdpTable(p, n, false, AF_INET6.0 as u32, UDP_TABLE_OWNER_PID, 0) };
        let b4 = table(get4)?;
        let b6 = table(get6)?;
        // SAFETY: buffers filled by GetExtendedUdpTable with these table layouts.
        unsafe {
            let t4 = &*(b4.as_ptr() as *const MIB_UDPTABLE_OWNER_PID);
            let r4 = std::slice::from_raw_parts(t4.table.as_ptr(), t4.dwNumEntries as usize);
            let t6 = &*(b6.as_ptr() as *const MIB_UDP6TABLE_OWNER_PID);
            let r6 = std::slice::from_raw_parts(t6.table.as_ptr(), t6.dwNumEntries as usize);
            Some(
                (r4.iter().filter(|r| r.dwOwningPid == pid).count()
                    + r6.iter().filter(|r| r.dwOwningPid == pid).count()) as u32,
            )
        }
    }

    fn tcp(pid: u32) -> Option<u32> {
        let get4 =
            |p, n| unsafe { GetExtendedTcpTable(p, n, false, AF_INET.0 as u32, TCP_TABLE_OWNER_PID_ALL, 0) };
        let get6 =
            |p, n| unsafe { GetExtendedTcpTable(p, n, false, AF_INET6.0 as u32, TCP_TABLE_OWNER_PID_ALL, 0) };
        let b4 = table(get4)?;
        let b6 = table(get6)?;
        // SAFETY: buffers filled by GetExtendedTcpTable with these table layouts.
        unsafe {
            let t4 = &*(b4.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
            let r4 = std::slice::from_raw_parts(t4.table.as_ptr(), t4.dwNumEntries as usize);
            let t6 = &*(b6.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID);
            let r6 = std::slice::from_raw_parts(t6.table.as_ptr(), t6.dwNumEntries as usize);
            Some(
                (r4.iter().filter(|r| r.dwOwningPid == pid).count()
                    + r6.iter().filter(|r| r.dwOwningPid == pid).count()) as u32,
            )
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::ProcSample;
    use anyhow::{Context, Result};
    use std::collections::HashSet;

    pub fn fill(pid: u32, s: &mut ProcSample) -> Result<()> {
        let base = format!("/proc/{pid}");
        let status = std::fs::read_to_string(format!("{base}/status")).context("/proc status")?;
        let kb = |key: &str| -> Option<u64> {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        };
        s.rss_bytes = kb("VmRSS:").unwrap_or(0) * 1024;
        s.private_bytes = kb("RssAnon:").map(|v| v * 1024);
        s.threads = kb("Threads:").map(|v| v as u32);
        let mut inodes = HashSet::new();
        let mut fds = 0u32;
        if let Ok(dir) = std::fs::read_dir(format!("{base}/fd")) {
            for e in dir.flatten() {
                fds += 1;
                if let Ok(link) = std::fs::read_link(e.path())
                    && let Some(rest) = link.to_str().and_then(|l| l.strip_prefix("socket:["))
                    && let Ok(inode) = rest.trim_end_matches(']').parse::<u64>()
                {
                    inodes.insert(inode);
                }
            }
            s.fds = Some(fds);
            s.sockets = Some(inodes.len() as u32);
            let count = |files: &[&str]| -> u32 {
                let mut n = 0;
                for f in files {
                    if let Ok(t) = std::fs::read_to_string(format!("{base}/net/{f}")) {
                        for l in t.lines().skip(1) {
                            if let Some(i) = l.split_whitespace().nth(9).and_then(|v| v.parse::<u64>().ok())
                                && inodes.contains(&i)
                            {
                                n += 1;
                            }
                        }
                    }
                }
                n
            };
            s.udp_sockets = Some(count(&["udp", "udp6"]));
            s.tcp_sockets = Some(count(&["tcp", "tcp6"]));
        }
        if let Ok(stat) = std::fs::read_to_string(format!("{base}/stat"))
            && let Some(after) = stat.rfind(')').map(|i| &stat[i + 1..])
        {
            // Fields after the command: state is field 3; utime 14, stime 15.
            let f: Vec<&str> = after.split_whitespace().collect();
            // SAFETY: sysconf has no preconditions.
            let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
            if f.len() > 12 {
                s.cpu_user_ms = f[11].parse::<f64>().unwrap_or(0.0) * 1e3 / tck;
                s.cpu_kernel_ms = f[12].parse::<f64>().unwrap_or(0.0) * 1e3 / tck;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ProcSample;
    use anyhow::{Result, bail};

    // mach_timebase_info: deprecated in libc in favour of the mach2 crate, which is
    // not in the root lock; the function itself is stable.
    #[allow(deprecated)]
    pub fn fill(pid: u32, s: &mut ProcSample) -> Result<()> {
        let ipid = pid as libc::c_int;
        // SAFETY: buffers sized as the flavours require.
        unsafe {
            let mut ti: libc::proc_taskinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
            let r = libc::proc_pidinfo(
                ipid,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut ti as *mut libc::proc_taskinfo).cast(),
                size,
            );
            if r != size {
                bail!("proc_pidinfo(PROC_PIDTASKINFO) failed for {pid}");
            }
            s.rss_bytes = ti.pti_resident_size;
            s.threads = Some(ti.pti_threadnum.max(0) as u32);
            // Task times are in Mach absolute time units (not ns on Apple Silicon).
            let mut tb = libc::mach_timebase_info { numer: 1, denom: 1 };
            libc::mach_timebase_info(&mut tb);
            let to_ms = |t: u64| t as f64 * tb.numer as f64 / tb.denom.max(1) as f64 / 1e6;
            s.cpu_user_ms = to_ms(ti.pti_total_user);
            s.cpu_kernel_ms = to_ms(ti.pti_total_system);

            let mut ru: libc::rusage_info_v2 = std::mem::zeroed();
            if libc::proc_pid_rusage(
                ipid,
                libc::RUSAGE_INFO_V2,
                (&mut ru as *mut libc::rusage_info_v2).cast(),
            ) == 0
            {
                s.private_bytes = Some(ru.ri_phys_footprint);
            }

            let entry = std::mem::size_of::<libc::proc_fdinfo>();
            let need = libc::proc_pidinfo(ipid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
            if need > 0 {
                let cap = need as usize / entry + 32;
                let mut buf: Vec<libc::proc_fdinfo> = vec![std::mem::zeroed(); cap];
                let got = libc::proc_pidinfo(
                    ipid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    buf.as_mut_ptr().cast(),
                    (cap * entry) as libc::c_int,
                );
                if got > 0 {
                    let n = got as usize / entry;
                    s.fds = Some(n as u32);
                    s.sockets = Some(
                        buf[..n].iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32).count()
                            as u32,
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use super::ProcSample;
    pub fn fill(_pid: u32, _s: &mut ProcSample) -> anyhow::Result<()> {
        anyhow::bail!("process sampling not implemented on this OS")
    }
}

/// Samples taken every `interval` on a background thread until stopped (long
/// sessions, plan 7.3: every 5 s).
pub struct Sampler {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<Vec<ProcSample>>>,
}

impl Sampler {
    pub fn start(pid: u32, interval: std::time::Duration) -> Sampler {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("bench-res-sampler".into())
            .spawn(move || {
                let mut out = Vec::new();
                let mut next = std::time::Instant::now();
                while !flag.load(Ordering::Relaxed) {
                    if let Ok(s) = ProcSample::of_pid(pid) {
                        out.push(s);
                    }
                    next += interval;
                    while !flag.load(Ordering::Relaxed) && std::time::Instant::now() < next {
                        std::thread::sleep(std::time::Duration::from_millis(50).min(interval));
                    }
                }
                out
            })
            .expect("sampler thread");
        Sampler { stop, thread: Some(thread) }
    }

    /// Stops and returns every sample.
    pub fn finish(mut self) -> Vec<ProcSample> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().map(|t| t.join().unwrap_or_default()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_this_process() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let _ = ProcSample::of_pid(std::process::id()).unwrap();
        // A sample must be cheap: it runs inside measured processes every 5 s.
        let t = std::time::Instant::now();
        let s = ProcSample::of_pid(std::process::id()).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_millis(50), "sample took {:?}", t.elapsed());
        assert!(s.rss_bytes > 0);
        assert!(s.threads.unwrap_or(1) >= 1);
        assert!(s.open_objects().unwrap_or(1) > 0);
        #[cfg(any(windows, target_os = "linux"))]
        assert!(s.udp_sockets.unwrap() >= 1, "{s:?}");
        drop(sock);
        let busy = std::time::Instant::now();
        while busy.elapsed() < std::time::Duration::from_millis(50) {
            std::hint::spin_loop();
        }
        let s2 = ProcSample::of_self();
        assert!(s2.cpu_total_ms() >= s.cpu_total_ms());
    }

    #[test]
    fn background_sampler() {
        let s = Sampler::start(std::process::id(), std::time::Duration::from_millis(20));
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let v = s.finish();
        assert!(v.len() >= 2, "{}", v.len());
    }
}
