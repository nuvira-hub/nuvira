//! The keyboard map that the Wayland compositor gives its programs, read once as
//! a client (`wl_keyboard.keymap`) and compiled with libxkbcommon (loaded at run
//! time, as winit does): which characters the layout makes, and with which keysym.
//!
//! Why: the compositors type a keysym (`NotifyKeyboardKeysym`) only when one of
//! the layout's keys makes it; GNOME writes "No keycode found for keyval" and
//! types nothing otherwise. And a character may have an older keysym than its
//! Unicode one ("€" is EuroSign, 0x20AC, not 0x10020AC).

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use xkbcommon_dl::{xkb_context_flags, xkb_keymap_compile_flags, xkb_keymap_format};

/// Levels looked at: plain, Shift, AltGr. The compositors press those modifiers
/// themselves; with AltGr+Shift GNOME typed the wrong letter.
const LEVELS: u32 = 3;

/// Characters of the first layout, each with the keysym that types it.
pub(crate) fn layout_chars() -> Result<HashMap<char, u32>> {
    let text = keymap_text()?;
    let xkb = xkbcommon_dl::xkbcommon_option().context("libxkbcommon not found")?;
    let text = CString::new(text.trim_end_matches('\0')).context("keymap with a NUL")?;
    let mut chars = HashMap::new();
    // SAFETY: calls into libxkbcommon with the pointers it returned, freed at the end.
    unsafe {
        let ctx = (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
        if ctx.is_null() {
            bail!("xkb context not created");
        }
        let keymap = (xkb.xkb_keymap_new_from_string)(
            ctx,
            text.as_ptr(),
            xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
            xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
        );
        if keymap.is_null() {
            (xkb.xkb_context_unref)(ctx);
            bail!("keymap of the compositor not compiled");
        }
        for keycode in (xkb.xkb_keymap_min_keycode)(keymap)..=(xkb.xkb_keymap_max_keycode)(keymap) {
            let levels = (xkb.xkb_keymap_num_levels_for_key)(keymap, keycode, 0).min(LEVELS);
            for level in 0..levels {
                let mut syms = std::ptr::null();
                let count = (xkb.xkb_keymap_key_get_syms_by_level)(keymap, keycode, 0, level, &mut syms);
                for i in 0..usize::try_from(count).unwrap_or(0) {
                    let keysym = *syms.add(i);
                    let code = (xkb.xkb_keysym_to_utf32)(keysym);
                    if let Some(c) = char::from_u32(code).filter(|c| !c.is_control() && code != 0) {
                        chars.entry(c).or_insert(keysym);
                    }
                }
            }
        }
        (xkb.xkb_keymap_unref)(keymap);
        (xkb.xkb_context_unref)(ctx);
    }
    Ok(chars)
}

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    keymap: Option<(OwnedFd, u32)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "wl_seat"
            && state.seat.is_none()
        {
            state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, ()));
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } =
            event
        {
            state.keymap = Some((fd, size));
        }
    }
}

/// The keymap in XKB's text format, as the compositor sends it to a new keyboard.
fn keymap_text() -> Result<String> {
    let conn = Connection::connect_to_env().context("Wayland connection")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    let seat = state.seat.clone().context("no Wayland seat")?;
    let keyboard = seat.get_keyboard(&qh, ());
    for _ in 0..3 {
        if state.keymap.is_some() {
            break;
        }
        queue.roundtrip(&mut state)?;
    }
    if keyboard.version() >= 3 {
        keyboard.release();
    }
    let (fd, size) = state.keymap.take().context("no keymap from the compositor")?;
    let mut buf = vec![0u8; size as usize];
    std::fs::File::from(fd).read_exact_at(&mut buf, 0).context("keymap not read")?;
    String::from_utf8(buf).context("keymap not UTF-8")
}
