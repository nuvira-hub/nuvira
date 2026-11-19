//! The display of the controlled PC stays on during a session (Windows).
//!
//! With the monitor off for power saving some machines compose nothing and DXGI
//! gives no picture at all (PC-LABORATORIO, review VID-10): an unattended session
//! stayed black until the technician happened to move the mouse. Decided by the
//! user on 2026-11-18, like macOS does (`nuvira_capture::macos`): while the
//! session's capture is open ([`wrap`]) a power request keeps the display on
//! ([`nuvira_capture::awake::DisplayAwake`]), and a display that is off anyway is
//! woken with an injected one-pixel move and back ([`nuvira_input::wake_display`]):
//! when no picture has come for [`FIRST_WAIT`], or when Windows says the display
//! went off (the lock screen turns it off after its own timeout). Not in a Remote
//! Desktop session, which has no monitor of its own.
//!
//! The wrapper lives on the capture thread and is dropped with the capture, so the
//! request ends with the session (or when the screen is switched, and a new one
//! starts with the new capture).

use std::time::{Duration, Instant};

/// No picture for this long after the capture opened (or lost its picture, at a
/// desktop switch): the display is woken. A display that is on gives the first
/// picture at once.
pub(crate) const FIRST_WAIT: Duration = Duration::from_millis(250);
/// Wake-ups at most this often (the monitor needs one or two seconds to come back
/// and DWM to compose again)...
pub(crate) const WAKE_EVERY: Duration = Duration::from_secs(3);
/// ...the first few times; then, while still no picture comes or the display
/// stays off (a capture that fails for other reasons, a closed laptop lid), only
/// this often.
pub(crate) const WAKE_SLOW: Duration = Duration::from_secs(30);
pub(crate) const FAST_TRIES: u32 = 5;

/// When to wake the display: pure, so it can be tested on every system.
#[derive(Debug, Default)]
pub(crate) struct WakePolicy {
    /// Since when the capture has had no picture.
    no_picture_since: Option<Instant>,
    last_wake: Option<Instant>,
    /// Wake-ups since things were last fine (a picture, the display on).
    tries: u32,
}

impl WakePolicy {
    /// Called before every capture: whether to wake the display now.
    pub(crate) fn due(&mut self, now: Instant, have_picture: bool, display_off: bool) -> bool {
        if have_picture {
            self.no_picture_since = None;
            if !display_off {
                self.tries = 0;
            }
        }
        let since = if have_picture { None } else { Some(*self.no_picture_since.get_or_insert(now)) };
        let waiting = since.is_some_and(|t| now.duration_since(t) >= FIRST_WAIT);
        if !waiting && !display_off {
            return false;
        }
        let every = if self.tries >= FAST_TRIES { WAKE_SLOW } else { WAKE_EVERY };
        if self.last_wake.is_some_and(|t| now.duration_since(t) < every) {
            return false;
        }
        self.last_wake = Some(now);
        self.tries = self.tries.saturating_add(1);
        true
    }

    pub(crate) fn tries(&self) -> u32 {
        self.tries
    }
}

#[cfg(windows)]
pub(crate) use windows_impl::wrap;

#[cfg(windows)]
mod windows_impl {
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use nuvira_capture::awake::{DisplayAwake, display_off, remote_session};
    use nuvira_capture::{Capturer, Display, Frame};
    use tracing::{debug, info};

    use super::WakePolicy;

    /// A session's capture that keeps the display on.
    struct Awake {
        inner: Box<dyn Capturer>,
        _request: Option<DisplayAwake>,
        policy: WakePolicy,
        /// Remote Desktop session: nothing to wake.
        remote: bool,
    }

    /// The capture of a real screen for a session, with the display kept on.
    pub(crate) fn wrap(inner: Box<dyn Capturer>) -> Box<dyn Capturer> {
        let remote = remote_session();
        // Registers for the display state now, so that it is known by the first capture.
        let off = display_off();
        info!(remote, off, "display kept on during the session");
        Box::new(Awake { inner, _request: DisplayAwake::new(), policy: WakePolicy::default(), remote })
    }

    impl Awake {
        fn wake_if_needed(&mut self) {
            if self.remote {
                return;
            }
            let have_picture = self.inner.last_frame().is_some();
            let off = display_off();
            if !self.policy.due(Instant::now(), have_picture, off) {
                return;
            }
            // The lock screen and UAC are another desktop: the move must go there.
            if let Some(name) = nuvira_capture::desktop::follow_input_desktop() {
                debug!("display wake-up on desktop {name}");
            }
            nuvira_input::wake_display();
            let tries = self.policy.tries();
            if tries <= 1 || off {
                info!(display_off = off, have_picture, "waking the display");
            } else {
                debug!(tries, "waking the display again: still no picture");
            }
        }
    }

    impl Capturer for Awake {
        fn display(&self) -> &Display {
            self.inner.display()
        }

        fn next_frame(&mut self, timeout: Duration) -> Result<Option<&Frame>> {
            self.wake_if_needed();
            self.inner.next_frame(timeout)
        }

        fn last_frame(&self) -> Option<&Frame> {
            self.inner.last_frame()
        }

        fn enable_gpu_output(&mut self) -> Option<::windows::Win32::Graphics::Direct3D11::ID3D11Device> {
            self.inner.enable_gpu_output()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_only_when_no_picture_comes_or_the_display_is_off() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = WakePolicy::default();
        // A display that is on: the picture comes at once, never woken.
        assert!(!p.due(at(0), false, false));
        assert!(!p.due(at(100), true, false));
        assert!(!p.due(at(10_000), true, false));
        assert_eq!(p.tries(), 0);

        // No picture: woken after FIRST_WAIT, then every WAKE_EVERY.
        let mut p = WakePolicy::default();
        assert!(!p.due(at(0), false, false));
        assert!(!p.due(at(200), false, false));
        assert!(p.due(at(250), false, false));
        assert!(!p.due(at(1_000), false, false));
        assert!(!p.due(at(3_000), false, false));
        assert!(p.due(at(3_250), false, false));
        // The picture came: nothing more, and the count starts again.
        assert!(!p.due(at(4_000), true, false));
        assert_eq!(p.tries(), 0);
        // The picture is lost (desktop switch): FIRST_WAIT again from then.
        assert!(!p.due(at(20_000), false, false));
        assert!(!p.due(at(20_200), false, false));
        assert!(p.due(at(20_300), false, false));
    }

    #[test]
    fn a_display_turned_off_is_woken_even_with_a_picture() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = WakePolicy::default();
        assert!(!p.due(at(0), true, false));
        // Off (the lock screen's own timeout): woken at once, then every WAKE_EVERY
        // while Windows still says off.
        assert!(p.due(at(60_000), true, true));
        assert!(!p.due(at(61_000), true, true));
        assert!(p.due(at(63_000), true, true));
        assert!(!p.due(at(64_000), true, false));
        // A display that stays off whatever is done (a closed laptop lid): the
        // fast tries, then every WAKE_SLOW.
        let mut p = WakePolicy::default();
        let wakes = (0..120_000).step_by(100).filter(|&ms| p.due(at(ms), true, true)).count();
        assert_eq!(wakes, FAST_TRIES as usize + 3, "0, 3, 6, 9, 12 s, then 42, 72, 102 s");
    }

    #[test]
    fn a_capture_that_never_gives_a_picture_is_woken_rarely_after_a_few_tries() {
        let t0 = Instant::now();
        let mut p = WakePolicy::default();
        let mut wakes = Vec::new();
        for ms in (0..120_000).step_by(100) {
            if p.due(t0 + Duration::from_millis(ms), false, false) {
                wakes.push(ms);
            }
        }
        // 250, 3250, ... five fast tries, then every 30 s.
        assert_eq!(&wakes[..FAST_TRIES as usize], &[300, 3300, 6300, 9300, 12300]);
        assert_eq!(wakes[FAST_TRIES as usize], 42_300);
        assert!(wakes.len() <= FAST_TRIES as usize + 4, "{wakes:?}");
    }

    /// Session-level check of VID-10 (pinned for review lot 13) on a real desktop
    /// (`NUVIRA_TEST_SCREEN=1`, run in the signed-in session): the monitor is
    /// turned off as the power plan does (`SC_MONITORPOWER` 2), then the session's
    /// capture ([`wrap`] around the real capturer) and the software encoder start,
    /// as a session does. The first encoded frame must come within 3 s.
    /// PC-LABORATORIO gives no DXGI frame at all with the display off (K41 keeps
    /// pinning the bare capturer). `NUVIRA_TEST_BARE=1` runs the same without
    /// [`wrap`], to show the old behaviour (no frame in 10 s there).
    #[cfg(windows)]
    #[test]
    fn a_session_gets_its_first_frame_with_the_display_off() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            HWND_BROADCAST, PostMessageW, SC_MONITORPOWER, WM_SYSCOMMAND,
        };

        use crate::pipeline::{Controls, Pipeline};

        if std::env::var("NUVIRA_TEST_SCREEN").as_deref() != Ok("1") {
            eprintln!(
                "NOT RUN (needs a real desktop session whose screen the test may turn off): set NUVIRA_TEST_SCREEN=1"
            );
            return;
        }
        let bare = std::env::var("NUVIRA_TEST_BARE").as_deref() == Ok("1");
        let _ =
            tracing_subscriber::fmt().with_env_filter("nuvira_agent=debug,nuvira_capture=debug").try_init();
        nuvira_input::set_dpi_awareness();
        // Follows the display state from now on.
        let _ = nuvira_capture::awake::display_off();
        // Monitor off (2), as the power plan does after its timeout.
        unsafe {
            let _ = PostMessageW(
                Some(HWND_BROADCAST),
                WM_SYSCOMMAND,
                WPARAM(SC_MONITORPOWER as usize),
                LPARAM(2),
            );
        }
        std::thread::sleep(Duration::from_secs(4));
        let off_before = nuvira_capture::awake::display_off();

        let started = Instant::now();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (_, pipeline) = Pipeline::start(
            move || {
                let capturer = nuvira_capture::open(None)?;
                Ok(if bare { capturer } else { wrap(capturer) })
            },
            |gpu| nuvira_codec::create_encoder(nuvira_codec::EncoderPlan::Software, Default::default(), gpu),
            false,
            tx,
            Controls {
                max_fps: Arc::new(AtomicU32::new(30)),
                keyframe: Arc::new(AtomicBool::new(true)),
                bitrate: Arc::new(AtomicU32::new(2_000_000)),
                view: Arc::new(AtomicU64::new(0)),
                stats: None,
            },
        )
        .expect("start the session's capture");
        let limit = Duration::from_secs(if bare { 10 } else { 3 });
        let first = loop {
            match rx.try_recv() {
                Ok(frame) => break Some((started.elapsed(), frame.keyframe)),
                Err(_) if started.elapsed() >= limit => break None,
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let off_after = nuvira_capture::awake::display_off();
        drop(pipeline);
        if bare {
            // Wakes the display for whatever runs next.
            nuvira_input::wake_display();
        }
        eprintln!(
            "RESULT display-awake bare={bare} display_off_before={off_before} first_frame={:?} display_off_after={off_after}",
            first.map(|(t, key)| (t.as_millis(), key))
        );
        if bare {
            return;
        }
        let (after, keyframe) = first.expect("no frame within 3 s with the display off");
        assert!(keyframe, "the first frame is a keyframe");
        assert!(after < limit, "first frame after {after:?}");
        assert!(!off_after, "the display is on again");
    }
}
