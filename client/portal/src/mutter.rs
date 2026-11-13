//! GNOME's own remote desktop API (`org.gnome.Mutter.RemoteDesktop` and
//! `org.gnome.Mutter.ScreenCast` on the session bus), the one gnome-remote-desktop
//! uses. Unlike the portal it asks nothing and keeps going while the screen is
//! locked (the lock screen is in the stream and takes the keys), and it is there
//! also at GDM's sign-in screen, where no portal runs. Used for unattended access
//! only, where the person already allowed remote control when installing it.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use ashpd::zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use ashpd::zbus::{Connection, Proxy};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{Cmd, Screen, WheelAxis};

const RD: &str = "org.gnome.Mutter.RemoteDesktop";
const SC: &str = "org.gnome.Mutter.ScreenCast";

/// `cursor-mode` of `RecordMonitor`: drawn into the image.
const CURSOR_EMBEDDED: u32 = 1;
/// `cursor-mode` of RecordMonitor: the pointer as stream metadata.
const CURSOR_METADATA: u32 = 2;

async fn proxy(
    bus: &Connection,
    service: &'static str,
    path: OwnedObjectPath,
    iface: &'static str,
) -> Result<Proxy<'static>> {
    Ok(Proxy::new(bus, service, path, iface).await?)
}

struct Opened {
    remote: Proxy<'static>,
    stream_path: String,
    screen: Screen,
    closed: futures_util::stream::BoxStream<'static, ()>,
}

async fn open() -> Result<Opened> {
    let bus = Connection::session().await.context("D-Bus session bus")?;
    let root: OwnedObjectPath = "/org/gnome/Mutter/RemoteDesktop".try_into()?;
    let rd = proxy(&bus, RD, root, RD).await?;
    let path: OwnedObjectPath =
        rd.call("CreateSession", &()).await.context("GNOME's remote desktop API not available")?;
    let remote = proxy(&bus, RD, path, "org.gnome.Mutter.RemoteDesktop.Session").await?;
    let session_id: String = remote.get_property("SessionId").await?;

    let root: OwnedObjectPath = "/org/gnome/Mutter/ScreenCast".try_into()?;
    let sc = proxy(&bus, SC, root, SC).await?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("remote-desktop-session-id", Value::from(session_id.as_str()));
    let path: OwnedObjectPath = sc.call("CreateSession", &(options,)).await?;
    let cast = proxy(&bus, SC, path, "org.gnome.Mutter.ScreenCast.Session").await?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    // The pointer as metadata when the agent wants it (Mutter has it since 3.34).
    let metadata = crate::cursor::wanted();
    crate::cursor::set_active(metadata);
    options.insert("cursor-mode", Value::from(if metadata { CURSOR_METADATA } else { CURSOR_EMBEDDED }));
    // An empty connector: the primary monitor.
    let stream_path: OwnedObjectPath = cast.call("RecordMonitor", &("", options)).await?;
    let stream = proxy(&bus, SC, stream_path.clone(), "org.gnome.Mutter.ScreenCast.Stream").await?;
    let mut added = stream.receive_signal("PipeWireStreamAdded").await?;

    let closed = remote.receive_signal("Closed").await?.map(|_| ()).boxed();
    remote.call::<_, _, ()>("Start", &()).await.context("starting the remote desktop session")?;
    let message = tokio::time::timeout(std::time::Duration::from_secs(10), added.next())
        .await
        .map_err(|_| anyhow!("no screen stream from GNOME"))?
        .context("screen stream signal ended")?;
    let node_id: u32 = message.body().deserialize()?;
    let parameters: HashMap<String, OwnedValue> = stream.get_property("Parameters").await.unwrap_or_default();
    let pair = |name: &str| -> Option<(i32, i32)> {
        parameters.get(name).and_then(|v| <(i32, i32)>::try_from(v.try_clone().ok()?).ok())
    };
    let (left, top) = pair("position").unwrap_or((0, 0));
    let (width, height) = pair("size").unwrap_or((0, 0));
    let screen = Screen { node_id, left, top, width: width.max(0) as u32, height: height.max(0) as u32 };
    info!(?screen, "GNOME remote desktop session started");
    Ok(Opened { remote, stream_path: stream_path.to_string(), screen, closed })
}

/// The user's PipeWire daemon: Mutter's (and KWin's) stream is a node of it.
pub(crate) fn pipewire_socket() -> Result<OwnedFd> {
    let path = match std::env::var_os("PIPEWIRE_REMOTE") {
        Some(remote) if std::path::Path::new(&remote).is_absolute() => remote.into(),
        remote => {
            let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
            std::path::PathBuf::from(dir).join(remote.unwrap_or_else(|| "pipewire-0".into()))
        }
    };
    let socket = std::os::unix::net::UnixStream::connect(&path).with_context(|| path.display().to_string())?;
    Ok(socket.into())
}

pub(crate) fn run(
    ready: std::sync::mpsc::Sender<Result<Screen>>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    open_flag: &AtomicBool,
) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(e.into()));
            return;
        }
    };
    rt.block_on(async {
        let Opened { remote, stream_path, screen, mut closed } = match open().await {
            Ok(opened) => opened,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(screen));
        let mut failed = false;
        loop {
            let cmd = tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => cmd,
                    None => break,
                },
                _ = closed.next() => {
                    info!("GNOME closed the remote desktop session");
                    open_flag.store(false, Ordering::Relaxed);
                    return;
                }
            };
            let result = match cmd {
                Cmd::PipeWire(reply) => {
                    let _ = reply.send(pipewire_socket());
                    continue;
                }
                Cmd::Motion(x, y) => {
                    remote.call::<_, _, ()>("NotifyPointerMotionAbsolute", &(stream_path.as_str(), x, y)).await
                }
                Cmd::Button(button, down) => remote.call::<_, _, ()>("NotifyPointerButton", &(button, down)).await,
                Cmd::Wheel(axis, steps) => {
                    let axis: u32 = if matches!(axis, WheelAxis::Horizontal) { 1 } else { 0 };
                    remote.call::<_, _, ()>("NotifyPointerAxisDiscrete", &(axis, steps)).await
                }
                // Flags 0: a scroll in the middle of a gesture.
                Cmd::Scroll(dx, dy) => remote.call::<_, _, ()>("NotifyPointerAxis", &(dx, dy, 0u32)).await,
                Cmd::Key(keycode, down) => {
                    remote.call::<_, _, ()>("NotifyKeyboardKeycode", &(keycode as u32, down)).await
                }
                Cmd::Keysym(keysym, down) => {
                    remote.call::<_, _, ()>("NotifyKeyboardKeysym", &(keysym as u32, down)).await
                }
            };
            if let Err(e) = result
                && !std::mem::replace(&mut failed, true)
            {
                warn!("GNOME remote desktop input: {e}");
            }
        }
        open_flag.store(false, Ordering::Relaxed);
        let _ = remote.call::<_, _, ()>("Stop", &()).await;
        info!("GNOME remote desktop session closed");
    });
}
