//! IBus's signals to the focused input context, read from IBus's own bus (the
//! address `ibus address` prints; the person's user can connect to it), to type
//! characters the keyboard layout lacks through IBus's Unicode entry
//! (Ctrl+Shift+U, the code, space) one after the other: the next one starts only
//! when IBus has committed the last one. IBus handles those keys some time after
//! the compositor got them, and an entry started too early mixed with the
//! previous one ("c8 À" for "ÀÈ").
//!
//! On GNOME the client of IBus is gnome-shell, whatever the application: its
//! context is the one that shows the entry ("u", "uc0") and commits ("À").

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

const IBUS: &str = "org.freedesktop.IBus";
const CONTEXT: &str = "org.freedesktop.IBus.InputContext";

pub(crate) enum Event {
    /// The text being entered ("u", "u1f6"), empty when the entry closes.
    Preedit(String),
    Commit(String),
}

pub(crate) struct IbusWatch {
    /// The connection and its stream: `None` only while dropping.
    bus: Option<Bus>,
    rt: tokio::runtime::Runtime,
}

struct Bus {
    stream: zbus::MessageStream,
    _conn: zbus::Connection,
}

impl Drop for IbusWatch {
    /// The stream and the connection end inside the runtime: dropping the stream
    /// spawns the removal of its match rule (zbus `queue_remove_match`, a
    /// `tokio::spawn`), which outside a runtime panicked with "there is no reactor
    /// running" on the agent's input thread at the end of every session that had
    /// typed through IBus (MT-P2-11 F3). One yield lets that task send it.
    fn drop(&mut self) {
        if let Some(bus) = self.bus.take() {
            self.rt.block_on(async move {
                drop(bus);
                tokio::task::yield_now().await;
            });
        }
    }
}

impl IbusWatch {
    pub(crate) fn open() -> Result<Self> {
        let out = std::process::Command::new("ibus").arg("address").output().context("ibus not found")?;
        let address = String::from_utf8(out.stdout)?.trim().to_string();
        anyhow::ensure!(out.status.success() && address.starts_with("unix:"), "IBus is not running");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let conn =
            rt.block_on(async { zbus::connection::Builder::address(address.as_str())?.build().await })?;
        Self::watch(rt, conn)
    }

    /// Watches IBus's input-context signals on `conn`, made inside `rt`.
    fn watch(rt: tokio::runtime::Runtime, conn: zbus::Connection) -> Result<Self> {
        let stream = rt.block_on(async {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(IBUS)?
                .interface(CONTEXT)?
                .build();
            anyhow::Ok(zbus::MessageStream::for_match_rule(rule, &conn, Some(256)).await?)
        })?;
        Ok(Self { bus: Some(Bus { stream, _conn: conn }), rt })
    }

    /// Forgets the signals already received (the person's own typing, an entry
    /// that ended late).
    pub(crate) fn drain(&mut self) {
        while self.wait(Duration::ZERO, |_| false) {}
    }

    /// Waits for a signal `want` accepts, at most `limit`. False on timeout.
    /// With `Duration::ZERO` it only looks at what has already arrived: true if
    /// something was taken.
    pub(crate) fn wait(&mut self, limit: Duration, mut want: impl FnMut(&Event) -> bool) -> bool {
        let Some(Bus { stream, .. }) = self.bus.as_mut() else { return false };
        self.rt.block_on(async {
            let deadline = tokio::time::Instant::now() + limit;
            loop {
                let next = tokio::time::timeout_at(deadline, stream.next()).await;
                let Ok(Some(Ok(msg))) = next else { return false };
                let Some(event) = event(&msg) else { continue };
                if limit.is_zero() || want(&event) {
                    return true;
                }
            }
        })
    }
}

fn event(msg: &zbus::Message) -> Option<Event> {
    let header = msg.header();
    match header.member()?.as_str() {
        "CommitText" => {
            let (text,): (OwnedValue,) = msg.body().deserialize().ok()?;
            Some(Event::Commit(ibus_text(&text)?))
        }
        "UpdatePreeditTextWithMode" | "UpdatePreeditText" => {
            let body = msg.body();
            let text: OwnedValue = body
                .deserialize::<(OwnedValue, u32, bool, u32)>()
                .map(|b| b.0)
                .ok()
                .or_else(|| body.deserialize::<(OwnedValue, u32, bool)>().map(|b| b.0).ok())?;
            Some(Event::Preedit(ibus_text(&text)?))
        }
        _ => None,
    }
}

/// The string of an IBusText: a structure ("IBusText", attachments, text, attributes)
/// inside a variant.
fn ibus_text(value: &Value) -> Option<String> {
    match value {
        Value::Value(inner) => ibus_text(inner),
        Value::Structure(s) => match s.fields().get(2)? {
            Value::Str(text) => Some(text.to_string()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A watch on a private zbus connection (a socket pair, no IBus needed),
    /// dropped on a plain thread as the agent's input thread drops it at the end
    /// of a session: no panic (MT-P2-11 F3; before, the stream's Drop called
    /// tokio::spawn outside the runtime: "there is no reactor running").
    #[test]
    fn dropping_the_watch_outside_a_runtime_does_not_panic() {
        std::thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let (conn, peer) = rt.block_on(async {
                let (a, b) = tokio::net::UnixStream::pair().unwrap();
                let guid = zbus::Guid::generate();
                let server = zbus::connection::Builder::unix_stream(a).server(guid).unwrap().p2p().build();
                let client = zbus::connection::Builder::unix_stream(b).p2p().build();
                let (server, client) = futures_util::future::join(server, client).await;
                (client.unwrap(), server.unwrap())
            });
            let mut watch = IbusWatch::watch(rt, conn).unwrap();
            assert!(!watch.wait(Duration::from_millis(50), |_| true), "no signal on a silent bus");
            drop(watch);
            drop(peer);
        })
        .join()
        .expect("the watch's drop panicked");
    }
}
