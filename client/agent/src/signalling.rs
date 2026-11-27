//! The agent registered with nuvira-server: the signalling loop that answers its
//! messages, and the sessions that arrive through it (direct or relayed).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nuvira_transport::Fingerprint;
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::ending::{closed_normally, end_reason};
use crate::policy::{lock_after, set_policy};
use crate::{
    AgentCommand, AgentEvent, Gate, MAX_PENDING_SESSIONS, Request, ServerOptions, SessionContext, access,
    inventory, normal_end, random_password, reopen, session, shared, stats, switches, watched,
};

/// Ticket of a support request: 128 random bits in hex.
fn support_ticket() -> String {
    let mut bytes = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("random generator");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the report to the server says about a session, filled while it runs.
#[derive(Default)]
struct Outcome {
    /// The viewer authenticated and this was the session in progress ([`Gate::claim`]):
    /// the computer stays busy until the end is handled.
    claimed: bool,
    connected: Option<std::time::Instant>,
    direct: bool,
    access: std::sync::Mutex<&'static str>,
}

pub(crate) async fn signal_loop(
    ctx: &Arc<SessionContext>,
    opts: &ServerOptions,
    identity: &Arc<nuvira_transport::identity::Identity>,
    server: SocketAddr,
    expected: Option<Fingerprint>,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
) -> Result<()> {
    use nuvira_proto::v1::{Accept, Reject, client_message, server_message};
    let device_name = nuvira_transport::device_name();
    let ws_url = opts
        .server_ws
        .clone()
        .unwrap_or_else(|| nuvira_transport::ws::default_url(&opts.server_name, server));
    let mut signal = nuvira_transport::signal::connect_auto(
        server,
        &ws_url,
        opts.transport,
        expected,
        identity,
        &nuvira_transport::signal::DeviceInfo {
            name: &device_name,
            reachable: true,
            alias: std::fs::read_to_string(ctx.access_dir.join(ALIAS_FILE)).ok().map(|a| a.trim().to_string()),
        },
    )
    .await?;
    let websocket = matches!(signal.transport, nuvira_transport::ServerTransport::WebSocket(_));
    if let nuvira_transport::ServerTransport::WebSocket(url) = &signal.transport {
        info!("connected to nuvira-server through the WebSocket {url}");
    }
    if expected.is_none() {
        nuvira_transport::signal::known_server::remember(&opts.server_name, &signal.server_fingerprint)?;
        info!(
            "nuvira-server fingerprint remembered: {}",
            nuvira_transport::fingerprint_hex(&signal.server_fingerprint)
        );
    }
    let id = signal.device_id.clone();
    info!("registered on {}: ID {} {} {}", opts.server_name, &id[..3], &id[3..6], &id[6..]);
    // The relay's address of the other IP family (IPv6 next to IPv4), for the
    // direct path of the sessions: resolved at every connection to the server.
    let relay_alt = match signal.transport {
        nuvira_transport::ServerTransport::Udp => nuvira_transport::other_family(&opts.server_name, server).await,
        nuvira_transport::ServerTransport::WebSocket(_) => None,
    };
    set_policy(ctx, signal.policy);
    if let Some(path) = &opts.info_file {
        // In automated tests the viewer reads ID and password from here.
        let password = ctx.password.lock().expect("password").clone();
        std::fs::write(path, format!("{id} {password}"))?;
    }
    let _ = ctx.events.send(AgentEvent::Online { device_id: id, websocket });
    if !signal.alias_error.is_empty() {
        warn!(error = signal.alias_error, "alias refused by the server");
    }
    keep_alias(&ctx.access_dir, &signal.alias, &signal.alias_error);
    let _ = ctx.events.send(AgentEvent::Alias { alias: signal.alias.clone(), error: signal.alias_error.clone() });

    let mut accepted: HashMap<String, Request> = HashMap::new();
    // Reports of sessions that ended while the server was unreachable go first.
    let mut reports = ctx.reports.rx.lock().await;
    let report_tx = ctx.reports.tx.clone();
    // Light RMM: the inventory goes only to a server that asks for it.
    let send_inventory = signal.features.iter().any(|f| f == "pro.inventory");
    *ctx.features.lock().expect("features") = signal.features.clone();
    let mut inventory_tick = tokio::time::interval(inventory::INTERVAL);

    // A support request still open from the last connection (a network cut, the
    // server restarted): the server dropped it when this device went away, so it is
    // asked again with the same ticket; the server gives it back its place.
    let open_ticket = ctx.support_ticket.lock().expect("support").clone();
    let resend = ctx
        .support_request
        .lock()
        .expect("support")
        .clone()
        .filter(|r| open_ticket.as_deref() == Some(r.ticket.as_str()));
    if let Some(request) = resend {
        info!("support request still open: asked again after reconnecting");
        signal.tx.send(client_message::Msg::SupportRequest(request)).await?;
    }

    loop {
        let reply = tokio::select! {
            Some(report) = reports.recv() => Some(client_message::Msg::SessionReport(report)),
            _ = inventory_tick.tick(), if send_inventory => {
                match tokio::task::spawn_blocking(inventory::collect).await {
                    Ok(inv) => Some(client_message::Msg::Inventory(inv)),
                    Err(_) => None,
                }
            }
            msg = signal.rx.recv() => match msg? {
                Some(server_message::Msg::Policy(policy)) => {
                    set_policy(ctx, policy);
                    None
                }
                Some(server_message::Msg::Incoming(inc)) => {
                    let mut request = Request {
                        viewer_id: inc.viewer_id,
                        viewer_name: inc.viewer_name,
                        limits: inc.limits.unwrap_or_default(),
                        join: false,
                    };
                    info!("connection request from {} ({})", request.viewer_name, request.viewer_id);
                    if request.limits != Default::default() {
                        info!(limits = ?request.limits, "limits of this session");
                    }
                    let retry = ctx
                        .waiting
                        .lock()
                        .expect("waiting")
                        .as_ref()
                        .filter(|(viewer, _)| *viewer == request.viewer_id)
                        .map(|(_, cancel)| cancel.clone());
                    if let Some(cancel) = retry {
                        info!("same viewer again: its previous attempt is abandoned");
                        cancel.notify_one();
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    } else if ctx.busy.load(Ordering::Relaxed) > 0
                        && ctx.hosts.silent(&request.viewer_id, reopen::HOST_SILENT)
                    {
                        // The viewer of the session in progress comes back before the
                        // loss of its connection was noticed (the web page back from
                        // the background): busy until then, not a guest of its own
                        // dead session (MT-P2-12 F1). The page tries again.
                        info!("the viewer of the session in progress, silent, asks again: refused as busy");
                        Some(client_message::Msg::Reject(Reject {
                            session_id: inc.session_id,
                            reason: "@agent.busy".into(),
                        }))
                    } else if ctx.busy.load(Ordering::Relaxed) > 0 && shared::can_join(ctx) {
                        // Another viewer joins the session in progress, after the same
                        // access control.
                        info!("a session is in progress: this viewer would join it");
                        request.join = true;
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    } else if ctx.busy.load(Ordering::Relaxed) > 0
                        || ctx.attempts.load(Ordering::Relaxed) >= MAX_PENDING_SESSIONS
                    {
                        if ctx.busy.load(Ordering::Relaxed) == 0 {
                            warn!("too many connections waiting for their password: refused as busy");
                        }
                        Some(client_message::Msg::Reject(Reject {
                            session_id: inc.session_id,
                            reason: "@agent.busy".into(),
                        }))
                    } else {
                        // Not busy until the viewer has authenticated (Gate::claim): one that
                        // never does cannot keep the others out. Confirmation by the person
                        // at the PC, if needed, comes after the password.
                        accepted.insert(inc.session_id.clone(), request);
                        Some(client_message::Msg::Accept(Accept { session_id: inc.session_id }))
                    }
                }
                Some(server_message::Msg::Route(route)) => {
                    let request = accepted.remove(&route.session_id);
                    let p2p_route = nuvira_transport::p2p::Route {
                        relay: server,
                        transport: signal.transport.clone(),
                        token: route.relay_token.as_slice().try_into().context("invalid relay token")?,
                        peer_public_key: route
                            .peer_public_key
                            .as_slice()
                            .try_into()
                            .context("invalid viewer key")?,
                        role: nuvira_transport::p2p::Role::Agent,
                        relay_alt,
                    };
                    let Some(request) = request else {
                        // Only routes of requests accepted here: their limits are known.
                        warn!("route of a session not accepted here: ignored");
                        continue;
                    };
                    let join = request.join;
                    if !join {
                        ctx.attempts.fetch_add(1, Ordering::Relaxed);
                    }
                    let (ctx, identity) = (ctx.clone(), identity.clone());
                    let session_id = route.session_id.clone();
                    let report_tx = report_tx.clone();
                    tokio::spawn(async move {
                        let mut outcome = Outcome::default();
                        let result = relayed_session(&ctx, &identity, &p2p_route, &request, &session_id, &mut outcome).await;
                        if let Err(e) = &result {
                            warn!("session ended: {e:#}");
                        }
                        // How it ended, for an automatic reopen by the same viewer: only
                        // sessions that got in (the first viewer's, once claimed; a
                        // guest's, once connected).
                        if outcome.claimed || (join && outcome.connected.is_some()) {
                            ctx.ends.record(&request.viewer_id, reopen::Ended::of(&result));
                        }
                        // The counters are reset when the session is claimed: a guest's
                        // transfers are none (they stay the session's), and an attempt
                        // that never authenticated had none.
                        let counted = join || outcome.claimed;
                        let (files_in, files_out, bytes_in, bytes_out) = match counted {
                            true => (
                                stats::FILES_IN.load(Ordering::Relaxed),
                                stats::FILES_OUT.load(Ordering::Relaxed),
                                stats::BYTES_IN.load(Ordering::Relaxed),
                                stats::BYTES_OUT.load(Ordering::Relaxed),
                            ),
                            false => (0, 0, 0, 0),
                        };
                        let _ = report_tx.send(nuvira_proto::v1::SessionReport {
                            session_id: session_id.clone(),
                            duration_s: outcome.connected.map_or(0, |t| t.elapsed().as_secs() as u32),
                            direct: outcome.direct,
                            access: outcome.access.lock().expect("access").to_string(),
                            files_in,
                            files_out,
                            bytes_in,
                            bytes_out,
                            error: result
                                .as_ref()
                                .err()
                                .filter(|e| !closed_normally(e))
                                .map(|e| format!("{e:#}"))
                                .unwrap_or_default(),
                        });
                        if !join {
                            let ended = AgentEvent::SessionEnded { error: result.err().map(|e| format!("{e:#}")) };
                            if outcome.claimed {
                                lock_after(&ctx, &request, outcome.connected.is_some());
                            }
                            let _claims = ctx.claims.lock().unwrap_or_else(|e| e.into_inner());
                            ctx.attempts.fetch_sub(1, Ordering::Relaxed);
                            if outcome.claimed {
                                ctx.busy.fetch_sub(1, Ordering::Relaxed);
                                let _ = ctx.events.send(ended);
                            } else if ctx.busy.load(Ordering::Relaxed) == 0 {
                                // As before: nobody else is connected.
                                lock_after(&ctx, &request, outcome.connected.is_some());
                                let _ = ctx.events.send(ended);
                            }
                            // Otherwise an attempt that never authenticated ended while another
                            // viewer's session goes on: the app is not told (it would close that one).
                        }
                    });
                    None
                }
                Some(server_message::Msg::Wake(wake)) => {
                    tokio::task::spawn_blocking(move || inventory::wake(&wake.macs));
                    None
                }
                Some(server_message::Msg::AliasState(state)) => {
                    info!(alias = state.alias, error = state.error, "alias on the server");
                    keep_alias(&ctx.access_dir, &state.alias, &state.error);
                    let _ = ctx.events.send(AgentEvent::Alias { alias: state.alias, error: state.error });
                    None
                }
                Some(server_message::Msg::SupportStatus(status)) => {
                    if status.closed {
                        *ctx.support_ticket.lock().expect("support") = None;
                    }
                    if !status.error.is_empty() {
                        warn!(error = status.error, "support request refused");
                    }
                    let _ = ctx.events.send(AgentEvent::Support {
                        position: status.position,
                        taken_by: status.taken_by,
                        closed: status.closed,
                        error: status.error,
                        invited_by: status.invited_by,
                    });
                    None
                }
                Some(server_message::Msg::Error(e)) => {
                    warn!("nuvira-server: {}", e.message);
                    None
                }
                Some(_) => None,
                None => bail!("nuvira-server closed the connection"),
            },
            cmd = commands.recv() => match cmd {
                Some(AgentCommand::Respond { session_id, accept }) => {
                    if let Some(answer) = ctx.pending.lock().expect("requests").remove(&session_id) {
                        let _ = answer.send(accept);
                    }
                    None
                }
                Some(AgentCommand::Chat { text }) => {
                    if let Some(chat) = ctx.chat.lock().expect("chat").as_ref() {
                        let _ = chat.send(text);
                    }
                    None
                }
                Some(AgentCommand::NewPassword) => {
                    let password = random_password();
                    *ctx.password.lock().expect("password") = password.clone();
                    let _ = ctx.events.send(AgentEvent::Password { password });
                    None
                }
                Some(AgentCommand::EndSession) => {
                    ctx.end_session.notify_waiters();
                    None
                }
                Some(AgentCommand::RequestSupport { message, invite }) => {
                    let ticket = support_ticket();
                    *ctx.support_ticket.lock().expect("support") = Some(ticket.clone());
                    info!(invitation = !invite.is_empty(), "support requested");
                    let request = nuvira_proto::v1::SupportRequest {
                        ticket,
                        name: device_name.clone(),
                        message,
                        cancel: false,
                        invite,
                    };
                    *ctx.support_request.lock().expect("support") = Some(request.clone());
                    Some(client_message::Msg::SupportRequest(request))
                }
                Some(AgentCommand::AllowPrivacy { allow }) => {
                    if ctx.allow_privacy.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "privacy mode allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowTunnel { allow }) => {
                    if ctx.allow_tunnel.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "tunnels allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowPrint { allow }) => {
                    if ctx.allow_print.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "remote printing allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowRecording { allow }) => {
                    if ctx.allow_recording.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "session recording allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::IdleMinutes { minutes }) => {
                    if ctx.idle_minutes.swap(minutes, Ordering::Relaxed) != minutes {
                        info!(minutes, "sessions end after a time without activity, by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::MuteMicrophone { muted }) => {
                    info!(muted, "technician's microphone muted by the person at the PC");
                    ctx.mic_muted.send_replace(muted);
                    None
                }
                Some(AgentCommand::SetAlias { alias }) => {
                    // Kept once the server takes it (AliasState).
                    let alias = alias.trim().to_lowercase();
                    Some(client_message::Msg::SetAlias(nuvira_proto::v1::SetAlias { alias }))
                }
                Some(AgentCommand::GetAccess) => {
                    let _ = ctx.events.send(AgentEvent::Access { view: access::View::of(&ctx.access_dir) });
                    None
                }
                Some(AgentCommand::LockOnEnd { on }) => {
                    if ctx.lock_on_end.swap(on, Ordering::Relaxed) != on {
                        info!(on, "lock at the end of sessions, by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::AllowTerminal { allow }) => {
                    if ctx.allow_terminal.swap(allow, Ordering::Relaxed) != allow {
                        info!(allow, "remote terminal allowed by the person at the PC");
                        ctx.save_permissions();
                    }
                    None
                }
                Some(AgentCommand::Switch { name, on }) => {
                    let switches = ctx.switches.lock().expect("switches").clone();
                    match (switches, switches::bit(&name)) {
                        (Some(switches), Some(bit)) if switches.set(bit, on) => {
                            info!(name, on, "switched by the person at the PC for this session");
                        }
                        _ => warn!(name, "nothing to switch in this session"),
                    }
                    None
                }
                Some(AgentCommand::CancelSupport) => {
                    *ctx.support_ticket.lock().expect("support") = None;
                    info!("support request withdrawn");
                    Some(client_message::Msg::SupportRequest(nuvira_proto::v1::SupportRequest {
                        cancel: true,
                        ..Default::default()
                    }))
                }
                None => return Ok(()),
            },
        };
        if let Some(reply) = reply {
            let report = match &reply {
                client_message::Msg::SessionReport(r) => Some(r.clone()),
                _ => None,
            };
            if let Err(e) = signal.tx.send(reply).await {
                // Kept for the next connection.
                if let Some(report) = report {
                    let _ = ctx.reports.tx.send(report);
                }
                return Err(e.into());
            }
        }
    }
}

async fn relayed_session(
    ctx: &SessionContext,
    identity: &nuvira_transport::identity::Identity,
    route: &nuvira_transport::p2p::Route,
    request: &Request,
    session_id: &str,
    outcome: &mut Outcome,
) -> Result<()> {
    let endpoint = nuvira_transport::p2p::agent_endpoint(identity, route).await?;
    // The viewer may open several connections (direct and through the relay): the one
    // it opens the control channel on counts; it closes the others itself.
    let (tx, mut rx) = mpsc::channel(2);
    let acceptor = {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    if let Ok(streams) = conn.accept_bi().await {
                        let _ = tx.send((conn, streams)).await;
                    }
                });
            }
        })
    };
    let cancel = Arc::new(Notify::new());
    *ctx.waiting.lock().expect("waiting") = Some((request.viewer_id.clone(), cancel.clone()));
    let connected = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(20), rx.recv()) => {
            r.context("the viewer did not connect within 20 s").map(|c| c.context("endpoint closed"))
        }
        () = cancel.notified() => Err(anyhow::anyhow!("the viewer asked again before connecting")),
    };
    {
        let mut waiting = ctx.waiting.lock().expect("waiting");
        if waiting.as_ref().is_some_and(|(_, c)| Arc::ptr_eq(c, &cancel)) {
            *waiting = None;
        }
    }
    acceptor.abort();
    let (conn, streams) = connected??;
    // Known as a host while it runs (a guest is not one).
    let _host = (!request.join).then(|| ctx.hosts.add(&request.viewer_id, &conn));
    let remote = nuvira_transport::p2p::canonical(conn.remote_address());
    let direct = remote != route.relay;
    outcome.connected = Some(std::time::Instant::now());
    outcome.direct = direct;
    info!(
        "viewer connected {} ({remote})",
        match (direct, remote.is_ipv6()) {
            (true, true) => "directly over IPv6",
            (true, false) => "directly over IPv4",
            (false, _) => "through the relay",
        }
    );
    let gate = Gate {
        ctx,
        session_id,
        request,
        direct,
        path: match (direct, &route.transport) {
            (true, _) => "direct",
            (false, nuvira_transport::ServerTransport::WebSocket(_)) => "websocket",
            (false, nuvira_transport::ServerTransport::Udp) => "relay",
        },
        access: &outcome.access,
        terminal_only: AtomicBool::new(false),
        profile_off: AtomicU8::new(0),
        claimed: AtomicBool::new(false),
        throttled: AtomicBool::new(false),
        end_reason: AtomicBool::new(false),
    };
    let mut at_computer = false;
    let result = tokio::select! {
        r = async {
            match request.join {
                true => watched(&conn, shared::guest_session(conn.clone(), streams, &gate)).await,
                false => session(conn.clone(), streams, &ctx.config, Some(&gate), None).await,
            }
        } => r,
        _ = ctx.end_session.notified() => {
            info!("session ended by the person at the PC");
            at_computer = true;
            Ok(())
        }
    };
    let result = normal_end(result);
    // The whole endpoint is closed: including a connection left over through the relay,
    // which would otherwise keep the agent "busy" until its timeout.
    endpoint.close(0u32.into(), &end_reason(&result, at_computer && gate.end_reason.load(Ordering::Relaxed)));
    endpoint.wait_idle().await;
    if result.is_ok() {
        info!("session ended");
    }
    // The caller frees the computer, after locking it if wanted.
    outcome.claimed = gate.claimed.swap(false, Ordering::Relaxed);
    result
}

/// The alias the device keeps (next to the device key), sent at every registration.
const ALIAS_FILE: &str = "alias.txt";

/// Keeps the alias the server confirmed: a server that lost it gets it back at the
/// next registration. One that is not valid gives way to the server's (it never
/// will be); one taken stays, asked for again at every start.
fn keep_alias(dir: &std::path::Path, alias: &str, error: &str) {
    if !error.is_empty() && error != "@server.alias_invalid" {
        return;
    }
    let path = dir.join(ALIAS_FILE);
    if std::fs::read_to_string(&path).map(|a| a.trim().to_string()).unwrap_or_default() == alias {
        return;
    }
    let result = if alias.is_empty() { std::fs::remove_file(&path) } else { std::fs::write(&path, alias) };
    if let Err(e) = result {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("alias not saved: {e}");
        }
    }
}
