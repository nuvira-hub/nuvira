//! Limits on wrong passwords, against online guessing of the passwords (review
//! AGT-4). Two brakes, both refused before any password is checked (no PBKDF2 for
//! a refused attempt) and answered like a wrong password (nothing tells the viewer
//! it is locked out, nothing to learn from the answer):
//!
//! - per viewer ID: after [`MAX_WRONG`] wrong passwords that ID is refused for
//!   [`FIRST_LOCK`], doubling with each further wrong one up to [`MAX_LOCK`]; it is
//!   forgotten [`WINDOW`] after its last wrong password (or the end of its lock);
//! - on the whole computer: after [`BUDGET`] wrong passwords in [`WINDOW`], from
//!   any ID, only the IDs that already gave a right password here since the agent
//!   started are let in. Someone guessing with new IDs cannot lock out a known
//!   technician; a technician new to this computer waits until the attack stops.
//!
//! Everything is in memory: it starts empty when the agent starts.

use std::collections::{HashMap, HashSet, VecDeque};

use tokio::time::{Duration, Instant};

/// Wrong passwords from one viewer ID before it is refused for a while.
pub(crate) const MAX_WRONG: u32 = 5;
/// How long wrong passwords are remembered.
pub(crate) const WINDOW: Duration = Duration::from_secs(600);
/// First refusal of a viewer ID, doubled with each further wrong password.
pub(crate) const FIRST_LOCK: Duration = Duration::from_secs(60);
pub(crate) const MAX_LOCK: Duration = Duration::from_secs(900);
/// Wrong passwords in [`WINDOW`] (all IDs together) after which only known IDs are let in.
pub(crate) const BUDGET: usize = 20;

/// Why an attempt is refused before its password is checked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// This viewer ID gave too many wrong passwords.
    Locked,
    /// Too many wrong passwords on this computer, and this ID never gave a right one.
    Budget,
}

/// What a wrong password changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Wrong {
    /// The viewer ID is refused from now for this long.
    pub lock: Option<Duration>,
    /// This wrong password used up the computer's budget.
    pub budget_reached: bool,
}

#[derive(Default)]
pub(crate) struct Guesses {
    by_viewer: HashMap<String, Viewer>,
    /// When the recent wrong passwords came, all IDs together.
    recent: VecDeque<Instant>,
    /// IDs that gave a right password since the agent started.
    known: HashSet<String>,
}

struct Viewer {
    wrong: u32,
    last: Instant,
    until: Option<Instant>,
}

impl Guesses {
    /// `Some` when this viewer is refused without checking its password.
    pub(crate) fn check(&mut self, viewer: &str) -> Option<Refusal> {
        let now = Instant::now();
        self.prune(now);
        if self.by_viewer.get(viewer).and_then(|v| v.until).is_some_and(|until| now < until) {
            return Some(Refusal::Locked);
        }
        if self.recent.len() >= BUDGET && !self.known.contains(viewer) {
            return Some(Refusal::Budget);
        }
        None
    }

    /// A wrong password from `viewer`.
    pub(crate) fn wrong(&mut self, viewer: &str) -> Wrong {
        let now = Instant::now();
        self.prune(now);
        self.recent.push_back(now);
        let entry =
            self.by_viewer.entry(viewer.to_string()).or_insert(Viewer { wrong: 0, last: now, until: None });
        entry.wrong += 1;
        entry.last = now;
        let lock = (entry.wrong >= MAX_WRONG).then(|| {
            let lock = FIRST_LOCK.saturating_mul(1 << (entry.wrong - MAX_WRONG).min(4)).min(MAX_LOCK);
            entry.until = Some(now + lock);
            lock
        });
        Wrong { lock, budget_reached: self.recent.len() == BUDGET }
    }

    /// A right password from `viewer`: its wrong ones are forgotten, and it is let in
    /// also while the computer's budget is used up.
    pub(crate) fn right(&mut self, viewer: &str) {
        self.by_viewer.remove(viewer);
        self.known.insert(viewer.to_string());
    }

    fn prune(&mut self, now: Instant) {
        while self.recent.front().is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW) {
            self.recent.pop_front();
        }
        self.by_viewer
            .retain(|_, v| now.saturating_duration_since(v.until.unwrap_or(v.last).max(v.last)) < WINDOW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_viewer_is_locked_after_max_wrong_and_the_lock_doubles() {
        let mut g = Guesses::default();
        for _ in 1..MAX_WRONG {
            assert_eq!(g.check("a"), None);
            assert_eq!(g.wrong("a"), Wrong::default());
        }
        assert_eq!(g.check("a"), None);
        assert_eq!(g.wrong("a").lock, Some(FIRST_LOCK));
        assert_eq!(g.check("a"), Some(Refusal::Locked));
        assert_eq!(g.check("b"), None, "other IDs are not locked");
        tokio::time::advance(FIRST_LOCK).await;
        assert_eq!(g.check("a"), None);
        // Each further wrong one doubles the lock, up to MAX_LOCK.
        let mut expected = FIRST_LOCK;
        for _ in 0..6 {
            expected = (expected * 2).min(MAX_LOCK);
            assert_eq!(g.wrong("a").lock, Some(expected));
            assert_eq!(g.check("a"), Some(Refusal::Locked));
            tokio::time::advance(expected - Duration::from_secs(1)).await;
            assert_eq!(g.check("a"), Some(Refusal::Locked));
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(g.check("a"), None);
        }
        assert_eq!(expected, MAX_LOCK);
    }

    #[tokio::test(start_paused = true)]
    async fn wrong_passwords_are_forgotten_after_the_window() {
        let mut g = Guesses::default();
        for _ in 1..MAX_WRONG {
            g.wrong("a");
        }
        tokio::time::advance(WINDOW).await;
        // Forgotten: MAX_WRONG - 1 more before a lock.
        for _ in 1..MAX_WRONG {
            assert_eq!(g.wrong("a").lock, None);
        }
        assert!(g.wrong("a").lock.is_some());
        // After a lock, the window counts from the lock's end.
        tokio::time::advance(FIRST_LOCK + WINDOW - Duration::from_secs(1)).await;
        assert_eq!(g.wrong("a").lock, Some(FIRST_LOCK * 2), "still remembered");
        tokio::time::advance(FIRST_LOCK * 2 + WINDOW).await;
        assert_eq!(g.wrong("a").lock, None, "forgotten");
        assert_eq!(g.by_viewer.len(), 1);
        assert!(g.recent.len() < BUDGET);
    }

    #[tokio::test(start_paused = true)]
    async fn a_right_password_forgets_the_wrong_ones() {
        let mut g = Guesses::default();
        for _ in 1..MAX_WRONG {
            g.wrong("a");
        }
        g.right("a");
        for _ in 1..MAX_WRONG {
            assert_eq!(g.wrong("a").lock, None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_refuses_only_ids_never_right_here() {
        let mut g = Guesses::default();
        g.right("known");
        // Many new IDs, a few wrong passwords each (never locked one by one).
        let mut reached = 0;
        for i in 0..BUDGET {
            let w = g.wrong(&format!("new{}", i / 2));
            assert_eq!(w.lock, None);
            reached += usize::from(w.budget_reached);
        }
        assert_eq!(reached, 1, "said once");
        assert_eq!(g.check("new0"), Some(Refusal::Budget));
        assert_eq!(g.check("never-seen"), Some(Refusal::Budget));
        assert_eq!(g.check("known"), None, "a known technician still gets in");
        // A known ID with wrong passwords is still locked on its own count.
        for _ in 0..MAX_WRONG {
            g.wrong("known");
        }
        assert_eq!(g.check("known"), Some(Refusal::Locked));
        // The budget frees itself once the wrong passwords are old enough.
        tokio::time::advance(WINDOW).await;
        assert_eq!(g.check("never-seen"), None);
        assert!(g.by_viewer.len() <= 1, "old IDs are forgotten");
    }
}
