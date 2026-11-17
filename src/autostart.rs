//! Start-with-system toggle.
//!
//! The .deb ships /etc/xdg/autostart/drzreplay.desktop; a user turns it off with a
//! Hidden=true override in ~/.config/autostart. Run from source, we write a full entry.
//!
//! i3 only runs XDG autostart entries when its config calls `dex --autostart`. When the
//! user's i3 config doesn't, we add (or remove) one marked `exec` line to it instead.
use crate::config as C;
use crate::daemon::self_exe;
use std::fs;
use std::path::PathBuf;

const SYSTEM_FILE: &str = "/etc/xdg/autostart/drzreplay.desktop";
const I3_MARK: &str = "# DrzReplay autostart";

fn user_file() -> PathBuf {
    C::conf_home().join("autostart/drzreplay.desktop")
}

fn quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// The command that starts this program: its name when installed, the full path otherwise.
fn exe() -> String {
    let me = self_exe();
    match crate::system::which("drzreplay") {
        Some(p) if p.canonicalize().ok() == me.canonicalize().ok() => "drzreplay".into(),
        _ => quote(&me.to_string_lossy()),
    }
}

fn entry(hidden: bool) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={}\nComment=Instant replay recorder\nExec={} daemon\nIcon=drzreplay\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled={}\nHidden={}\n",
        C::APP_NAME,
        exe(),
        if hidden { "false" } else { "true" },
        if hidden { "true" } else { "false" },
    )
}

pub fn is_i3() -> bool {
    std::env::var_os("I3SOCK").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP").map(|d| d.to_lowercase().split(':').any(|p| p == "i3")).unwrap_or(false)
}

/// The user's own i3 config, or None when i3 runs the system one (which calls dex).
fn i3_config() -> Option<PathBuf> {
    [C::conf_home().join("i3/config"), C::home().join(".i3/config")]
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.canonicalize().unwrap_or(p)) // dotfiles are often symlinks: edit the real file
}

fn i3_lines() -> Option<(PathBuf, Vec<String>)> {
    let p = if is_i3() { i3_config()? } else { return None };
    let text = fs::read_to_string(&p).ok()?;
    let lines: Vec<String> = text.split_inclusive('\n').map(String::from).collect();
    let dex = lines.iter().filter(|l| !l.contains(I3_MARK)).any(|l| {
        let t = l.trim_start();
        t.starts_with("exec") && t.find("dex").zip(t.find("--autostart")).is_some_and(|(a, b)| a < b)
    });
    (!dex).then_some((p, lines)) // dex handles XDG autostart for us
}

fn xdg_enabled() -> bool {
    match fs::read_to_string(user_file()) {
        Ok(t) => !t.contains("Hidden=true") && !t.contains("X-GNOME-Autostart-enabled=false"),
        Err(_) => std::path::Path::new(SYSTEM_FILE).exists(),
    }
}

pub fn enabled() -> bool {
    match i3_lines() {
        Some((_, lines)) => lines.iter().any(|l| l.contains(I3_MARK) && !l.trim_start().starts_with('#')),
        None => xdg_enabled(),
    }
}

pub fn set_enabled(on: bool) {
    let uf = user_file();
    if let Some(d) = uf.parent() {
        let _ = fs::create_dir_all(d);
    }
    if on && std::path::Path::new(SYSTEM_FILE).exists() {
        let _ = fs::remove_file(&uf);
    } else {
        let _ = fs::write(&uf, entry(!on));
    }
    if let Some((p, mut lines)) = i3_lines() {
        lines.retain(|l| !l.contains(I3_MARK));
        if on {
            if let Some(last) = lines.last_mut() {
                if !last.ends_with('\n') {
                    last.push('\n');
                }
            }
            lines.push(format!("exec --no-startup-id {} daemon  {I3_MARK}\n", exe()));
        }
        let tmp = PathBuf::from(format!("{}.drzreplay.tmp", p.display()));
        if fs::write(&tmp, lines.concat()).is_ok() {
            let _ = fs::rename(tmp, p);
        }
    }
}
