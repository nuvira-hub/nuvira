//! The rules of a session: the server's, with the limits of each session on top, the
//! lock of the computer at its end, and what switch sides allows.

use std::sync::atomic::Ordering;

use tracing::{info, warn};

use crate::{AgentEvent, Request, SessionContext, lock};

pub(crate) fn set_policy(ctx: &SessionContext, policy: nuvira_proto::v1::Policy) {
    info!(
        confirm_always = policy.confirm_always,
        no_files = policy.no_files,
        no_clipboard = policy.no_clipboard,
        no_privacy = policy.no_privacy,
        no_terminal = policy.no_terminal,
        no_tunnel = policy.no_tunnel,
        no_print = policy.no_print,
        record_always = policy.record_always,
        no_resolution = policy.no_resolution,
        no_audio = policy.no_audio,
        no_lan_discovery = policy.no_lan_discovery,
        "rules of the server"
    );
    // The app shows the setting of the recordings locked when the rules want them.
    let _ = ctx.events.send(AgentEvent::Rules {
        recording_required: policy.record_always,
        lock_required: policy.lock_on_end,
        idle_minutes: policy.idle_minutes,
        lan_off: policy.no_lan_discovery,
    });
    *ctx.policy.lock().expect("policy") = policy;
}

/// The rules and the limits of a session together: every limit of either holds.
pub(crate) fn merge_limits(a: nuvira_proto::v1::Policy, b: &nuvira_proto::v1::Policy) -> nuvira_proto::v1::Policy {
    nuvira_proto::v1::Policy {
        confirm_always: a.confirm_always || b.confirm_always,
        no_files: a.no_files || b.no_files,
        no_clipboard: a.no_clipboard || b.no_clipboard,
        no_privacy: a.no_privacy || b.no_privacy,
        no_terminal: a.no_terminal || b.no_terminal,
        no_tunnel: a.no_tunnel || b.no_tunnel,
        no_print: a.no_print || b.no_print,
        record_always: a.record_always || b.record_always,
        no_input: a.no_input || b.no_input,
        no_restart: a.no_restart || b.no_restart,
        no_record: a.no_record || b.no_record,
        no_resolution: a.no_resolution || b.no_resolution,
        no_audio: a.no_audio || b.no_audio,
        lock_on_end: a.lock_on_end || b.lock_on_end,
        idle_minutes: shorter_idle(a.idle_minutes, b.idle_minutes),
        no_microphone: a.no_microphone || b.no_microphone,
        // A rule of the computer, not of a session: the server's stays.
        no_lan_discovery: a.no_lan_discovery,
    }
}

/// The shorter of two idle limits in minutes (0: none).
pub(crate) fn shorter_idle(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, b) => b,
        (a, 0) => a,
        (a, b) => a.min(b),
    }
}

/// Whether the computer is locked when the session in progress ends: by the
/// computer's setting, the server's rules (with this session's limits) or the
/// viewer's request.
pub(crate) fn lock_wanted(ctx: &SessionContext, request: &Request) -> (bool, bool) {
    let forced = ctx.lock_on_end.load(Ordering::Relaxed)
        || merge_limits(*ctx.policy.lock().expect("policy"), &request.limits).lock_on_end;
    (forced || ctx.lock_asked.load(Ordering::Relaxed), forced)
}

/// At the end of a session (not a guest's): locks the computer when wanted, unless
/// the session was handed over (the guest carries on) or the computer is restarting.
pub(crate) fn lock_after(ctx: &SessionContext, request: &Request, connected: bool) {
    let (wanted, _) = lock_wanted(ctx, request);
    ctx.lock_asked.store(false, Ordering::Relaxed);
    if !wanted || !connected || !lock::available() {
        return;
    }
    if ctx.restarting.lock().expect("restarting").is_some() {
        info!("not locked: the computer is restarting");
        return;
    }
    if ctx.handover.lock().expect("handover").is_some() {
        info!("not locked: the session was handed over");
        return;
    }
    match lock::lock() {
        Ok(()) => info!("computer locked at the end of the session"),
        Err(e) => warn!("computer not locked at the end of the session: {e:#}"),
    }
}

/// What the person at the controlled computer can do on the technician's: see and
/// use the screen; no files, clipboard, terminal, tunnels, printing, recording,
/// privacy mode, restart, resolution or sound.
pub(crate) fn reverse_policy() -> nuvira_proto::v1::Policy {
    nuvira_proto::v1::Policy {
        no_files: true,
        no_clipboard: true,
        no_privacy: true,
        no_terminal: true,
        no_tunnel: true,
        no_print: true,
        no_record: true,
        no_restart: true,
        no_resolution: true,
        no_audio: true,
        ..Default::default()
    }
}
