//! The access control of a session arriving from the server: the password, the
//! two-factor code, the support ticket or the hand-over token, the access rules and
//! the confirmation by the person at the PC.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use nuvira_proto::v1::{Auth, ControlMessage, Hello, control_message::Msg};
use nuvira_transport::{FramedRecv, FramedSend};
use tracing::{info, warn};

use crate::{
    AgentEvent, CODE_MAX_WRONG, CODE_REQUIRED, CODE_WINDOW, CONFIRM_TIMEOUT, ENDED_AT_COMPUTER, NOT_ALLOWED,
    RESTART_GIVEN_UP, Request, SessionContext, WRONG_PASSWORD, access, control, guesses, merge_limits, same_secret,
    shared, stats, switches,
};

/// Checks the viewer's `Auth`. When the computer also wants a two-factor code and
/// the viewer did not send one, a viewer that can answer is asked for it
/// (`CodeRequired`) and everything is checked again with it; an older viewer is
/// told to update. `Err` only when the connection broke.
pub(crate) async fn authorize_with_code(
    gate: &Gate<'_>,
    mut given: Auth,
    viewer_two_factor: bool,
    viewer_protocol: u32,
    ctrl_tx: &mut FramedSend,
    ctrl_rx: &mut FramedRecv,
) -> Result<Result<(), String>> {
    // Viewers that know it are told when the person at the PC is being asked.
    let tells_pending = viewer_protocol >= nuvira_proto::v1::ProtocolVersion::ConfirmationPending as u32;
    let verdict = gate.authorize(&given, tells_pending.then_some(&mut *ctrl_tx)).await;
    if !matches!(&verdict, Err(e) if e == CODE_REQUIRED) {
        return Ok(verdict);
    }
    if !viewer_two_factor {
        info!("two-factor code needed: the viewer cannot ask for it");
        // A key older viewers know: "protocol version not supported: update NuviraHub".
        return Ok(Err("@server.old_version".into()));
    }
    info!("password right: asking the viewer for the two-factor code");
    ctrl_tx.send(&control(Msg::CodeRequired(nuvira_proto::v1::CodeRequired {}))).await?;
    let code = match tokio::time::timeout(Duration::from_secs(180), ctrl_rx.recv::<ControlMessage>()).await {
        Ok(Ok(Some(ControlMessage { msg: Some(Msg::Auth(auth)) }))) => auth.code,
        Ok(Err(e)) => return Err(e),
        _ => return Ok(Err("@agent.no_code".into())),
    };
    given.code = code;
    Ok(gate.authorize(&given, tells_pending.then_some(ctrl_tx)).await)
}

/// Access control of a session arriving from the server: permanent password
/// (unattended access) or temporary password, with confirmation by the person
/// at the PC if required.
pub(crate) struct Gate<'a> {
    pub(crate) ctx: &'a SessionContext,
    pub(crate) session_id: &'a str,
    pub(crate) request: &'a Request,
    pub(crate) direct: bool,
    /// `direct`, `relay` or `websocket`, for the statistics log.
    pub(crate) path: &'static str,
    /// How access went, for the report to the server.
    pub(crate) access: &'a std::sync::Mutex<&'static str>,
    /// The viewer asked for a terminal only (set from its Hello).
    pub(crate) terminal_only: AtomicBool,
    /// Switches off because of the profile whose password was given ([`access`]).
    pub(crate) profile_off: AtomicU8,
    /// This viewer's session is the one in progress ([`Gate::claim`]).
    pub(crate) claimed: AtomicBool,
    /// Refused as a wrong password without checking it ([`guesses`]): the
    /// connection is closed without the longer pause of a wrong password.
    pub(crate) throttled: AtomicBool,
    /// The viewer shows "@agent.ended_at_computer" as the end of the session
    /// (`Hello.end_reason`): the person at the PC's end is closed with that key.
    pub(crate) end_reason: AtomicBool,
}

/// A gate dropped with its claim still on (its session over) frees the computer.
impl Drop for Gate<'_> {
    fn drop(&mut self) {
        if self.claimed.swap(false, Ordering::Relaxed) {
            let _claims = self.ctx.claims.lock().unwrap_or_else(|e| e.into_inner());
            self.ctx.busy.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Gate<'_> {
    /// From here this viewer's session is the one in progress (not for a guest):
    /// the computer is busy until it ends. Taken once the viewer's credentials are
    /// right, before the person at the PC is asked, so that they are asked about
    /// one viewer at a time. `Err` when another viewer authenticated first while
    /// both were connecting.
    /// A viewer reopening by itself (`Hello.reopen`, the web page back from the
    /// background) a session that ended normally here (the person at the PC ended
    /// it while the page was suspended or stopped and never heard of it): refused,
    /// before its password is asked for or read (MT-P2-11 F2 case 1, MT-P2-12 F2).
    /// Called with every viewer's Hello: it also keeps `Hello.end_reason`.
    pub(crate) fn refuse_reopen(&self, hello: &Hello) -> Result<()> {
        self.end_reason.store(hello.end_reason, Ordering::Relaxed);
        if hello.reopen && self.ctx.ends.refuses_reopen(&self.request.viewer_id) {
            info!(viewer = self.request.viewer_id, "automatic reopen after a normal end: refused");
            bail!(ENDED_AT_COMPUTER);
        }
        Ok(())
    }

    fn claim(&self) -> Result<(), String> {
        if self.request.join || self.claimed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _claims = self.ctx.claims.lock().unwrap_or_else(|e| e.into_inner());
        if self.ctx.busy.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            info!("another viewer's session started first: refused as busy");
            return Err("@agent.busy".into());
        }
        self.claimed.store(true, Ordering::Relaxed);
        // The transfer counters of the report are this session's.
        stats::reset();
        Ok(())
    }

    /// `Err` with the reason to show to the viewer.
    async fn authorize(&self, auth: &Auth, pending: Option<&mut FramedSend>) -> Result<(), String> {
        let ctx = self.ctx;
        if ctx.restarting.lock().expect("restarting").is_some_and(|t| t.elapsed() < RESTART_GIVEN_UP) {
            return Err("@agent.restarting".into());
        }
        let given = auth.password.as_str();
        let confirm_always = self.policy().confirm_always;
        let set = |access: &'static str| *self.access.lock().expect("access") = access;
        let rules = access::AccessRules::load(&ctx.access_dir);
        if let Err(e) = &rules {
            warn!("rules of unattended access not read: {e}");
        }
        let viewer = self.request.viewer_id.as_str();
        // Who may connect, also with the temporary password when the rules say so.
        let attended_allowed = rules.as_ref().map_or(true, |r| r.allows(viewer, true));
        if (!auth.handover.is_empty() || !auth.support_ticket.is_empty()) && !attended_allowed {
            info!(viewer, "this computer is not among those allowed to connect");
            set("not_allowed");
            return Err(NOT_ALLOWED.into());
        }
        // The guest a session was handed over to, with its token: the person at the
        // PC accepted it already (not asked again).
        if !auth.handover.is_empty() {
            let mut slot = ctx.handover.lock().expect("handover");
            let valid = !self.request.join
                && slot.as_ref().is_some_and(|h| {
                    h.at.elapsed() < shared::HANDOVER_VALID
                        && h.viewer_id == self.request.viewer_id
                        && same_secret(&auth.handover, &h.token)
                });
            if valid {
                *slot = None;
                drop(slot);
                info!("access with the token of a session handed over");
                set("handover");
                return self.started();
            }
            set("wrong_password");
            return Err(WRONG_PASSWORD.into());
        }
        // The ticket of this device's support request, used once: it replaces the
        // password, never the consent of the person at the PC.
        let support = !auth.support_ticket.is_empty() && {
            let mut open = ctx.support_ticket.lock().expect("support");
            let matches = open.as_deref().is_some_and(|t| same_secret(&auth.support_ticket, t));
            if matches {
                *open = None;
            }
            matches
        };
        if support {
            info!("access with the support request's ticket: asking the person at the PC");
            set("support");
        } else if !auth.support_ticket.is_empty() {
            set("wrong_password");
            return Err("@agent.support_ticket_invalid".into());
        } else if let Some(refusal) = ctx.guesses.lock().expect("guesses").check(viewer) {
            // Before any password is checked (no PBKDF2), answered like a wrong one.
            match refusal {
                guesses::Refusal::Locked => info!(viewer, "too many wrong passwords from this viewer"),
                guesses::Refusal::Budget => info!(viewer, "too many wrong passwords: new viewer refused"),
            }
            self.throttled.store(true, Ordering::Relaxed);
            set("wrong_password");
            return Err(WRONG_PASSWORD.into());
        } else if let Some(profile) = self.permanent(given, rules.as_ref().ok()) {
            // A permanent password: the rules hold, and an unreadable file refuses it.
            let Ok(rules) = &rules else {
                set("not_allowed");
                return Err("@agent.access_rules_invalid".into());
            };
            if !rules.allows(viewer, false) {
                info!(viewer, "this computer is not among those allowed to connect");
                set("not_allowed");
                return Err(NOT_ALLOWED.into());
            }
            if let Some(secret) = &rules.totp {
                self.check_code(secret, &auth.code)?;
            }
            ctx.guesses.lock().expect("guesses").right(viewer);
            match profile {
                Some(profile) => {
                    let off = profile.off.iter().filter_map(|n| switches::bit(n)).fold(0, |a, b| a | b);
                    let (name, off_names) = (&profile.name, &profile.off);
                    info!(profile = name, off = ?off_names, "access with the permanent password of a profile");
                    self.profile_off.store(off, Ordering::Relaxed);
                    set("profile");
                }
                None => {
                    info!("access with the permanent password");
                    set("permanent");
                }
            }
            if !confirm_always {
                return self.started();
            }
            info!("the server's rules require confirmation also with the permanent password");
        } else {
            let temporary = ctx.password.lock().expect("password").clone();
            if !same_secret(given, &temporary) {
                // An empty password guesses nothing (it is never right).
                if !given.is_empty() {
                    let wrong = ctx.guesses.lock().expect("guesses").wrong(viewer);
                    if let Some(lock) = wrong.lock {
                        let lock_s = lock.as_secs();
                        warn!(viewer, lock_s, "too many wrong passwords from this viewer: locked for a while");
                    }
                    if wrong.budget_reached {
                        warn!("too many wrong passwords: only viewers already known here let in for a while");
                    }
                }
                set("wrong_password");
                return Err(WRONG_PASSWORD.into());
            }
            if !attended_allowed {
                info!(viewer, "this computer is not among those allowed to connect");
                set("not_allowed");
                return Err(NOT_ALLOWED.into());
            }
            ctx.guesses.lock().expect("guesses").right(viewer);
            set("temporary");
            if !ctx.confirm && !confirm_always {
                return self.started();
            }
        }
        // The credentials are right: the session is this viewer's from here.
        self.claim()?;
        if let Some(ctrl_tx) = pending {
            let waiting =
                nuvira_proto::v1::ConfirmationPending { timeout_s: CONFIRM_TIMEOUT.as_secs() as u32 };
            let _ = ctrl_tx.send(&control(Msg::ConfirmationPending(waiting))).await;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.pending.lock().expect("requests").insert(self.session_id.to_string(), tx);
        let _ = ctx.events.send(AgentEvent::Incoming {
            session_id: self.session_id.to_string(),
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
            support,
        });
        let answer = tokio::time::timeout(CONFIRM_TIMEOUT, rx).await;
        ctx.pending.lock().expect("requests").remove(self.session_id);
        let _ = ctx.events.send(AgentEvent::IncomingClosed { session_id: self.session_id.to_string() });
        match answer {
            Ok(Ok(true)) => {
                set("confirmed");
                self.started()
            }
            // Rejected by the person at the PC, or no app open to ask them.
            Ok(Ok(false)) => {
                info!("connection not accepted on the device");
                set("not_accepted");
                Err("@agent.not_accepted".into())
            }
            _ => {
                info!("no answer to the connection request: rejected");
                set("no_answer");
                Err("@agent.no_answer".into())
            }
        }
    }

    /// `given` is a permanent password: `Some(None)` the main one, `Some(Some(_))`
    /// a profile's (only where there is a main one: unattended access).
    fn permanent(&self, given: &str, rules: Option<&access::AccessRules>) -> Option<Option<access::Profile>> {
        let main = self.ctx.permanent.as_deref()?;
        if given.is_empty() {
            return None;
        }
        #[cfg(test)]
        gate_characterisation::VERIFY_CALLS.with(|c| c.set(c.get() + 1));
        if nuvira_secret::verify(given, main) {
            return Some(None);
        }
        rules?.profile(given).cloned().map(Some)
    }

    /// The two-factor code: each works once, a few wrong ones lock it for a while.
    fn check_code(&self, secret: &str, code: &str) -> Result<(), String> {
        let ctx = self.ctx;
        let mut wrong = ctx.wrong_codes.lock().expect("codes");
        wrong.retain(|t| t.elapsed() < CODE_WINDOW);
        if wrong.len() >= CODE_MAX_WRONG {
            warn!("too many wrong two-factor codes: permanent passwords refused for a while");
            *self.access.lock().expect("access") = "code_locked";
            return Err("@agent.code_locked".into());
        }
        if code.trim().is_empty() {
            return Err(CODE_REQUIRED.into());
        }
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        match nuvira_secret::totp_verify(secret, code, now) {
            Some(step) if ctx.last_code_step.fetch_max(step, Ordering::Relaxed) < step => Ok(()),
            found => {
                if found.is_some() {
                    info!("two-factor code already used");
                }
                wrong.push(Instant::now());
                *self.access.lock().expect("access") = "wrong_code";
                Err("@agent.code_wrong".into())
            }
        }
    }

    /// Rules of the server now, with the limits of this session.
    pub(crate) fn policy(&self) -> nuvira_proto::v1::Policy {
        merge_limits(*self.ctx.policy.lock().expect("policy"), &self.request.limits)
    }

    fn started(&self) -> Result<(), String> {
        // A guest is told to the app with the list of guests (AgentEvent::Guests).
        if self.request.join {
            return Ok(());
        }
        self.claim()?;
        let _ = self.ctx.events.send(AgentEvent::SessionStarted {
            viewer_id: self.request.viewer_id.clone(),
            viewer_name: self.request.viewer_name.clone(),
            direct: self.direct,
            terminal: self.terminal_only.load(Ordering::Relaxed),
        });
        Ok(())
    }
}

/// Characterisation tests (Phase 2 of the full review) of the access gate of a
/// session (`Gate::authorize`, `Gate::check_code`): temporary and permanent
/// passwords, profiles, access rules, two-factor codes, hand-over tokens, support
/// tickets, confirmation by the person at the PC, restart. They pin the CURRENT
/// behaviour; the connection around it (Hello, AuthResult, pauses) is the end-to-end
/// tests' part.
#[cfg(test)]
mod gate_characterisation {
    use super::*;
    use crate::ending::end_reason;
    use crate::{
        AgentConfig, Arc, AtomicU32, AtomicU64, EncoderPlan, HashMap, Mutex, Notify, anyhow, close_reason, local_hello,
        mpsc, reopen, session,
    };
    use nuvira_proto::v1::Policy;
    use std::sync::OnceLock;

    const TEMPORARY: &str = "abcd2345";
    const PERMANENT: &str = "the permanent one";
    const PROFILE: &str = "view only please";
    const VIEWER: &str = "111 222 333";

    thread_local! {
        /// PBKDF2 checks of a permanent password made on this thread (the tests'
        /// runtime is single-threaded).
        pub(super) static VERIFY_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn verify_calls() -> u32 {
        VERIFY_CALLS.with(|c| c.get())
    }

    /// The PBKDF2 hashes, made once (300 000 iterations each).
    fn hashes() -> &'static (String, String) {
        static H: OnceLock<(String, String)> = OnceLock::new();
        H.get_or_init(|| (nuvira_secret::hash(PERMANENT), nuvira_secret::hash(PROFILE)))
    }

    struct Fixture {
        ctx: SessionContext,
        events: mpsc::UnboundedReceiver<AgentEvent>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.ctx.access_dir);
        }
    }

    fn fixture(tag: &str, permanent: bool, confirm: bool, rules: Option<&str>) -> Fixture {
        let dir = std::env::temp_dir().join(format!("nuvira-p2-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(rules) = rules {
            std::fs::write(dir.join(access::FILE), rules).unwrap();
        }
        let (events, rx) = mpsc::unbounded_channel();
        let ctx = SessionContext {
            config: AgentConfig {
                display: None,
                synthetic: Some((64, 64, 30)),
                bitrate_kbps: 1000,
                fps: 30,
                plan: EncoderPlan::Software,
                fixed_bitrate: false,
                view_only: false,
            },
            password: Arc::new(Mutex::new(TEMPORARY.to_string())),
            permanent: permanent.then(|| hashes().0.clone()),
            confirm,
            pending: Mutex::new(HashMap::new()),
            events,
            chat: Mutex::new(None),
            busy: AtomicU32::new(0),
            attempts: AtomicU32::new(0),
            claims: Mutex::new(()),
            waiting: Mutex::new(None),
            end_session: Notify::new(),
            ends: Default::default(),
            hosts: Default::default(),
            shared: Mutex::new(None),
            handover: Mutex::new(None),
            policy: Default::default(),
            support_ticket: Mutex::new(None),
            support_request: Mutex::new(None),
            allow_privacy: AtomicBool::new(true),
            allow_terminal: AtomicBool::new(true),
            allow_tunnel: AtomicBool::new(true),
            allow_print: AtomicBool::new(true),
            allow_recording: AtomicBool::new(true),
            lock_on_end: AtomicBool::new(false),
            lock_asked: AtomicBool::new(false),
            idle_minutes: AtomicU32::new(0),
            mic_muted: tokio::sync::watch::channel(false).0,
            access_dir: dir,
            last_code_step: AtomicU64::new(0),
            wrong_codes: Mutex::new(Vec::new()),
        guesses: Mutex::new(Default::default()),
        reports: Default::default(),
            features: Mutex::new(Vec::new()),
            unattended: permanent,
            restarting: Mutex::new(None),
            switches: Mutex::new(None),
        };
        Fixture { ctx, events: rx }
    }

    fn request(viewer: &str, join: bool) -> Request {
        Request { viewer_id: viewer.into(), viewer_name: "Tech".into(), limits: Policy::default(), join }
    }

    fn password(p: &str) -> Auth {
        Auth { password: p.into(), ..Default::default() }
    }

    /// What `authorize` said, the access noted for the report, the profile's switches off.
    struct Verdict {
        result: Result<(), String>,
        access: &'static str,
        profile_off: u8,
    }

    fn gate<'a>(
        ctx: &'a SessionContext,
        request: &'a Request,
        access: &'a std::sync::Mutex<&'static str>,
        session_id: &'a str,
    ) -> Gate<'a> {
        Gate {
            ctx,
            session_id,
            request,
            direct: true,
            path: "direct",
            access,
            terminal_only: AtomicBool::new(false),
            profile_off: AtomicU8::new(0),
            claimed: AtomicBool::new(false),
            throttled: AtomicBool::new(false),
            end_reason: AtomicBool::new(false),
        }
    }

    async fn authorize(ctx: &SessionContext, request: &Request, auth: &Auth) -> Verdict {
        let access = std::sync::Mutex::new("");
        let gate = gate(ctx, request, &access, "s1");
        let result = gate.authorize(auth, None).await;
        let profile_off = gate.profile_off.load(Ordering::Relaxed);
        drop(gate);
        Verdict { result, access: *access.lock().unwrap(), profile_off }
    }

    /// `authorize` while answering the confirmation request with `answer` (`None`:
    /// no answer). Returns the verdict and the events seen.
    async fn authorize_answering(
        f: &mut Fixture,
        request: &Request,
        auth: &Auth,
        answer: Option<bool>,
    ) -> (Verdict, Vec<String>) {
        let (ctx, events) = (&f.ctx, &mut f.events);
        let seen = std::cell::RefCell::new(Vec::new());
        let respond = async {
            while let Some(event) = events.recv().await {
                let name = serde_json::to_value(&event).unwrap()["event"].as_str().unwrap().to_string();
                seen.borrow_mut().push(name);
                match event {
                    AgentEvent::Incoming { session_id, .. } => {
                        if let Some(answer) = answer {
                            let tx = ctx.pending.lock().unwrap().remove(&session_id).unwrap();
                            let _ = tx.send(answer);
                        }
                    }
                    AgentEvent::IncomingClosed { .. } => break,
                    _ => {}
                }
            }
        };
        let (verdict, ()) = tokio::join!(authorize(ctx, request, auth), async {
            // Stops once authorize is done and nothing more comes.
            let _ = tokio::time::timeout(CONFIRM_TIMEOUT + Duration::from_secs(5), respond).await;
        });
        while let Ok(event) = f.events.try_recv() {
            seen.borrow_mut().push(serde_json::to_value(&event).unwrap()["event"].as_str().unwrap().to_string());
        }
        (verdict, seen.into_inner())
    }

    fn drain(f: &mut Fixture) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = f.events.try_recv() {
            out.push(serde_json::to_value(&event).unwrap()["event"].as_str().unwrap().to_string());
        }
        out
    }

    fn code_now(secret: &str) -> String {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        nuvira_secret::totp_code(secret, now / nuvira_secret::TOTP_STEP).unwrap()
    }

    #[tokio::test]
    async fn temporary_password() {
        let mut f = fixture("temp", false, false, None);
        let req = request(VIEWER, false);
        let v = authorize(&f.ctx, &req, &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Ok(()), "temporary"));
        assert_eq!(drain(&mut f), ["session_started"]);
        for wrong in ["", "ABCD2345", "abcd234", "abcd23456", " abcd2345"] {
            let v = authorize(&f.ctx, &req, &password(wrong)).await;
            assert_eq!((v.result, v.access), (Err(WRONG_PASSWORD.into()), "wrong_password"), "{wrong:?}");
        }
        assert!(drain(&mut f).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn wrong_passwords_lock_the_viewer_for_a_while() {
        // Review AGT-4 (Phase 6 lot 3; was the Phase 2 pin
        // `characterises_wrong_passwords_are_never_locked_out`, K15): after
        // guesses::MAX_WRONG wrong passwords from one viewer ID, that ID is refused
        // like a wrong password, also with the right one, without the password being
        // checked (no PBKDF2), until the lock ends. Other IDs are not affected.
        let f = fixture("lock", true, false, None);
        let (req, other) = (request(VIEWER, false), request("444 555 666", false));
        for i in 0..guesses::MAX_WRONG {
            let before = verify_calls();
            let v = authorize(&f.ctx, &req, &password(&format!("guess{i}"))).await;
            assert_eq!((v.result, v.access), (Err(WRONG_PASSWORD.into()), "wrong_password"));
            assert_eq!(verify_calls(), before + 1, "a wrong guess is checked");
        }
        let before = verify_calls();
        for p in [TEMPORARY, PERMANENT, "guess again"] {
            let access = std::sync::Mutex::new("");
            let g = gate(&f.ctx, &req, &access, "s1");
            assert_eq!(g.authorize(&password(p), None).await, Err(WRONG_PASSWORD.into()), "{p}");
            assert!(g.throttled.load(Ordering::Relaxed), "no 2 s pause for a refusal without a check");
            assert_eq!(*access.lock().unwrap(), "wrong_password");
        }
        assert_eq!(verify_calls(), before, "no PBKDF2 while the viewer is locked");
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0);
        // Another viewer ID still gets in.
        assert_eq!(authorize(&f.ctx, &other, &password(PERMANENT)).await.result, Ok(()));
        f.ctx.busy.store(0, Ordering::Relaxed);
        // Once the lock is over the right password works, and the wrong ones are forgotten.
        tokio::time::advance(guesses::FIRST_LOCK).await;
        assert_eq!(authorize(&f.ctx, &req, &password(PERMANENT)).await.result, Ok(()));
        f.ctx.busy.store(0, Ordering::Relaxed);
        for _ in 1..guesses::MAX_WRONG {
            authorize(&f.ctx, &req, &password("wrong")).await;
        }
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn wrong_temporary_passwords_also_lock_and_empty_ones_do_not_count() {
        let f = fixture("lock-temp", false, false, None);
        let req = request(VIEWER, false);
        for _ in 0..3 * guesses::MAX_WRONG {
            assert_eq!(authorize(&f.ctx, &req, &password("")).await.result, Err(WRONG_PASSWORD.into()));
        }
        for _ in 0..guesses::MAX_WRONG {
            authorize(&f.ctx, &req, &password("ABCD2345")).await;
        }
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Err(WRONG_PASSWORD.into()));
        tokio::time::advance(guesses::FIRST_LOCK).await;
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_spares_viewers_that_were_right_here() {
        // Review AGT-4: guesses spread over many new IDs use up the computer's
        // budget; then only IDs that already gave a right password get in (the
        // technician cannot be locked out by an attacker); new ones are refused
        // without a check until the wrong passwords are old enough.
        let f = fixture("budget", true, false, None);
        let known = request(VIEWER, false);
        assert_eq!(authorize(&f.ctx, &known, &password(PERMANENT)).await.result, Ok(()));
        f.ctx.busy.store(0, Ordering::Relaxed);
        for i in 0..guesses::BUDGET {
            let req = request(&format!("900 000 {i:03}"), false);
            assert_eq!(authorize(&f.ctx, &req, &password("guess")).await.result, Err(WRONG_PASSWORD.into()));
        }
        let before = verify_calls();
        let new = request("777 888 999", false);
        assert_eq!(authorize(&f.ctx, &new, &password(PERMANENT)).await.result, Err(WRONG_PASSWORD.into()));
        assert_eq!(verify_calls(), before, "refused before the check");
        assert_eq!(authorize(&f.ctx, &known, &password(PERMANENT)).await.result, Ok(()), "known viewer");
        f.ctx.busy.store(0, Ordering::Relaxed);
        assert_eq!(authorize(&f.ctx, &known, &password(TEMPORARY)).await.result, Ok(()), "known viewer");
        f.ctx.busy.store(0, Ordering::Relaxed);
        tokio::time::advance(guesses::WINDOW).await;
        assert_eq!(authorize(&f.ctx, &new, &password(PERMANENT)).await.result, Ok(()));
    }

    #[tokio::test]
    async fn permanent_password_and_profiles() {
        let rules = serde_json::json!({
            "profiles": [{ "name": "View", "password_hash": hashes().1, "off": ["input", "files"] }]
        })
        .to_string();
        let mut f = fixture("perm", true, true, Some(&rules));
        let req = request(VIEWER, false);
        // The permanent password is never confirmed, also when ctx.confirm is set.
        let v = authorize(&f.ctx, &req, &password(PERMANENT)).await;
        assert_eq!((v.result, v.access, v.profile_off), (Ok(()), "permanent", 0));
        let v = authorize(&f.ctx, &req, &password(PROFILE)).await;
        let off = switches::bit("input").unwrap() | switches::bit("files").unwrap();
        assert_eq!((v.result, v.access, v.profile_off), (Ok(()), "profile", off));
        assert_eq!(drain(&mut f), ["session_started", "session_started"]);
        // Without a main permanent password, a profile's password is not one: it is
        // compared with the temporary password.
        let f2 = fixture("perm-none", false, false, Some(&rules));
        let v = authorize(&f2.ctx, &req, &password(PROFILE)).await;
        assert_eq!((v.result, v.access), (Err(WRONG_PASSWORD.into()), "wrong_password"));
        let v = authorize(&f2.ctx, &req, &password(PERMANENT)).await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
    }

    #[tokio::test]
    async fn server_rule_confirm_always_asks_also_for_the_permanent_password() {
        let mut f = fixture("always", true, false, None);
        *f.ctx.policy.lock().unwrap() = Policy { confirm_always: true, ..Default::default() };
        let req = request(VIEWER, false);
        let (v, events) = authorize_answering(&mut f, &req, &password(PERMANENT), Some(true)).await;
        assert_eq!((v.result, v.access), (Ok(()), "confirmed"));
        assert_eq!(events, ["incoming", "incoming_closed", "session_started"]);
        // The same rule from the session's limits (Incoming.limits).
        let mut f = fixture("always-limits", true, false, None);
        let req = Request { limits: Policy { confirm_always: true, ..Default::default() }, ..request(VIEWER, false) };
        let (v, _) = authorize_answering(&mut f, &req, &password(TEMPORARY), Some(false)).await;
        assert_eq!((v.result, v.access), (Err("@agent.not_accepted".into()), "not_accepted"));
    }

    #[tokio::test(start_paused = true)]
    async fn confirmation_by_the_person_at_the_pc() {
        let req = request(VIEWER, false);
        let mut f = fixture("confirm", false, true, None);
        let (v, events) = authorize_answering(&mut f, &req, &password(TEMPORARY), Some(true)).await;
        assert_eq!((v.result, v.access), (Ok(()), "confirmed"));
        assert_eq!(events, ["incoming", "incoming_closed", "session_started"]);
        let (v, events) = authorize_answering(&mut f, &req, &password(TEMPORARY), Some(false)).await;
        assert_eq!((v.result, v.access), (Err("@agent.not_accepted".into()), "not_accepted"));
        assert_eq!(events, ["incoming", "incoming_closed"]);
        // No answer within CONFIRM_TIMEOUT (the clock is paused: no real wait).
        let start = tokio::time::Instant::now();
        let (v, events) = authorize_answering(&mut f, &req, &password(TEMPORARY), None).await;
        assert_eq!((v.result, v.access), (Err("@agent.no_answer".into()), "no_answer"));
        assert_eq!(events, ["incoming", "incoming_closed"]);
        assert!(start.elapsed() >= CONFIRM_TIMEOUT);
        assert!(f.ctx.pending.lock().unwrap().is_empty());
        // A wrong password is refused before anyone is asked.
        let (v, events) = authorize_answering(&mut f, &req, &password("nope"), Some(true)).await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn allow_list() {
        let only = r#"{"allow":[{"id":"999 888 777"}]}"#;
        let attended_too = r#"{"allow":[{"id":"999888777"}],"allow_attended":true}"#;
        let (other, listed) = (request(VIEWER, false), request("999-888-777", false));

        let f = fixture("allow", true, false, Some(only));
        let v = authorize(&f.ctx, &other, &password(PERMANENT)).await;
        assert_eq!((v.result, v.access), (Err(NOT_ALLOWED.into()), "not_allowed"));
        assert_eq!(authorize(&f.ctx, &listed, &password(PERMANENT)).await.result, Ok(()));
        // Without allow_attended the temporary password works from any computer.
        assert_eq!(authorize(&f.ctx, &other, &password(TEMPORARY)).await.result, Ok(()));

        let f = fixture("allow-att", true, false, Some(attended_too));
        let v = authorize(&f.ctx, &other, &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Err(NOT_ALLOWED.into()), "not_allowed"));
        // The password is checked first: a wrong one from such a computer is "wrong".
        let v = authorize(&f.ctx, &other, &password("nope")).await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
        assert_eq!(authorize(&f.ctx, &listed, &password(TEMPORARY)).await.result, Ok(()));
        // An empty viewer ID is never on the list.
        let v = authorize(&f.ctx, &request("", false), &password(PERMANENT)).await;
        assert_eq!(v.result, Err(NOT_ALLOWED.into()));
    }

    #[tokio::test]
    async fn characterises_unreadable_rules_refuse_only_permanent_passwords() {
        // An access.json that does not parse refuses every permanent password
        // (documented in access.rs). The temporary password is still accepted from
        // any computer, even if the broken file meant to restrict it
        // (`allow_attended`): `attended_allowed` falls back to true. Kept on purpose
        // (review AGT-5, user decision of 2026-11-18; docs/42-access-rules.md).
        let f = fixture("broken", true, false, Some(r#"{"allow":[{"id":"999888777"}],"allow_attended":true,"#));
        let req = request(VIEWER, false);
        let v = authorize(&f.ctx, &req, &password(PERMANENT)).await;
        assert_eq!((v.result, v.access), (Err("@agent.access_rules_invalid".into()), "not_allowed"));
        let v = authorize(&f.ctx, &req, &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Ok(()), "temporary"));
        // A UTF-8 BOM at the start is accepted (Notepad).
        let f = fixture("bom", true, false, Some("\u{feff}{\"allow\":[{\"id\":\"999888777\"}]}"));
        assert_eq!(authorize(&f.ctx, &req, &password(PERMANENT)).await.result, Err(NOT_ALLOWED.into()));
    }

    #[tokio::test]
    async fn two_factor_codes() {
        let secret = nuvira_secret::totp_secret();
        let rules = serde_json::json!({ "totp": secret }).to_string();
        let f = fixture("totp", true, false, Some(&rules));
        let req = request(VIEWER, false);
        let with_code = |code: &str| Auth { password: PERMANENT.into(), code: code.into(), ..Default::default() };
        // No code: asked for (not counted as wrong).
        for _ in 0..10 {
            assert_eq!(authorize(&f.ctx, &req, &with_code(" ")).await.result, Err(CODE_REQUIRED.into()));
        }
        assert!(f.ctx.wrong_codes.lock().unwrap().is_empty());
        // The temporary password needs no code.
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
        // A right code works once.
        let code = code_now(&secret);
        let v = authorize(&f.ctx, &req, &with_code(&code)).await;
        assert_eq!((v.result, v.access), (Ok(()), "permanent"));
        let v = authorize(&f.ctx, &req, &with_code(&code)).await;
        assert_eq!((v.result, v.access), (Err("@agent.code_wrong".into()), "wrong_code"), "already used");
        // A wrong password with a code: the password is wrong, the code not looked at.
        let v = authorize(&f.ctx, &req, &Auth { password: "nope".into(), code: "000000".into(), ..Default::default() })
            .await;
        assert_eq!(v.result, Err(WRONG_PASSWORD.into()));
        assert_eq!(f.ctx.wrong_codes.lock().unwrap().len(), 1, "only the reused code");
    }

    #[tokio::test]
    async fn five_wrong_codes_lock_the_permanent_passwords() {
        let secret = nuvira_secret::totp_secret();
        let rules = serde_json::json!({ "totp": secret }).to_string();
        let f = fixture("totp-lock", true, false, Some(&rules));
        let req = request(VIEWER, false);
        let with_code = |code: &str| Auth { password: PERMANENT.into(), code: code.into(), ..Default::default() };
        let right = code_now(&secret);
        let wrong = if right == "123456" { "654321" } else { "123456" };
        for _ in 0..CODE_MAX_WRONG {
            assert_eq!(authorize(&f.ctx, &req, &with_code(wrong)).await.result, Err("@agent.code_wrong".into()));
        }
        // Locked: even the right code, and even an empty one, is refused.
        let v = authorize(&f.ctx, &req, &with_code(&right)).await;
        assert_eq!((v.result, v.access), (Err("@agent.code_locked".into()), "code_locked"));
        assert_eq!(authorize(&f.ctx, &req, &with_code("")).await.result, Err("@agent.code_locked".into()));
        // The lock does not grow while locked.
        assert_eq!(f.ctx.wrong_codes.lock().unwrap().len(), CODE_MAX_WRONG);
        // The temporary password is not locked.
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
        // After CODE_WINDOW the wrong codes are forgotten.
        let long_ago = Instant::now().checked_sub(CODE_WINDOW + Duration::from_secs(1)).unwrap();
        f.ctx.wrong_codes.lock().unwrap().iter_mut().for_each(|t| *t = long_ago);
        assert_eq!(authorize(&f.ctx, &req, &with_code(&right)).await.result, Ok(()));
    }

    #[tokio::test]
    async fn hand_over_tokens() {
        let mut f = fixture("handover", false, true, None);
        let token = "token-0123456789";
        let set = |f: &Fixture, viewer: &str, age: Duration| {
            *f.ctx.handover.lock().unwrap() = Some(shared::Handover {
                viewer_id: viewer.into(),
                token: token.into(),
                at: Instant::now().checked_sub(age).unwrap(),
            });
        };
        let with = |t: &str| Auth { handover: t.into(), ..Default::default() };
        let req = request(VIEWER, false);
        // Right viewer, right token, in time: no confirmation (ctx.confirm is set),
        // and only once.
        set(&f, VIEWER, Duration::ZERO);
        let v = authorize(&f.ctx, &req, &with(token)).await;
        assert_eq!((v.result, v.access), (Ok(()), "handover"));
        assert_eq!(drain(&mut f), ["session_started"]);
        assert!(f.ctx.handover.lock().unwrap().is_none());
        assert_eq!(authorize(&f.ctx, &req, &with(token)).await.result, Err(WRONG_PASSWORD.into()));
        // Wrong token, another viewer (IDs compared as given), too late, or a join.
        for (viewer, age, auth, join) in [
            (VIEWER, Duration::ZERO, with("token-x"), false),
            ("111222333", Duration::ZERO, with(token), false),
            (VIEWER, shared::HANDOVER_VALID, with(token), false),
            (VIEWER, Duration::ZERO, with(token), true),
        ] {
            set(&f, viewer, age);
            let v = authorize(&f.ctx, &request(VIEWER, join), &auth).await;
            assert_eq!((v.result, v.access), (Err(WRONG_PASSWORD.into()), "wrong_password"), "{viewer} {age:?}");
            // A failed attempt leaves the token there.
            assert!(f.ctx.handover.lock().unwrap().is_some());
        }
        // A token beats the password: a right temporary password with a wrong token is refused.
        set(&f, VIEWER, Duration::ZERO);
        let auth = Auth { password: TEMPORARY.into(), handover: "bad".into(), ..Default::default() };
        assert_eq!(authorize(&f.ctx, &req, &auth).await.result, Err(WRONG_PASSWORD.into()));
    }

    #[tokio::test]
    async fn hand_over_and_ticket_follow_allow_attended() {
        let f = fixture("handover-allow", false, false, Some(r#"{"allow":[{"id":"999888777"}],"allow_attended":true}"#));
        *f.ctx.handover.lock().unwrap() =
            Some(shared::Handover { viewer_id: VIEWER.into(), token: "t".into(), at: Instant::now() });
        let req = request(VIEWER, false);
        let v = authorize(&f.ctx, &req, &Auth { handover: "t".into(), ..Default::default() }).await;
        assert_eq!((v.result, v.access), (Err(NOT_ALLOWED.into()), "not_allowed"));
        let v = authorize(&f.ctx, &req, &Auth { support_ticket: "x".into(), ..Default::default() }).await;
        assert_eq!(v.result, Err(NOT_ALLOWED.into()));
    }

    #[tokio::test]
    async fn support_tickets_still_ask_the_person_at_the_pc() {
        let mut f = fixture("ticket", false, false, None);
        *f.ctx.support_ticket.lock().unwrap() = Some("ticket-1".into());
        let req = request(VIEWER, false);
        let with = |t: &str| Auth { support_ticket: t.into(), ..Default::default() };
        let v = authorize(&f.ctx, &req, &with("ticket-2")).await;
        assert_eq!((v.result, v.access), (Err("@agent.support_ticket_invalid".into()), "wrong_password"));
        // The right ticket: asked even though ctx.confirm is off; used up at once.
        let (v, events) = authorize_answering(&mut f, &req, &with("ticket-1"), Some(true)).await;
        assert_eq!((v.result, v.access), (Ok(()), "confirmed"));
        assert_eq!(events, ["incoming", "incoming_closed", "session_started"]);
        assert!(f.ctx.support_ticket.lock().unwrap().is_none());
        let v = authorize(&f.ctx, &req, &with("ticket-1")).await;
        assert_eq!(v.result, Err("@agent.support_ticket_invalid".into()));
    }

    #[tokio::test]
    async fn a_restart_under_way_refuses_everyone_for_three_minutes() {
        let f = fixture("restart", true, false, None);
        let req = request(VIEWER, false);
        *f.ctx.restarting.lock().unwrap() = Some(Instant::now());
        for p in [TEMPORARY, PERMANENT, "wrong"] {
            assert_eq!(authorize(&f.ctx, &req, &password(p)).await.result, Err("@agent.restarting".into()));
        }
        let given_up = Instant::now().checked_sub(RESTART_GIVEN_UP).unwrap();
        *f.ctx.restarting.lock().unwrap() = Some(given_up);
        assert_eq!(authorize(&f.ctx, &req, &password(TEMPORARY)).await.result, Ok(()));
    }

    #[tokio::test]
    async fn one_session_is_claimed_at_a_time_once_the_password_is_right() {
        // Review AGT-1 (Phase 6 lot 2): a viewer is not counted as busy before its
        // credentials are right; the first right one takes the computer, a second
        // one is refused as busy until the first ends; a guest is not counted.
        let mut f = fixture("claim", false, false, None);
        let (req, other) = (request(VIEWER, false), request("444 555 666", false));
        let (a1, a2, a3) = (Mutex::new(""), Mutex::new(""), Mutex::new(""));
        let first = gate(&f.ctx, &req, &a1, "s1");
        assert_eq!(first.authorize(&password("nope"), None).await, Err(WRONG_PASSWORD.into()));
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0, "a wrong password claims nothing");
        assert_eq!(first.authorize(&password(TEMPORARY), None).await, Ok(()));
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 1);
        let second = gate(&f.ctx, &other, &a2, "s2");
        assert_eq!(second.authorize(&password(TEMPORARY), None).await, Err("@agent.busy".into()));
        let guest_req = request("444 555 666", true);
        let guest = gate(&f.ctx, &guest_req, &a3, "s3");
        assert_eq!(guest.authorize(&password(TEMPORARY), None).await, Ok(()));
        drop(guest);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 1);
        // The first session ends: the next one is let in.
        drop(first);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0);
        assert_eq!(second.authorize(&password(TEMPORARY), None).await, Ok(()));
        drop(second);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0);
        assert_eq!(drain(&mut f), ["session_started", "session_started"]);
    }

    #[tokio::test]
    async fn the_person_at_the_pc_is_asked_about_one_viewer_at_a_time() {
        // Review AGT-1: the session is claimed before the confirmation, so a second
        // viewer with the right password while the first waits is refused as busy
        // without asking the person at the PC again.
        let mut f = fixture("claim-confirm", false, true, None);
        let (req, other) = (request(VIEWER, false), request("444 555 666", false));
        let (a1, a2) = (Mutex::new(""), Mutex::new(""));
        let first = gate(&f.ctx, &req, &a1, "s1");
        let second = gate(&f.ctx, &other, &a2, "s2");
        let (ctx, events) = (&f.ctx, &mut f.events);
        let right = password(TEMPORARY);
        let (r1, r2) = tokio::join!(first.authorize(&right, None), async {
            let Some(AgentEvent::Incoming { session_id, .. }) = events.recv().await else {
                panic!("the first viewer was not asked about");
            };
            assert_eq!(session_id, "s1");
            let r2 = second.authorize(&right, None).await;
            ctx.pending.lock().unwrap().remove("s1").unwrap().send(true).unwrap();
            r2
        });
        assert_eq!((r1, r2), (Ok(()), Err("@agent.busy".into())));
        drop((first, second));
        assert_eq!(drain(&mut f), ["incoming_closed", "session_started"]);
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_guest_joining_is_not_announced_as_a_session() {
        let mut f = fixture("join", false, false, None);
        let v = authorize(&f.ctx, &request(VIEWER, true), &password(TEMPORARY)).await;
        assert_eq!((v.result, v.access), (Ok(()), "temporary"));
        assert!(drain(&mut f).is_empty(), "no SessionStarted for a guest");
    }

    /// A reopen by itself (`Hello.reopen`) after a normal end of that viewer's last
    /// session is refused; after a loss, with no record, from another viewer or
    /// asked by the person (no flag) it is not (MT-P2-11 F2 case 1).
    #[tokio::test]
    async fn an_automatic_reopen_after_a_normal_end_is_refused() {
        let f = fixture("reopen", false, false, None);
        let req = request(VIEWER, false);
        let access = std::sync::Mutex::new("");
        let gate = gate(&f.ctx, &req, &access, "s1");
        let reopen = Hello { reopen: true, ..Default::default() };
        let asked = Hello::default();
        assert!(gate.refuse_reopen(&reopen).is_ok(), "no record: as before");
        f.ctx.ends.record(VIEWER, reopen::Ended::Normal);
        let e = gate.refuse_reopen(&reopen).unwrap_err();
        assert_eq!(format!("{e:#}"), "@agent.ended_at_computer");
        assert!(gate.refuse_reopen(&asked).is_ok(), "asked by the person: not refused");
        f.ctx.ends.record("someone-else", reopen::Ended::Lost);
        assert!(gate.refuse_reopen(&reopen).is_err(), "another viewer's record changes nothing");
        f.ctx.ends.record(VIEWER, reopen::Ended::Lost);
        assert!(gate.refuse_reopen(&reopen).is_ok(), "after a loss: reopened");
    }

    /// The person at the PC ends the session (Phase 7 F-END-MSG): a viewer that
    /// said so in its Hello (`end_reason`) reads "@agent.ended_at_computer" as the
    /// close reason; an older viewer (no flag) reads "end of session" as before.
    /// Any other end keeps its own reason, and the flag is kept from each Hello.
    #[tokio::test]
    async fn the_end_at_the_computer_says_so_to_viewers_that_show_it() {
        let f = fixture("end-reason", false, false, None);
        let req = request(VIEWER, false);
        let access = std::sync::Mutex::new("");
        let gate = gate(&f.ctx, &req, &access, "s1");
        assert!(!gate.end_reason.load(Ordering::Relaxed));
        gate.refuse_reopen(&Hello { end_reason: true, ..Default::default() }).unwrap();
        assert!(gate.end_reason.load(Ordering::Relaxed), "kept from the Hello");
        gate.refuse_reopen(&Hello::default()).unwrap();
        assert!(!gate.end_reason.load(Ordering::Relaxed), "an older viewer's Hello");

        assert_eq!(end_reason(&Ok(()), true), b"@agent.ended_at_computer".to_vec());
        assert_eq!(end_reason(&Ok(()), false), b"end of session".to_vec(), "older viewers: as before");
        let idle = || Err(anyhow!("@agent.idle_ended").context("session"));
        assert_eq!(end_reason(&idle(), true), b"@agent.idle_ended".to_vec(), "another end keeps its key");
        assert_eq!(end_reason(&Err(anyhow!("timed out")), true), b"end of session".to_vec());
        // Still a normal end: recorded as such (an automatic reopen is refused after it).
        assert_eq!(reopen::Ended::of(&Ok(())), reopen::Ended::Normal);
    }

    /// The refusal comes right after the viewer's Hello, on the real session path:
    /// the agent sends nothing (no Hello, so no password is asked for or read) and
    /// the session ends with the key, which the caller gives as the close reason.
    #[tokio::test]
    async fn the_refused_reopen_ends_before_the_password() {
        let f = fixture("reopen-session", false, false, None);
        f.ctx.ends.record(VIEWER, reopen::Ended::Normal);
        let req = request(VIEWER, false);
        let access = std::sync::Mutex::new("");
        let gate = gate(&f.ctx, &req, &access, "s1");
        let (endpoint, fingerprint) =
            nuvira_transport::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let accepted = tokio::spawn(async move { endpoint.accept().await.unwrap().await.unwrap() });
        let viewer = nuvira_transport::connect(addr, Some(fingerprint)).await.unwrap();
        let agent = accepted.await.unwrap();
        let (send, recv) = viewer.open_bi().await.unwrap();
        let (mut tx, mut rx) = (FramedSend::new(send), FramedRecv::new(recv));
        tx.send(&control(Msg::Hello(Hello { reopen: true, ..local_hello() }))).await.unwrap();
        let streams = agent.accept_bi().await.unwrap();
        let result = session(agent.clone(), streams, &f.ctx.config, Some(&gate), None).await;
        let e = result.unwrap_err();
        assert!(format!("{e:#}").contains("@agent.ended_at_computer"), "{e:#}");
        assert_eq!(close_reason(&Err(e)), b"@agent.ended_at_computer".to_vec());
        agent.close(0u32.into(), b"@agent.ended_at_computer");
        let next = rx.recv::<ControlMessage>().await;
        assert!(!matches!(next, Ok(Some(_))), "the agent sent something: {next:?}");
        assert_eq!(f.ctx.busy.load(Ordering::Relaxed), 0, "nothing claimed");
    }
}
