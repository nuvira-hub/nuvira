//! macOS: `nuvirahub://` links that reach the agent instead of the app.
//!
//! The agent of unattended access is this program, started from the same bundle
//! by its LaunchAgent. Once it talks to the window server (capture, clipboard,
//! input), Launch Services counts it as a running NuviraHub: a link opened while
//! the app is closed is sent to the agent as an Apple event, and nobody answers it
//! (Safari: "The application NuviraHub is not open anymore"). So the agent runs its
//! work on another thread and keeps the main thread in an `NSApplication` loop,
//! with no Dock icon, to take the link and start the app with it; the app takes a
//! link from its arguments (and hands it to the running app, if any).
//!
//! For the same reason, opening NuviraHub (Dock, Finder, `open -a`) while the agent
//! runs reaches the agent as a "reopen" event: it starts the app too.
//!
//! And "quit NuviraHub" reaches it as well: macOS's "Quit & Reopen" after a
//! permission is granted (Screen Recording), `osascript … to quit`. NSApplication
//! would end the process with 0, which the LaunchAgents of the whole-Mac
//! installation and of the sign-in screen take as "nothing to do here" (`KeepAlive`
//! → `SuccessfulExit` false): the Mac stayed unreachable until the next sign-in.
//! The agent ends with the code of a restart instead, and launchd starts it again,
//! with the new permission.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject};
use objc2::{AnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSString;

/// 'GURL': the class and the ID of the "open this URL" Apple event.
const GET_URL: u32 = u32::from_be_bytes(*b"GURL");
/// '----': the event's direct object, the URL.
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NuviraAgentLinks"]
    struct Links;

    impl Links {
        #[unsafe(method(getUrl:withReply:))]
        fn get_url(&self, event: &AnyObject, _reply: &AnyObject) {
            let param: Option<Retained<AnyObject>> =
                unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
            let url: Option<Retained<NSString>> = param.and_then(|p| unsafe { msg_send![&*p, stringValue] });
            if let Some(url) = url {
                let url = url.to_string();
                if url.starts_with("nuvirahub:") {
                    tracing::info!("nuvirahub:// link opened while the app is closed: starting the app");
                    start_app(Some(&url));
                }
            }
        }

        /// The app delegate's "reopen" (NSApplication answers 'rapp' itself and
        /// asks its delegate).
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &AnyObject, _visible: Bool) -> Bool {
            tracing::info!("NuviraHub opened while only the agent runs: starting the app");
            start_app(None);
            Bool::NO
        }

        /// "Quit" (the 'quit' Apple event NSApplication turns into `terminate:`):
        /// the agent ends so that launchd starts it again (see the module notes).
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &AnyObject) -> usize {
            tracing::info!("asked to quit (macOS after a permission change, or a quit event): the agent starts again");
            std::process::exit(nuvira_update::unix::RESTART_EXIT)
        }
    }
);

/// Starts the app, with the link if any: a new start, which hands over to the
/// running app if there is one (one app at a time).
fn start_app(url: Option<&str>) {
    // .../NuviraHub.app/Contents/MacOS/nuvira
    let Some(bundle) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(|p| p.to_path_buf()))
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
    else {
        tracing::warn!("not in an app bundle: the app is not started");
        return;
    };
    let mut open = std::process::Command::new("/usr/bin/open");
    open.arg("-n").arg(&bundle);
    if let Some(url) = url {
        open.arg("--args").arg(url);
    }
    match open.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::warn!("the app did not start: {e}"),
    }
}

/// Runs the agent (`work`) on another thread and takes the links on this one, the
/// main thread. Never returns: the process ends with the agent's exit code.
pub fn agent_with_links(work: impl FnOnce() -> i32 + Send + 'static) -> ! {
    let Some(mtm) = MainThreadMarker::new() else {
        std::process::exit(work());
    };
    // NSApplication reads the bundle: one being replaced (an update renames the old
    // one aside, then the new one in) made it crash on a null executable URL, with
    // the system's "quit unexpectedly". Without a whole bundle the agent just runs
    // (launchd starts it again after the update anyway).
    let whole = std::env::current_exe().ok().is_some_and(|exe| {
        exe.is_file() && exe.ancestors().nth(2).is_some_and(|contents| contents.join("Info.plist").is_file())
    });
    if !whole {
        tracing::warn!("app bundle incomplete: nuvirahub:// links not taken by this agent");
        std::process::exit(work());
    }
    std::thread::Builder::new()
        .name("agent".into())
        .spawn(move || std::process::exit(work()))
        .expect("agent thread");
    let app = NSApplication::sharedApplication(mtm);
    // Accessory, not Prohibited: a background-only app gets no "reopen".
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let links: Retained<Links> = unsafe { msg_send![Links::alloc(), init] };
    unsafe {
        let manager: Retained<AnyObject> =
            msg_send![objc2::class!(NSAppleEventManager), sharedAppleEventManager];
        let _: () = msg_send![
            &*manager,
            setEventHandler: &*links,
            andSelector: sel!(getUrl:withReply:),
            forEventClass: GET_URL,
            andEventID: GET_URL
        ];
        // "Reopen" (Dock, Finder, `open -a`) comes to the delegate.
        let _: () = msg_send![&*app, setDelegate: &*links];
    }
    // The handler and the delegate (held weakly) live as long as the process.
    std::mem::forget(links);
    app.run();
    // `run` returns only if something stops the loop: the agent goes on.
    loop {
        std::thread::park();
    }
}
