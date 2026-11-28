//! DNS-SD through Avahi (the system's mDNS daemon on Linux desktops), over the
//! D-Bus system bus: <https://github.com/avahi/avahi/tree/master/avahi-daemon>
//! (org.freedesktop.Avahi.*.xml).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tracing::debug;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use crate::{Announcement, Found, SERVICE_TYPE};

const AVAHI: &str = "org.freedesktop.Avahi";
const IF_UNSPEC: i32 = -1;
const PROTO_UNSPEC: i32 = -1;
/// AVAHI_LOOKUP_NO_ADDRESS: resolving gives the TXT record without waiting for
/// the host's address, which is not needed.
const LOOKUP_NO_ADDRESS: u32 = 8;

async fn server(conn: &Connection) -> Result<Proxy<'static>> {
    Ok(Proxy::new(conn, AVAHI, "/", "org.freedesktop.Avahi.Server").await?)
}

/// The entry group of the announcement: Avahi removes it when this connection
/// closes, that is when the registration is dropped.
pub(crate) struct Registration {
    _conn: Connection,
}

pub(crate) async fn register(a: &Announcement) -> Result<Registration> {
    let conn = Connection::system().await.context("D-Bus system bus")?;
    let path: OwnedObjectPath =
        server(&conn).await?.call("EntryGroupNew", &()).await.context("Avahi is not running")?;
    let group = Proxy::new(&conn, AVAHI, path, "org.freedesktop.Avahi.EntryGroup").await?;
    let txt: Vec<Vec<u8>> = a.txt().into_iter().map(String::into_bytes).collect();
    let _: () = group
        .call(
            "AddService",
            &(IF_UNSPEC, PROTO_UNSPEC, 0u32, a.instance(), SERVICE_TYPE, "", "", 21116u16, txt),
        )
        .await
        .context("Avahi AddService")?;
    let _: () = group.call("Commit", &()).await.context("Avahi Commit")?;
    Ok(Registration { _conn: conn })
}

pub(crate) async fn browse(wait: Duration) -> Result<Vec<Found>> {
    let conn = Connection::system().await.context("D-Bus system bus")?;
    let server = server(&conn).await?;
    // Subscribed before the browser exists: Avahi starts sending as soon as it is
    // made, before its path is known here.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.Avahi.ServiceBrowser")?
        .build();
    let mut signals = MessageStream::for_match_rule(rule, &conn, Some(64)).await?;
    let browser: OwnedObjectPath = server
        .call("ServiceBrowserNew", &(IF_UNSPEC, PROTO_UNSPEC, SERVICE_TYPE, "local", 0u32))
        .await
        .context("Avahi is not running")?;

    // Instance name → (interface, protocol) of one of its announcements.
    let mut items: HashMap<String, (i32, i32)> = HashMap::new();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let msg = match tokio::time::timeout_at(deadline, signals.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(_))) => continue,
            Ok(None) | Err(_) => break,
        };
        let header = msg.header();
        if header.path().map(|p| p.as_str()) != Some(browser.as_str()) {
            continue;
        }
        let member = header.member().map(|m| m.to_string()).unwrap_or_default();
        if member != "ItemNew" && member != "ItemRemove" {
            continue;
        }
        let Ok((iface, proto, name, _ty, _domain, _flags)) =
            msg.body().deserialize::<(i32, i32, String, String, String, u32)>()
        else {
            continue;
        };
        if member == "ItemNew" {
            items.insert(name, (iface, proto));
        } else {
            items.remove(&name);
        }
    }
    let browser_proxy = Proxy::new(&conn, AVAHI, browser, "org.freedesktop.Avahi.ServiceBrowser").await?;
    let _: Result<(), _> = browser_proxy.call("Free", &()).await;

    let mut found = Vec::new();
    for (name, (iface, proto)) in items {
        type Resolved = (i32, i32, String, String, String, String, i32, String, u16, Vec<Vec<u8>>, u32);
        let args = (iface, proto, name.as_str(), SERVICE_TYPE, "local", PROTO_UNSPEC, LOOKUP_NO_ADDRESS);
        let call = server.call::<_, _, Resolved>("ResolveService", &args);
        match tokio::time::timeout(Duration::from_secs(2), call).await {
            Ok(Ok(r)) => {
                let txt: Vec<String> =
                    r.9.into_iter().map(|b| String::from_utf8_lossy(&b).into_owned()).collect();
                found.extend(Announcement::from_txt(&txt));
            }
            Ok(Err(e)) => debug!("Avahi could not resolve {name}: {e}"),
            Err(_) => debug!("Avahi did not resolve {name} in time"),
        }
    }
    Ok(found)
}
