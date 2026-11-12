//! What the person at the PC switches off (and back on) during a session, from the
//! strip of the app ([`crate::AgentCommand::Switch`]).
//!
//! Only what the session has can be switched: the server's rules, the limits of the
//! session (permissions of the technician's account) and the settings of this
//! computer come first, and nothing they do not allow becomes available here.
//! Switching something off stops it at once: keyboard and mouse are released, open
//! terminals, tunnels and file transfers end.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::watch;

pub const INPUT: u8 = 1;
pub const CLIPBOARD: u8 = 2;
pub const FILES: u8 = 4;
pub const TERMINAL: u8 = 8;
pub const TUNNEL: u8 = 16;
pub const PRINT: u8 = 32;
pub const RESOLUTION: u8 = 64;
pub const AUDIO: u8 = 128;

/// The switches with their names (in the app's commands and in `Permissions`).
pub const ALL: [(u8, &str); 8] = [
    (INPUT, "input"),
    (CLIPBOARD, "clipboard"),
    (FILES, "files"),
    (TERMINAL, "terminal"),
    (TUNNEL, "tunnel"),
    (PRINT, "print"),
    (RESOLUTION, "resolution"),
    (AUDIO, "audio"),
];

pub fn bit(name: &str) -> Option<u8> {
    ALL.iter().find(|(_, n)| *n == name).map(|(b, _)| *b)
}

pub fn names(mask: u8) -> Vec<String> {
    ALL.iter().filter(|(b, _)| mask & b != 0).map(|(_, n)| n.to_string()).collect()
}

/// Switches of a session: what it offers, and what is off now (bits).
pub struct Switches {
    offered: AtomicU8,
    tx: watch::Sender<u8>,
}

impl Switches {
    pub fn offered(&self) -> u8 {
        self.offered.load(Ordering::Relaxed)
    }

    /// What the session has (set once it knows, after the hello).
    pub fn set_offered(&self, offered: u8) {
        self.offered.store(offered, Ordering::Relaxed);
    }

    /// Sets one switch; false if the session does not offer it.
    pub fn set(&self, bit: u8, on: bool) -> bool {
        if self.offered() & bit == 0 {
            return false;
        }
        self.tx.send_if_modified(|off| {
            let new = if on { *off & !bit } else { *off | bit };
            std::mem::replace(off, new) != new
        });
        true
    }
}

/// What the session reads: whether a switch is on, and when one goes off.
#[derive(Clone)]
pub struct Off(watch::Receiver<u8>);

/// A new set of switches (all on); the session keeps `Switches` alive as long as it runs.
pub fn channel() -> (Arc<Switches>, Off) {
    let (tx, rx) = watch::channel(0);
    (Arc::new(Switches { offered: AtomicU8::new(0), tx }), Off(rx))
}

impl Off {
    pub fn is_on(&self, bit: u8) -> bool {
        *self.0.borrow() & bit == 0
    }

    /// The bits off now.
    pub fn mask(&self) -> u8 {
        *self.0.borrow()
    }

    /// Ends when the switch goes off (never, if it stays on).
    pub async fn switched_off(&self, bit: u8) {
        let mut rx = self.0.clone();
        if rx.wait_for(|off| off & bit != 0).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Ends at the next change, with the new bits (never, once the switches are gone).
    pub async fn changed(&mut self) -> u8 {
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        *self.0.borrow_and_update()
    }
}

/// Runs `fut` until the switch goes off: `None` if it was stopped that way.
pub async fn until_off<F: Future>(off: Off, bit: u8, fut: F) -> Option<F::Output> {
    tokio::select! {
        out = fut => Some(out),
        () = off.switched_off(bit) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn switches() {
        let (switches, mut off) = channel();
        switches.set_offered(INPUT | FILES);
        assert!(off.is_on(INPUT) && off.is_on(TERMINAL));
        assert!(!switches.set(TERMINAL, false), "a switch the session does not offer");
        assert!(switches.set(FILES, false));
        assert_eq!(off.changed().await, FILES);
        assert!(!off.is_on(FILES) && off.is_on(INPUT));
        let stopped = until_off(off.clone(), FILES, std::future::pending::<()>()).await;
        assert!(stopped.is_none());
        switches.set(FILES, true);
        assert_eq!(off.changed().await, 0);
        assert_eq!(until_off(off.clone(), FILES, async { 7 }).await, Some(7));
        assert_eq!(names(INPUT | PRINT), ["input", "print"]);
        assert_eq!(bit("tunnel"), Some(TUNNEL));
    }
}
