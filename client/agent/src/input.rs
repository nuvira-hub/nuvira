//! The viewer's input on this computer: the thread that injects it, and the keys
//! of a paste.

use anyhow::Result;
use nuvira_proto::v1::InputEvent;
#[cfg(windows)]
use tracing::info;
use tracing::warn;

/// The keys of a Clipboard.paste, pressed and released in order.
pub(crate) fn paste_keys(paste: nuvira_proto::v1::Paste) -> Vec<InputEvent> {
    use nuvira_proto::v1::Paste;
    let combo: &[&str] = match paste {
        Paste::None => &[],
        Paste::V => &["KeyV"],
        Paste::Shortcut if cfg!(target_os = "macos") => &["MetaLeft", "KeyV"],
        Paste::Shortcut => &["ControlLeft", "KeyV"],
        Paste::Terminal => &["ControlLeft", "ShiftLeft", "KeyV"],
    };
    let key = |code: &str, down| InputEvent {
        event: Some(nuvira_proto::v1::input_event::Event::Key(nuvira_proto::v1::Key { code: code.into(), down })),
    };
    let downs = combo.iter().map(|c| key(c, true));
    let ups = combo.iter().rev().map(|c| key(c, false));
    downs.chain(ups).collect()
}

/// What the input thread receives.
pub(crate) enum InputCmd {
    Event(InputEvent),
    /// Screen change: mouse coordinates refer to this one.
    Screen(nuvira_input::ScreenRect),
}

/// Injects input on a dedicated thread, which can follow the active desktop
/// (UAC, lock screen) without touching tokio's threads. Closing the channel
/// ends the thread, which releases the keys still pressed.
pub(crate) fn input_thread(
    mut injector: Box<dyn nuvira_input::InputInjector>,
) -> Result<std::sync::mpsc::Sender<InputCmd>> {
    let (tx, rx) = std::sync::mpsc::channel::<InputCmd>();
    std::thread::Builder::new().name("input".into()).spawn(move || {
        for cmd in rx {
            let event = match cmd {
                InputCmd::Event(event) => event,
                InputCmd::Screen(screen) => {
                    injector.set_screen(screen);
                    continue;
                }
            };
            #[cfg(windows)]
            if let Some(name) = nuvira_capture::desktop::follow_input_desktop() {
                info!("input on desktop {name}");
            }
            if let Err(e) = injector.inject(&event) {
                warn!("input not injected: {e:#}");
            }
        }
    })?;
    Ok(tx)
}
