//! This computer announced on its local network while it is registered with its
//! server, for the "On this network" list of the apps there
//! ([`nuvira_discovery`]), unless the person at the PC or a rule of the server
//! turned it off.

use std::sync::atomic::Ordering;

use nuvira_discovery::{Announcement, Announcer};
use tracing::{info, warn};

use crate::SessionContext;

pub(crate) struct LanPresence {
    announcement: Announcement,
    held: Option<Announcer>,
    /// The last attempt failed (no DNS-SD service): said once in the log.
    failed: bool,
}

impl LanPresence {
    pub(crate) fn new(id: &str, name: &str, server_fingerprint: &[u8]) -> Self {
        LanPresence {
            announcement: Announcement {
                id: id.to_string(),
                name: name.to_string(),
                os: nuvira_discovery::this_os().to_string(),
                server: nuvira_discovery::server_tag(server_fingerprint),
            },
            held: None,
            failed: false,
        }
    }

    /// Announces or withdraws as the setting and the rules want now.
    pub(crate) async fn update(&mut self, ctx: &SessionContext) {
        let want =
            ctx.allow_lan.load(Ordering::Relaxed) && !ctx.policy.lock().expect("policy").no_lan_discovery;
        match (want, self.held.is_some()) {
            (true, false) => match nuvira_discovery::announce(&self.announcement).await {
                Ok(held) => {
                    info!("announced on the local network");
                    self.held = Some(held);
                    self.failed = false;
                }
                Err(e) => {
                    if !self.failed {
                        warn!("not announced on the local network: {e:#}");
                    }
                    self.failed = true;
                }
            },
            (false, true) => {
                self.held = None;
                info!("no longer announced on the local network");
            }
            _ => {}
        }
    }
}
