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
    rt: tokio::runtime::Runtime,
    stream: zbus::MessageStream,
    _conn: zbus::Connection,
}

impl IbusWatch {
    pub(crate) fn open() -> Result<Self> {
        let out = std::process::Command::new("ibus").arg("address").output().context("ibus not found")?;
        let address = String::from_utf8(out.stdout)?.trim().to_string();
        anyhow::ensure!(out.status.success() && address.starts_with("unix:"), "IBus is not running");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (conn, stream) = rt.block_on(async {
            let conn = zbus::connection::Builder::address(address.as_str())?.build().await?;
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(IBUS)?
                .interface(CONTEXT)?
                .build();
            let stream = zbus::MessageStream::for_match_rule(rule, &conn, Some(256)).await?;
            anyhow::Ok((conn, stream))
        })?;
        Ok(Self { rt, stream, _conn: conn })
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
        let stream = &mut self.stream;
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
            let text: OwnedValue = body.deserialize::<(OwnedValue, u32, bool, u32)>().map(|b| b.0).ok().or_else(|| {
                body.deserialize::<(OwnedValue, u32, bool)>().map(|b| b.0).ok()
            })?;
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
