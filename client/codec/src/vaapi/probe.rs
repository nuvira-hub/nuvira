//! The driver test before the first encoder: a child process (fork) opens the
//! driver and encodes, since some drivers abort instead of failing.

use anyhow::{Result, anyhow};
use tracing::warn;

use super::VaapiEncoder;
use crate::EncoderSettings;

/// Set in the child of [`driver_survives`]: it tries the driver itself.
static IN_PROBE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the driver opens and encodes without killing the process: the first
/// time, a child process (fork) does what [`VaapiEncoder::new`] does. Some drivers
/// abort instead of failing: i965 2.4.1 on an Intel Braswell (Celeron N3xxx),
/// `intel_enc_hw_context_init: Assertion 'encoder_context->mfc_context' failed` in
/// vaCreateContext, also with ffmpeg (2026-11-12); without this the agent died at
/// its start. A driver that only fails is left to the attempt in this process,
/// which tells why; one that crashes is not used again in this process. One that
/// does not answer within 15 s is tried once more at the next encoder (the child
/// of a multi-threaded process can block on a lock another thread held at the
/// fork), and then not used again either.
///
/// The child takes as few locks as it can: logging is switched off for it with
/// `Dispatch::none()` (no allocation, no registration in tracing's global lists),
/// so the process's subscriber and its writer (a `Mutex<File>` in the app) are
/// never entered there.
///
/// A driver that kills the child leaves no crash report: the child catches the
/// fatal signals and leaves with an exit code ([`quiet_crashes`]). Before, with
/// i965 on an Intel Haswell under Ubuntu 26.10 (`slice_header: Assertion '0'
/// failed`, 2026-11-25), every start of the app showed Ubuntu's "internal
/// error" window for the expected crash of the child.
pub(super) fn driver_survives(settings: EncoderSettings) -> Result<()> {
    driver_survives_within(settings, std::time::Duration::from_secs(15))
}

/// What [`driver_survives_within`] found so far in this process.
enum Probe {
    Untried,
    TimedOutOnce,
    Done(std::result::Result<(), String>),
}

static PROBE: std::sync::Mutex<Probe> = std::sync::Mutex::new(Probe::Untried);

fn driver_survives_within(settings: EncoderSettings, limit: std::time::Duration) -> Result<()> {
    if IN_PROBE.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let mut probe = PROBE.lock().unwrap_or_else(|e| e.into_inner());
    if let Probe::Done(verdict) = &*probe {
        return verdict.clone().map_err(|e| anyhow!(e));
    }
    let none = tracing::Dispatch::none();
    let verdict = try_in_child(limit, || {
        IN_PROBE.store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::dispatcher::with_default(&none, || VaapiEncoder::new(settings).is_ok())
    });
    let timed_out = verdict.as_ref().is_err_and(|e| e.contains("did not answer"));
    *probe = match &*probe {
        Probe::Untried if timed_out => {
            warn!("VA-API: the driver test did not answer; tried again at the next encoder");
            Probe::TimedOutOnce
        }
        _ => Probe::Done(verdict.clone()),
    };
    verdict.map_err(|e| anyhow!(e))
}

/// Exit code of the child of [`try_in_child`] that a fatal signal ended: this
/// plus the signal.
const PROBE_CRASHED: i32 = 100;

extern "C" fn probe_crashed(sig: libc::c_int) {
    // SAFETY: _exit is async-signal-safe and runs no handlers of the parent.
    unsafe { libc::_exit(PROBE_CRASHED + sig) }
}

/// In the child: a crash ends it with an exit code instead of a signal, so the
/// kernel makes no core dump and no crash report is written for it (apport on
/// Ubuntu, systemd-coredump on Fedora). Not dumpable and no core either, for
/// what the handlers cannot catch.
///
/// # Safety
/// Only in the child of a fork, before the test.
unsafe fn quiet_crashes() {
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        let handler = probe_crashed as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for sig in [libc::SIGABRT, libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE] {
            libc::signal(sig, handler);
        }
    }
}

/// Runs test in a child process (fork) and tells whether the child survived:
/// Ok also when test returned false (a failure is left to the caller, which
/// tells why); Err when the child crashed or did not answer within limit.
fn try_in_child(limit: std::time::Duration, test: impl FnOnce() -> bool) -> std::result::Result<(), String> {
    use std::time::Instant;
    // SAFETY: the child only runs the test and leaves with _exit (no
    // destructors, no atexit handlers of the parent).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Ok(()); // no child: tried in this process
    }
    if pid == 0 {
        // SAFETY: in the child, before the test.
        unsafe { quiet_crashes() };
        let ok = test();
        unsafe { libc::_exit(if ok { 0 } else { 1 }) }
    }
    let started = Instant::now();
    let mut status = 0;
    loop {
        // SAFETY: waiting for our own child.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            break;
        }
        if r < 0 {
            return Ok(());
        }
        if started.elapsed() > limit {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return Err(format!("the VA-API driver did not answer within {} s", limit.as_secs()));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let signal = if libc::WIFSIGNALED(status) {
        Some(libc::WTERMSIG(status))
    } else if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) > PROBE_CRASHED {
        Some(libc::WEXITSTATUS(status) - PROBE_CRASHED)
    } else {
        None
    };
    match signal {
        Some(sig) => Err(format!("the VA-API driver crashed while trying it (signal {sig})")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use tracing::info;

    use super::{driver_survives, try_in_child};
    use crate::EncoderSettings;

    /// The driver test forks while other threads load and unload libraries and
    /// log through a writer whose lock they hold most of the time (as the app's
    /// `Mutex<File>`): the child must not block on those locks (VID-6). Each
    /// attempt in a fresh process (the verdict is kept per process), 50 of them
    /// (`NUVIRA_TEST_PROBE_RUNS`). A driver that fails or crashes is fine here;
    /// only "did not answer" is not.
    #[test]
    fn probe_with_busy_threads() {
        if std::env::var_os("NUVIRA_TEST_PROBE_CHILD").is_some() {
            struct Slow;
            impl std::io::Write for Slow {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    std::thread::sleep(std::time::Duration::from_micros(500));
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let _ = tracing_subscriber::fmt().with_writer(std::sync::Mutex::new(Slow)).try_init();
            for t in 0..4 {
                std::thread::spawn(move || {
                    let mut i = 0u64;
                    loop {
                        // SAFETY: the C library's maths library, loaded and unloaded.
                        drop(unsafe { libloading::Library::new("libm.so.6") });
                        info!(t, i, "busy");
                        i += 1;
                    }
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
            let started = std::time::Instant::now();
            let verdict = driver_survives(EncoderSettings::default());
            eprintln!("verdict after {:?}: {verdict:?}", started.elapsed());
            if let Err(e) = verdict {
                assert!(!format!("{e:#}").contains("did not answer"), "{e:#}");
            }
            return;
        }
        let runs: u32 =
            std::env::var("NUVIRA_TEST_PROBE_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
        let exe = std::env::current_exe().unwrap();
        // This test's name as libtest knows it: its path without the crate's name.
        let path = concat!(module_path!(), "::probe_with_busy_threads");
        let name = path.split_once("::").map_or(path, |(_, name)| name);
        let started = std::time::Instant::now();
        for run in 0..runs {
            let out = std::process::Command::new(&exe)
                .args([name, "--exact", "--nocapture", "--test-threads=1"])
                .env("NUVIRA_TEST_PROBE_CHILD", "1")
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "run {run}: {}\n{log}", out.status);
            if run == 0 {
                eprintln!("{}", log.lines().find(|l| l.starts_with("verdict")).unwrap_or("no verdict line"));
            }
        }
        eprintln!("{runs} fresh processes in {:?}", started.elapsed());
    }

    /// A child that the driver kills is a crash, told by its exit code, and the
    /// kernel makes no core of it (no crash report); one that only fails or
    /// hangs is told as before.
    #[test]
    fn child_crash_is_quiet() {
        let limit = std::time::Duration::from_secs(10);
        assert_eq!(try_in_child(limit, || true), Ok(()));
        assert_eq!(try_in_child(limit, || false), Ok(()));
        for (sig, crash) in [
            (libc::SIGABRT, (|| unsafe { libc::abort() }) as fn() -> bool),
            (libc::SIGSEGV, || unsafe { libc::raise(libc::SIGSEGV) != 0 }),
        ] {
            let e = try_in_child(limit, crash).unwrap_err();
            assert_eq!(e, format!("the VA-API driver crashed while trying it (signal {sig})"));
        }
        let e = try_in_child(std::time::Duration::from_millis(300), || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            true
        })
        .unwrap_err();
        assert!(e.contains("did not answer"), "{e}");
        // The handlers are only in the child.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe { libc::sigaction(libc::SIGABRT, std::ptr::null(), &mut old) };
        assert_eq!(old.sa_sigaction, libc::SIG_DFL);
    }
}
