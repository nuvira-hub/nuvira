//! Customisation from a profile of a NuviraHub Pro server: the PCs installed with
//! the MSI package and `PROFILE=<code>`, or given the "Customised installer profile"
//! policy, ask their server for that profile's customisation (logo, colours,
//! contacts, language) and keep it next to the program, where the app reads it
//! (`branding::installed_path`), as for a customised program. The request goes on
//! the signalling connection, whose server certificate is checked as the agent's.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nuvira_proto::v1::{GetBranding, api_request, api_response};
use tracing::{info, warn};

use crate::config::{self, ServiceConfig};

/// A profile changed on the server reaches the PCs within a day.
const REFRESH: Duration = Duration::from_secs(24 * 3600);
const RETRY: Duration = Duration::from_secs(10 * 60);

/// The profile in force: the policy's, otherwise the installation's.
pub fn current(service: &ServiceConfig) -> Option<String> {
    crate::policy::read().profile.or_else(|| service.profile.clone())
}

/// Keeps the customisation of the profile up to date, in a thread of the service.
pub fn spawn(stop: &'static AtomicBool, service: ServiceConfig) {
    let _ = std::thread::Builder::new().name("profile".into()).spawn(move || {
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if Instant::now() >= next {
                next = match current(&service) {
                    None => Instant::now() + REFRESH,
                    Some(profile) => match fetch(&service, &profile) {
                        Ok(()) => Instant::now() + REFRESH,
                        Err(e) => {
                            warn!("customisation of profile {profile} not received: {e:#}");
                            Instant::now() + RETRY
                        }
                    },
                };
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn fetch(service: &ServiceConfig, profile: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let doc = runtime.block_on(ask(service, profile))?;
    // The app checks every field again; here only that it is a customisation.
    let value: serde_json::Value = serde_json::from_str(&doc).context("the answer is not a customisation")?;
    if !value.is_object() {
        bail!("the answer is not a customisation");
    }
    let path = crate::branding::installed_path();
    if std::fs::read_to_string(&path).ok().as_deref() == Some(doc.as_str()) {
        return Ok(());
    }
    std::fs::write(&path, &doc).with_context(|| format!("writing {}", path.display()))?;
    info!("customisation of profile {profile} saved: the app shows it from its next start");
    Ok(())
}

async fn ask(service: &ServiceConfig, profile: &str) -> Result<String> {
    use nuvira_transport::api::ApiClient;

    let server_name = nuvira_transport::signal::server_or_public(&service.server).to_string();
    let server = nuvira_transport::resolve_addr(&server_name, nuvira_transport::signal::DEFAULT_PORT)?;
    let ws_url =
        service.server_ws.clone().unwrap_or_else(|| nuvira_transport::ws::default_url(&server_name, server));
    let fingerprint = service.server_key.as_deref().map(nuvira_transport::parse_fingerprint).transpose()?;
    let mode = match service.transport.as_str() {
        "udp" => nuvira_transport::signal::TransportMode::Udp,
        "websocket" => nuvira_transport::signal::TransportMode::WebSocket,
        _ => nuvira_transport::signal::TransportMode::Auto,
    };
    // Its own identity: with the agent's the server would take it for the agent.
    let identity = nuvira_transport::identity::Identity::load_or_create(&config::data_dir().join("profile.key"))?;
    let name = nuvira_transport::device_name();
    let client = tokio::time::timeout(
        Duration::from_secs(30),
        ApiClient::connect_pinned(&server_name, server, &ws_url, mode, &identity, &name, fingerprint),
    )
    .await
    .context("the server does not answer")??;
    let call = api_request::Call::GetBranding(GetBranding { profile: profile.to_string() });
    match client.call(call).await? {
        Some(api_response::Result::Branding(branding)) => Ok(branding.doc),
        _ => bail!("unexpected answer"),
    }
}
