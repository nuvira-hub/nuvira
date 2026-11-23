//! A viewer that opens again by itself a session it lost: the web page back from
//! the background (iOS suspends a page it does not show, Chrome for Android stops
//! one hidden for about a minute). One rule (review Phase 7, MT-P2-11 F2 case 1
//! and MT-P2-12 F2):
//!
//! - [`Ends`]: how each viewer's last session here ended. A viewer that says in its
//!   `Hello` that it is reopening (`Hello.reopen`) a session that ended normally
//!   (the person at the PC ended it, or the agent did) is refused before its
//!   password with `@agent.ended_at_computer`: the end never reached the page
//!   while it was not running, and the person at the PC must not see the session
//!   come back by itself. After a lost connection, or with no record (the agent
//!   restarted since: a known limit), it opens as before.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Records older than this are forgotten.
const KEEP: Duration = Duration::from_secs(24 * 3600);
/// At most this many viewers are remembered (the oldest go first).
const MAX_VIEWERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Ended on purpose: by the person at the PC, the agent (a time limit, its
    /// rules) or the viewer itself.
    Normal,
    /// The connection was lost (nothing received for 10 s, an idle timeout, a reset).
    Lost,
}

impl Ended {
    /// How a session that gave `result` ended (after [`crate::normal_end`]): `Ok`
    /// and an "@key" of the agent are a normal end, any other error a loss.
    pub fn of(result: &anyhow::Result<()>) -> Self {
        match result {
            Ok(()) => Ended::Normal,
            Err(e) if e.chain().any(|c| c.to_string().starts_with('@')) => Ended::Normal,
            Err(_) => Ended::Lost,
        }
    }
}

/// How each viewer's last session here ended.
#[derive(Default)]
pub struct Ends {
    map: Mutex<HashMap<String, (Ended, Instant)>>,
}

impl Ends {
    pub fn record(&self, viewer: &str, ended: Ended) {
        self.record_at(viewer, ended, Instant::now());
    }

    fn record_at(&self, viewer: &str, ended: Ended, at: Instant) {
        let mut map = self.map.lock().expect("ends");
        map.retain(|_, (_, t)| at.saturating_duration_since(*t) < KEEP);
        map.insert(viewer.to_string(), (ended, at));
        while map.len() > MAX_VIEWERS {
            let Some(oldest) = map.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone()) else { break };
            map.remove(&oldest);
        }
    }

    /// Whether an automatic reopen from `viewer` is refused: its last session here
    /// ended normally, less than a day ago.
    pub fn refuses_reopen(&self, viewer: &str) -> bool {
        self.refuses_reopen_at(viewer, Instant::now())
    }

    fn refuses_reopen_at(&self, viewer: &str, now: Instant) -> bool {
        let map = self.map.lock().expect("ends");
        map.get(viewer).is_some_and(|(e, t)| *e == Ended::Normal && now.saturating_duration_since(*t) < KEEP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_end_refuses_a_reopen_a_loss_does_not() {
        let ends = Ends::default();
        assert!(!ends.refuses_reopen("v"), "no record: as before");
        ends.record("v", Ended::Normal);
        assert!(ends.refuses_reopen("v"));
        assert!(!ends.refuses_reopen("other"), "only that viewer");
        ends.record("v", Ended::Lost);
        assert!(!ends.refuses_reopen("v"), "the last end counts");
        ends.record("v", Ended::Normal);
        assert!(ends.refuses_reopen("v"));
    }

    #[test]
    fn records_expire_and_are_bounded() {
        let ends = Ends::default();
        let t0 = Instant::now();
        ends.record_at("old", Ended::Normal, t0);
        assert!(ends.refuses_reopen_at("old", t0 + KEEP - Duration::from_secs(1)));
        assert!(!ends.refuses_reopen_at("old", t0 + KEEP), "a day later: forgotten");
        for i in 0..MAX_VIEWERS + 10 {
            ends.record_at(&format!("v{i}"), Ended::Normal, t0 + Duration::from_millis(i as u64));
        }
        let map = ends.map.lock().unwrap();
        assert_eq!(map.len(), MAX_VIEWERS);
        assert!(
            !map.contains_key("v0") && map.contains_key(&format!("v{}", MAX_VIEWERS + 9)),
            "the oldest go"
        );
    }

    #[test]
    fn how_a_session_ended() {
        assert_eq!(Ended::of(&Ok(())), Ended::Normal);
        assert_eq!(Ended::of(&Err(anyhow::anyhow!("@agent.idle_timeout"))), Ended::Normal);
        assert_eq!(Ended::of(&Err(anyhow::anyhow!("@agent.idle_timeout").context("control"))), Ended::Normal);
        let lost = anyhow::Error::new(quinn::ConnectionError::TimedOut)
            .context("connection lost: nothing received from the other side for 10 s");
        assert_eq!(Ended::of(&Err(lost)), Ended::Lost);
        assert_eq!(Ended::of(&Err(anyhow::anyhow!("sending video: connection lost"))), Ended::Lost);
    }
}
