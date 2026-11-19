//! KDE Plasma: KWin's version, and what the remote desktop portal of that
//! version needs.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};

/// KWin's major and minor version, if KWin runs this session (asked once).
pub fn kwin_version() -> Option<(u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        // Own thread: the caller may be inside an async runtime.
        std::thread::spawn(|| -> Result<(u32, u32)> {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(async {
                let ask = async {
                    let conn = ashpd::zbus::Connection::session().await?;
                    let reply = conn
                        .call_method(
                            Some("org.kde.KWin"),
                            "/KWin",
                            Some("org.kde.KWin"),
                            "supportInformation",
                            &(),
                        )
                        .await?;
                    let text: String = reply.body().deserialize()?;
                    parse_version(&text).context("no version in KWin's support information")
                };
                // Asked when a portal session starts: a KWin that does not answer
                // must not hold the session up.
                tokio::time::timeout(Duration::from_secs(5), ask).await.context("no answer from KWin")?
            })
        })
        .join()
        .ok()
        .and_then(Result::ok)
    })
}

/// "KWin version: 6.6.6" in KWin's support information.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.lines().find_map(|l| l.trim().strip_prefix("KWin version:"))?;
    let mut parts = rest.trim().split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// What one wheel notch is in the portal's discrete axis on this desktop.
///
/// xdg-desktop-portal-kde up to Plasma 6.8 passes a discrete step count to KWin's
/// fake input as a smooth scroll of that many units, and Xwayland, GTK and Qt
/// count 10 such units as one notch: there a notch is sent as 10 steps. From 6.9
/// the portal sends discrete steps through libei: not scaled, nor for a version
/// not known (seen on 6.6 and 6.8; 6.9 not released yet, to check before
/// widening).
pub fn portal_wheel_scale() -> i32 {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    wheel_scale(&desktop, kwin_version)
}

fn wheel_scale(desktop: &str, kwin: impl FnOnce() -> Option<(u32, u32)>) -> i32 {
    // A list such as "KDE" or "ubuntu:KDE": the portal backend is then KDE's.
    let kde = desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE"));
    match kde.then(kwin).flatten() {
        Some((6, minor)) if minor <= 8 => 10,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kwin_version_line() {
        let text = "KWin Support Information:\n==========================\n\nVersion\n=======\nKWin version: 6.6.6\nQt Version: 6.10.1\n";
        assert_eq!(parse_version(text), Some((6, 6)));
        assert_eq!(parse_version("nothing"), None);
    }

    #[test]
    fn wheel_scaled_only_on_plasma_6_up_to_6_8() {
        assert_eq!(wheel_scale("KDE", || Some((6, 6))), 10);
        assert_eq!(wheel_scale("KDE", || Some((6, 0))), 10);
        assert_eq!(wheel_scale("ubuntu:kde", || Some((6, 8))), 10);
        assert_eq!(wheel_scale("KDE", || Some((6, 9))), 1);
        assert_eq!(wheel_scale("KDE", || Some((7, 0))), 1);
        assert_eq!(wheel_scale("KDE", || Some((5, 27))), 1);
        assert_eq!(wheel_scale("KDE", || None), 1);
        assert_eq!(wheel_scale("GNOME", || Some((6, 6))), 1);
        assert_eq!(wheel_scale("", || Some((6, 6))), 1);
    }

    #[test]
    fn kwin_not_asked_outside_kde() {
        assert_eq!(wheel_scale("ubuntu:GNOME", || panic!("KWin asked on GNOME")), 1);
    }
}
