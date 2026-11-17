//! Global hotkeys for any X11 desktop.
//!
//! Presses are read from XI2 raw key events, so the hotkeys keep working while a fullscreen game
//! grabs the keyboard. The key is also grabbed, which only keeps it from reaching other programs.
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, GrabMode, ModMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

pub const SHIFT: u16 = 1;
pub const LOCK: u16 = 2;
pub const CONTROL: u16 = 4;
pub const MOD1: u16 = 8;
pub const MOD2: u16 = 16;
pub const MOD4: u16 = 64;
/// Lock, NumLock, AltGr & co. don't matter
const CHECKED: u16 = SHIFT | CONTROL | MOD1 | MOD4;

/// '<Primary><Alt>r' -> (modmask, "r")
pub fn parse(accel: &str) -> (u16, String) {
    let mut mods = 0;
    let mut rest = accel.trim();
    while let Some(r) = rest.strip_prefix('<') {
        let Some(end) = r.find('>') else { break };
        mods |= match r[..end].to_lowercase().as_str() {
            "shift" => SHIFT,
            "control" | "ctrl" | "primary" => CONTROL,
            "alt" | "mod1" => MOD1,
            "super" | "meta" | "mod4" => MOD4,
            _ => 0,
        };
        rest = &r[end + 1..];
    }
    (mods, rest.trim().to_string())
}

/// '<Primary><Alt>r' -> "Ctrl+Alt+R"
pub fn pretty(accel: &str) -> String {
    let (mods, key) = parse(accel);
    if key.is_empty() {
        return "—".into();
    }
    let mut parts: Vec<String> = vec![];
    for (bit, name) in [(CONTROL, "Ctrl"), (MOD1, "Alt"), (SHIFT, "Shift"), (MOD4, "Super")] {
        if mods & bit != 0 {
            parts.push(name.into());
        }
    }
    let key = if key.chars().count() == 1 { key.to_uppercase() } else { key.replace('_', " ") };
    parts.push(key);
    parts.join("+")
}

/// X keysym for a key name as GTK writes it in an accelerator ("z", "F9", "Pause"…)
pub fn keysym_from_name(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        // Latin-1 characters are their own keysyms; the rest are 0x1000000 + the code point
        let u = c as u32;
        return Some(if (0x20..0x7f).contains(&u) || (0xa0..0x100).contains(&u) { u } else { 0x100_0000 + u });
    }
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=35).contains(&n) {
            return Some(0xffbd + n);
        }
    }
    if let Some(n) = name.strip_prefix("KP_").and_then(|n| n.parse::<u32>().ok()) {
        if n <= 9 {
            return Some(0xffb0 + n);
        }
    }
    Some(match name {
        "space" => 0x20,
        "BackSpace" => 0xff08,
        "Tab" => 0xff09,
        "Return" => 0xff0d,
        "Pause" => 0xff13,
        "Scroll_Lock" => 0xff14,
        "Escape" => 0xff1b,
        "Home" => 0xff50,
        "Left" => 0xff51,
        "Up" => 0xff52,
        "Right" => 0xff53,
        "Down" => 0xff54,
        "Page_Up" | "Prior" => 0xff55,
        "Page_Down" | "Next" => 0xff56,
        "End" => 0xff57,
        "Print" => 0xff61,
        "Insert" => 0xff63,
        "Menu" => 0xff67,
        "Num_Lock" => 0xff7f,
        "Delete" => 0xffff,
        "KP_Add" => 0xffab,
        "KP_Subtract" => 0xffad,
        "KP_Multiply" => 0xffaa,
        "KP_Divide" => 0xffaf,
        "KP_Enter" => 0xff8d,
        "KP_Decimal" => 0xffae,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "comma" => 0x2c,
        "period" => 0x2e,
        "slash" => 0x2f,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "grave" => 0x60,
        "bracketleft" => 0x5b,
        "bracketright" => 0x5d,
        "backslash" => 0x5c,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub enum Hk {
    /// the key is (not) working: why
    State { name: String, ok: bool, err: String },
    Press { name: String },
}

struct Key {
    name: String,
    code: u8,
    mods: u16,
}

fn keycode_for(conn: &RustConnection, sym: u32) -> Option<u8> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
    let per = map.keysyms_per_keycode as usize;
    // look at the first columns first (the Latin layout), then the rest
    for cols in [2.min(per), per] {
        for (i, chunk) in map.keysyms.chunks(per).enumerate() {
            if chunk.iter().take(cols).any(|s| *s == sym) {
                return Some(min + i as u8);
            }
        }
    }
    None
}

fn modifier_keycodes(conn: &RustConnection) -> Vec<(u16, Vec<u8>)> {
    let Some(m) = conn.get_modifier_mapping().ok().and_then(|c| c.reply().ok()) else { return vec![] };
    let n = m.keycodes_per_modifier() as usize;
    (0..8).map(|i| (1u16 << i, m.keycodes[i * n..(i + 1) * n].iter().copied().filter(|k| *k != 0).collect())).collect()
}

/// Run until `stop` is set. `keys` is [(name, accelerator)]; `emit` gets states and presses.
pub fn run(keys: Vec<(String, String)>, stop: Arc<AtomicBool>, emit: impl Fn(Hk) + Send + 'static) {
    let state = |name: &str, ok: bool, err: &str| emit(Hk::State { name: name.into(), ok, err: err.into() });
    let (conn, screen) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(_) => {
            for (name, _) in &keys {
                state(name, false, "cannot open X display");
            }
            return;
        }
    };
    let root = conn.setup().roots[screen].root;
    let mut grabs: Vec<Key> = vec![];
    for (name, accel) in &keys {
        let (mods, key) = parse(accel);
        let code = keysym_from_name(&key)
            .or_else(|| keysym_from_name(&key.to_lowercase()))
            .and_then(|s| keycode_for(&conn, s));
        let Some(code) = code else {
            state(name, false, &format!("unknown key '{key}'"));
            continue;
        };
        // The grab only keeps the shortcut from reaching other apps; presses are read from XI2
        // raw events on a second connection: the X server holds back raw events from the client
        // whose grab is active, so on one connection the key releases would go missing.
        let mut refused = false;
        for extra in [0, LOCK, MOD2, LOCK | MOD2] {
            let ck = conn.grab_key(true, root, ModMask::from(mods | extra), code, GrabMode::ASYNC, GrabMode::ASYNC);
            if ck.map(|c| c.check().is_err()).unwrap_or(true) {
                refused = true;
            }
        }
        grabs.push(Key { name: name.clone(), code, mods });
        if refused {
            // not fatal while XI2 works: the key then reaches other programs too
            eprintln!("hotkey {accel}: the key is grabbed by another program");
        }
    }
    let _ = conn.flush();
    if grabs.is_empty() {
        return;
    }

    let listen = RustConnection::connect(None).ok().filter(|(c, _)| {
        // XI2 2.2 for raw events
        c.xinput_xi_query_version(2, 2).ok().and_then(|r| r.reply().ok()).is_some()
    });
    let xi = listen.as_ref().is_some_and(|(c, n)| {
        let root = c.setup().roots[*n].root;
        let mask = xinput::EventMask { deviceid: 1, mask: vec![xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_KEY_RELEASE] };
        c.xinput_xi_select_events(root, &[mask]).ok().and_then(|c| c.check().ok()).is_some()
    });
    let _ = conn.sync_ok();
    for k in &grabs {
        state(&k.name, true, "");
    }

    let modkeys = modifier_keycodes(&conn);
    let mut down: std::collections::HashSet<u8> = Default::default();
    let mut last: std::collections::HashMap<String, Instant> = Default::default();
    let mut fire = |name: &str| {
        let now = Instant::now();
        if last.get(name).is_none_or(|t| now.duration_since(*t) > Duration::from_millis(500)) {
            last.insert(name.to_string(), now); // one press, one save
            emit(Hk::Press { name: name.to_string() });
        }
    };
    let fds: Vec<i32> = if xi {
        vec![listen.as_ref().unwrap().0.stream().as_raw_fd(), conn.stream().as_raw_fd()]
    } else {
        vec![conn.stream().as_raw_fd()]
    };
    while !stop.load(Ordering::Relaxed) {
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|fd| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 }).collect();
        unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, 1000) };
        // the grab connection: drain it (and use it for presses when there is no XI2)
        while let Ok(Some(ev)) = conn.poll_for_event() {
            if !xi {
                if let Event::KeyPress(kp) = ev {
                    if let Some(k) = grabs.iter().find(|k| k.code == kp.detail && (kp.state.bits() & CHECKED) == (k.mods & CHECKED)) {
                        fire(&k.name);
                    }
                }
            }
        }
        let Some((lc, _)) = listen.as_ref().filter(|_| xi) else { continue };
        while let Ok(Some(ev)) = lc.poll_for_event() {
            match ev {
                Event::XinputRawKeyRelease(r) => {
                    down.remove(&(r.detail as u8));
                }
                Event::XinputRawKeyPress(r) => {
                    let code = r.detail as u8;
                    let repeat = !down.insert(code);
                    if repeat {
                        continue;
                    }
                    let Some(k) = grabs.iter().find(|k| k.code == code) else { continue };
                    let mut pressed = down.clone();
                    if let Some(map) = lc.query_keymap().ok().and_then(|c| c.reply().ok()) {
                        for kc in 0..=255usize {
                            if map.keys[kc >> 3] & (1 << (kc & 7)) != 0 {
                                pressed.insert(kc as u8);
                            }
                        }
                    }
                    let held: u16 = modkeys.iter().filter(|(_, codes)| codes.iter().any(|c| pressed.contains(c))).map(|(b, _)| *b).sum();
                    if held & CHECKED == k.mods & CHECKED {
                        fire(&k.name.clone());
                    }
                }
                _ => {}
            }
        }
    }
    for k in &grabs {
        for extra in [0, LOCK, MOD2, LOCK | MOD2] {
            let _ = conn.ungrab_key(k.code, root, ModMask::from(k.mods | extra));
        }
    }
    let _ = conn.flush();
}

trait SyncOk {
    fn sync_ok(&self) -> Result<(), ()>;
}
impl SyncOk for RustConnection {
    fn sync_ok(&self) -> Result<(), ()> {
        self.get_input_focus().map_err(|_| ())?.reply().map(|_| ()).map_err(|_| ())
    }
}

#[allow(dead_code)]
fn _unused(_: xproto::Window) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accel() {
        assert_eq!(parse("<Primary><Alt>r"), (CONTROL | MOD1, "r".to_string()));
        assert_eq!(pretty("<Primary><Alt>r"), "Ctrl+Alt+R");
        assert_eq!(pretty("<Alt>z"), "Alt+Z");
        assert_eq!(pretty("Pause"), "Pause");
        assert_eq!(pretty(""), "—");
        assert_eq!(keysym_from_name("z"), Some(0x7a));
        assert_eq!(keysym_from_name("F9"), Some(0xffc6));
    }
}
