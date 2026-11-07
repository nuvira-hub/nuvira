//! Privacy mode on macOS.
//!
//! - Black screens: AppKit windows must be made on a main thread, and the agent's
//!   main thread runs tokio (or Tauri, inside the app). The windows are therefore
//!   shown by a child process, the same program started with `--privacy-overlay`
//!   ([`overlay_if_asked`]): one borderless black window per screen at the
//!   shielding level, above the menu bar, the Dock and full-screen apps, letting
//!   clicks through. The child ends when its standard input closes, so the black
//!   screens go away with the agent even if the agent dies.
//! - Capture: ScreenCaptureKit leaves out the windows of the child (filter by
//!   process, see `nuvira_capture::hide_process`). The windows start transparent
//!   and turn black only once the capture leaves them out, so the technician never
//!   sees them; `NSWindowSharingNone` is set as well.
//! - Local mouse and keyboard: an active event tap in the agent's session drops
//!   every event without [`INJECTED_TAG`] in its user-data field (set on the
//!   events NuviraHub injects). It needs the Accessibility permission, which
//!   remote control needs anyway. As on Windows, some system keys cannot be
//!   blocked (power button, Touch ID): the person at the Mac can always lock it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use core_foundation::base::TCFType;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::event::{
    CGEventField, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use tracing::{info, warn};

/// User-data field of the events NuviraHub injects (`kCGEventSourceUserData`).
pub(crate) const USER_DATA: CGEventField = EventField::EVENT_SOURCE_USER_DATA;
/// Value of [`USER_DATA`] on injected events: the only ones privacy mode lets through.
pub(crate) const INJECTED_TAG: i64 = 0x4e55_5652; // "NUVR"

/// Argument that turns the program into the black-screen child process.
const OVERLAY_ARG: &str = "--privacy-overlay";
/// Message shown on the black screens, given to the child.
const MESSAGE_ENV: &str = "NUVIRA_PRIVACY_MESSAGE";
/// How long the child has to show its (still transparent) windows.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

pub fn available() -> bool {
    true
}

pub struct Overlay {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    tap: Option<InputBlock>,
}

impl Overlay {
    pub fn start(message: &str, hide_from_capture: &dyn Fn(u32)) -> Result<Self> {
        // The local input first: without Accessibility there is no privacy mode.
        let tap = InputBlock::start()?;
        let exe = std::env::current_exe().context("program path")?;
        let mut child = Command::new(exe)
            .arg(OVERLAY_ARG)
            .env(MESSAGE_ENV, message)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("black screens not started")?;
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take().context("output of the black screens")?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("privacy-overlay".into()).spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        })?;
        let line = match rx.recv_timeout(READY_TIMEOUT) {
            Ok(line) if line.starts_with("ready") => line,
            other => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("black screens not shown ({other:?})");
            }
        };
        info!(pid = child.id(), windows = line.trim(), "privacy black screens ready");
        // Left out of the capture before they turn black.
        hide_from_capture(child.id());
        if let Some(input) = stdin.as_mut() {
            writeln!(input, "show").and_then(|()| input.flush()).context("black screens")?;
        }
        info!("privacy mode on");
        Ok(Self { child: Some(child), stdin, tap: Some(tap) })
    }

    pub fn stop(&mut self) {
        // Closing its input ends the child (and its windows).
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        warn!("black screens did not end: stopped");
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        if let Some(tap) = self.tap.take() {
            tap.stop();
            info!("privacy mode off");
        }
    }
}

/// Mach port of the running tap, to switch it on again when macOS turns it off
/// (callback too slow, or secure input).
static TAP_PORT: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
}

/// The event tap that drops the local mouse and keyboard, on its own run loop.
struct InputBlock {
    run_loop: CFRunLoop,
    thread: std::thread::JoinHandle<()>,
}

impl InputBlock {
    fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Result<CFRunLoop, ()>>();
        let thread = std::thread::Builder::new().name("privacy-input".into()).spawn(move || {
            let events = vec![
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGEventType::MouseMoved,
                CGEventType::LeftMouseDragged,
                CGEventType::RightMouseDragged,
                CGEventType::KeyDown,
                CGEventType::KeyUp,
                CGEventType::FlagsChanged,
                CGEventType::ScrollWheel,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGEventType::OtherMouseDragged,
            ];
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                events,
                |_proxy, kind, event| match kind {
                    CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                        let port = TAP_PORT.load(Ordering::Acquire);
                        if !port.is_null() {
                            // SAFETY: the port is valid while the tap lives (cleared before it is dropped).
                            unsafe { CGEventTapEnable(port, true) };
                        }
                        CallbackResult::Keep
                    }
                    _ if event.get_integer_value_field(USER_DATA) == INJECTED_TAG => CallbackResult::Keep,
                    _ => CallbackResult::Drop,
                },
            );
            let Ok(tap) = tap else {
                let _ = tx.send(Err(()));
                return;
            };
            let Ok(source) = tap.mach_port().create_runloop_source(0) else {
                let _ = tx.send(Err(()));
                return;
            };
            let run_loop = CFRunLoop::get_current();
            // SAFETY: the static mode constant of CoreFoundation.
            run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
            tap.enable();
            TAP_PORT.store(tap.mach_port().as_concrete_TypeRef().cast(), Ordering::Release);
            let _ = tx.send(Ok(run_loop));
            CFRunLoop::run_current();
            TAP_PORT.store(std::ptr::null_mut(), Ordering::Release);
            drop(tap);
        })?;
        match rx.recv() {
            Ok(Ok(run_loop)) => Ok(Self { run_loop, thread }),
            _ => {
                let _ = thread.join();
                // Creating an active tap fails without the Accessibility permission.
                Err(anyhow!("event tap not created")).context("@agent.privacy_permission_mac")
            }
        }
    }

    fn stop(self) {
        self.run_loop.stop();
        let _ = self.thread.join();
    }
}

/// Runs the black-screen child process when the program was started for it
/// (never returns then); otherwise does nothing. Call it first thing in `main`.
pub fn overlay_if_asked() {
    if std::env::args_os().nth(1).is_some_and(|a| a == OVERLAY_ARG) {
        let message = std::env::var(MESSAGE_ENV).unwrap_or_default();
        overlay::run(&message);
    }
}

mod overlay {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSFont, NSScreen,
        NSTextAlignment, NSTextField, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        /// Level of the windows above everything but the cursor (`CGShieldingWindowLevel`).
        fn CGShieldingWindowLevel() -> i32;
    }

    pub fn run(message: &str) -> ! {
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("privacy overlay: not on the main thread");
            std::process::exit(2);
        };
        let app = NSApplication::sharedApplication(mtm);
        // No Dock icon, no menu bar of its own.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let windows: Vec<Retained<NSWindow>> =
            NSScreen::screens(mtm).iter().map(|screen| black_window(mtm, screen.frame(), message)).collect();
        let numbers: Vec<String> = windows.iter().map(|w| w.windowNumber().to_string()).collect();
        println!("ready {}", numbers.join(" "));
        let _ = std::io::Write::flush(&mut std::io::stdout());

        std::thread::spawn(|| {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) if line.trim() == "show" => {
                        dispatch2::DispatchQueue::main().exec_async(|| {
                            // SAFETY: work given to the main queue runs on the main thread.
                            let mtm = unsafe { MainThreadMarker::new_unchecked() };
                            for window in NSApplication::sharedApplication(mtm).windows().iter() {
                                window.setAlphaValue(1.0);
                            }
                        });
                    }
                    Ok(_) => {}
                }
            }
        });
        app.run();
        drop(windows);
        std::process::exit(0);
    }

    fn black_window(mtm: MainThreadMarker, frame: NSRect, message: &str) -> Retained<NSWindow> {
        // SAFETY: plain window creation on the main thread; released by `Retained`.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setLevel(unsafe { CGShieldingWindowLevel() } as isize);
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        window.setOpaque(false);
        window.setHasShadow(false);
        window.setIgnoresMouseEvents(true);
        window.setSharingType(NSWindowSharingType::None);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        // Transparent until the capture leaves it out ("show" from the agent).
        window.setAlphaValue(0.0);

        let label = NSTextField::wrappingLabelWithString(&NSString::from_str(message), mtm);
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setFont(Some(&NSFont::systemFontOfSize(26.0)));
        label.setAlignment(NSTextAlignment::Center);
        let (w, h) = (frame.size.width * 0.7, 160.0);
        label.setFrame(NSRect::new(
            NSPoint::new((frame.size.width - w) / 2.0, (frame.size.height - h) / 2.0),
            NSSize::new(w, h),
        ));
        if let Some(view) = window.contentView() {
            view.addSubview(&label);
        }
        window.setFrame_display(frame, true);
        window.orderFrontRegardless();
        window
    }
}
