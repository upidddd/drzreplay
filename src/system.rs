//! Probing the machine: session type, monitors, encoders, audio devices; running helper programs.
use crate::config::{self as C, Config};
use serde_json::{json, Value};
use std::env;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    /// the last non-empty line of stderr, for an error message
    pub fn last_err(&self, fallback: &str) -> String {
        self.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(fallback).trim().to_string()
    }
}

/// Run a program and collect its output; None when it can't start or runs past `timeout`.
pub fn run(cmd: &[&str], timeout: Duration) -> Option<Out> {
    let mut c = Command::new(cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    run_cmd(c, timeout)
}

pub fn run_cmd(mut c: Command, timeout: Duration) -> Option<Out> {
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let mut so = child.stdout.take()?;
    let mut se = child.stderr.take()?;
    let t1 = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = so.read_to_end(&mut b);
        b
    });
    let t2 = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = se.read_to_end(&mut b);
        b
    });
    let end = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    let (o, e) = (t1.join().unwrap_or_default(), t2.join().unwrap_or_default());
    let s = status?;
    Some(Out {
        code: s.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o).into_owned(),
        stderr: String::from_utf8_lossy(&e).into_owned(),
    })
}

pub fn which(name: &str) -> Option<std::path::PathBuf> {
    env::var_os("PATH").and_then(|p| env::split_paths(&p).map(|d| d.join(name)).find(|f| f.is_file()))
}

pub fn is_wayland() -> bool {
    env::var("XDG_SESSION_TYPE").map(|v| v.eq_ignore_ascii_case("wayland")).unwrap_or(false)
        || (env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
            && env::var_os("DISPLAY").is_none_or(|v| v.is_empty()))
}

// ---------------------------------------------------------------- screens
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub primary: bool,
    pub w: i64,
    pub h: i64,
    pub x: i64,
    pub y: i64,
}

pub fn screen_size() -> (i64, i64) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    if let Ok((conn, n)) = x11rb::connect(None) {
        let root = conn.setup().roots[n].root;
        if let Ok(Ok(g)) = conn.get_geometry(root).map(|c| c.reply()) {
            return (g.width as i64, g.height as i64);
        }
    }
    (1920, 1080)
}

/// every active output
pub fn monitors() -> Vec<Monitor> {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::ConnectionExt as _;
    let mut out = vec![];
    let Ok((conn, n)) = x11rb::connect(None) else { return out };
    let root = conn.setup().roots[n].root;
    let Ok(Ok(reply)) = conn.randr_get_monitors(root, true).map(|c| c.reply()) else { return out };
    for m in reply.monitors {
        let name = conn
            .get_atom_name(m.name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_default();
        out.push(Monitor {
            name,
            primary: m.primary,
            w: m.width as i64,
            h: m.height as i64,
            x: m.x as i64,
            y: m.y as i64,
        });
    }
    out
}

/// (w, h, x, y) to grab; even sizes so every encoder is happy
pub fn capture_geometry(cfg: &Config) -> (i64, i64, i64, i64) {
    let (mut w, mut h) = screen_size();
    let (mut x, mut y) = (0, 0);
    let mon = cfg.s("monitor");
    if !mon.is_empty() && mon != "all" {
        if let Some(m) = monitors().into_iter().find(|m| m.name == mon) {
            (w, h, x, y) = (m.w, m.h, m.x, m.y);
        }
    }
    (w - w % 2, h - h % 2, x, y)
}

// ---------------------------------------------------------------- encoders
fn try_ffmpeg(args: &[&str]) -> bool {
    let mut cmd = vec!["ffmpeg", "-hide_banner", "-v", "error"];
    cmd.extend_from_slice(args);
    cmd.extend_from_slice(&["-f", "null", "-"]);
    run(&cmd, Duration::from_secs(15)).is_some_and(|o| o.ok())
}

#[derive(Clone, Debug)]
pub struct Encoders {
    pub available: Vec<String>,
    pub vaapi_device: Option<String>,
}

impl Encoders {
    fn from_json(v: &Value) -> Option<Encoders> {
        let available: Vec<String> =
            v.get("available")?.as_array()?.iter().filter_map(|e| e.as_str().map(String::from)).collect();
        if available.is_empty() {
            return None;
        }
        Some(Encoders { available, vaapi_device: v.get("vaapi_device").and_then(|d| d.as_str()).map(String::from) })
    }
    fn to_json(&self) -> Value {
        json!({"available": self.available, "vaapi_device": self.vaapi_device, "time": C::now()})
    }
}

/// {"available": [...], "vaapi_device": path}: cached per boot in /dev/shm
pub fn detect_encoders(force: bool) -> Encoders {
    if !force {
        if let Some(v) = C::read_json(&C::encoders_cache()) {
            if C::now() - v.get("time").and_then(|t| t.as_f64()).unwrap_or(0.0) < 24.0 * 3600.0 {
                if let Some(e) = Encoders::from_json(&v) {
                    return e;
                }
            }
        }
    }
    let src = ["-f", "lavfi", "-i", "color=black:s=640x360:r=30", "-frames:v", "3"];
    let mut avail = vec![];
    // the probes are independent, so they run side by side
    let nv = std::thread::spawn(move || try_ffmpeg(&[&src[..], &["-c:v", "h264_nvenc"]].concat()));
    let mut devs: Vec<_> = std::fs::read_dir("/dev/dri")
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    devs.retain(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD")));
    devs.sort();
    let mut vdev = None;
    for dev in devs {
        let d = dev.to_string_lossy().into_owned();
        let a = [&["-vaapi_device", d.as_str()][..], &src[..], &["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi"]]
            .concat();
        if try_ffmpeg(&a) {
            vdev = Some(d);
            break;
        }
    }
    if nv.join().unwrap_or(false) {
        avail.push("nvenc".to_string());
    }
    if vdev.is_some() {
        avail.push("vaapi".to_string());
    }
    avail.push("x264".to_string());
    let res = Encoders { available: avail, vaapi_device: vdev };
    let _ = C::write_json(&C::encoders_cache(), &res.to_json());
    res
}

/// Encoders to try, preferred one first.
pub fn encoder_order(cfg: &Config, info: &Encoders) -> Vec<String> {
    let mut order: Vec<String> =
        ["nvenc", "vaapi", "x264"].iter().filter(|e| info.available.iter().any(|a| a == *e)).map(|e| e.to_string()).collect();
    let pref = cfg.s("encoder");
    if let Some(i) = order.iter().position(|e| *e == pref) {
        let e = order.remove(i);
        order.insert(0, e);
    }
    order
}

pub fn encoder_label(name: &str) -> String {
    match name {
        "auto" => "Automatic",
        "nvenc" => "NVIDIA NVENC",
        "vaapi" => "AMD / Intel (VA-API)",
        "x264" => "CPU (x264)",
        other => other,
    }
    .to_string()
}

// ---------------------------------------------------------------- sound devices
fn pactl_json(kind: &str) -> Vec<Value> {
    if let Some(o) = run(&["pactl", "--format=json", "list", kind], Duration::from_secs(10)) {
        if o.ok() {
            if let Ok(Value::Array(a)) = serde_json::from_str(&o.stdout) {
                return a;
            }
        }
    }
    let mut items = vec![];
    if let Some(o) = run(&["pactl", "list", "short", kind], Duration::from_secs(10)) {
        if o.ok() {
            for line in o.stdout.lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() > 1 {
                    items.push(json!({"name": parts[1], "description": parts[1]}));
                }
            }
        }
    }
    items
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

/// (pulse source name of the monitor, label): what you hear
pub fn audio_outputs() -> Vec<(String, String)> {
    pactl_json("sinks")
        .iter()
        .map(|s| {
            let name = text(s, "name");
            let desc = text(s, "description");
            (format!("{name}.monitor"), if desc.is_empty() { name } else { desc }.to_string())
        })
        .collect()
}

/// (source name, label): microphones, monitors excluded
pub fn audio_inputs() -> Vec<(String, String)> {
    pactl_json("sources")
        .iter()
        .filter(|s| {
            !text(s, "name").ends_with(".monitor")
                && match s.get("monitor_of_sink") {
                    None | Some(Value::Null) => true,
                    Some(Value::String(m)) => m.is_empty() || m == "n/a",
                    _ => false,
                }
        })
        .map(|s| {
            let name = text(s, "name");
            let desc = text(s, "description");
            (name.to_string(), if desc.is_empty() { name } else { desc }.to_string())
        })
        .collect()
}

/// (default sink, default source) of the PulseAudio / PipeWire server
pub fn pulse_defaults() -> (Option<String>, Option<String>) {
    let (mut sink, mut source) = (None, None);
    if let Some(o) = run(&["pactl", "info"], Duration::from_secs(10)) {
        if o.ok() {
            for line in o.stdout.lines() {
                if let Some(v) = line.strip_prefix("Default Sink:") {
                    sink = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("Default Source:") {
                    source = Some(v.trim().to_string());
                }
            }
        }
    }
    (sink, source)
}

/// (source-output index, source name) of the recording streams opened by these processes
pub fn pulse_streams(pids: &[u32]) -> Vec<(i64, String)> {
    let names: std::collections::HashMap<i64, String> = pactl_json("sources")
        .iter()
        .filter_map(|s| Some((s.get("index")?.as_i64()?, text(s, "name").to_string())))
        .collect();
    let mut out = vec![];
    for so in pactl_json("source-outputs") {
        let pid = so.pointer("/properties/application.process.id").and_then(|p| p.as_str()).and_then(|p| p.parse::<u32>().ok());
        if let (Some(pid), Some(idx)) = (pid, so.get("index").and_then(|i| i.as_i64())) {
            if pids.contains(&pid) {
                let src = so.get("source").and_then(|s| s.as_i64());
                out.push((idx, src.and_then(|s| names.get(&s).cloned()).unwrap_or_default()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- updates
/// True when running from the .deb (and so can be updated by the installer)
pub fn installed() -> bool {
    env::current_exe().ok().and_then(|p| p.canonicalize().ok()).is_some_and(|p| p.starts_with("/usr/bin"))
}

/// Newest release tag on GitHub, e.g. "1.2.0", or None when offline
pub fn latest_version() -> Option<String> {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(6)).user_agent(C::APP_NAME).build();
    let url = format!("https://github.com/{}/releases/latest", C::REPO);
    let resp = match agent.head(&url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(_) => return None,
    };
    let loc = resp.header("location")?.to_string();
    let tag = loc.trim_end_matches('/').rsplit('/').next()?.to_string();
    let t = tag.trim_start_matches('v');
    (!t.is_empty() && t.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))).then(|| t.to_string())
}

pub fn version_tuple(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
}

pub fn is_newer(latest: &str) -> bool {
    version_tuple(latest) > version_tuple(C::VERSION)
}

pub const UPDATER: &str = "/usr/lib/drzreplay/drzreplay-update";

/// Install the newest release without a password, through the root helper the package ships
/// (allowed by /etc/sudoers.d/drzreplay). (Some(ok), message); None when the helper can't be used
/// here (older install, no sudo) and the terminal installer is needed instead.
pub fn self_update() -> (Option<bool>, String) {
    if !(std::path::Path::new(UPDATER).exists() && which("sudo").is_some()) {
        return (None, "no updater".into());
    }
    let Some(o) = run(&["sudo", "-n", UPDATER], Duration::from_secs(900)) else {
        return (Some(false), "the updater didn't finish".into());
    };
    let text = format!("{}{}", o.stdout, o.stderr);
    let lines: Vec<&str> = text.trim().lines().collect();
    if !o.ok() && lines.iter().any(|l| l.to_lowercase().contains("password")) {
        return (None, "sudo wants a password".into());
    }
    (Some(o.ok()), lines.last().unwrap_or(&"").to_string())
}

/// Open a terminal window running a shell command (for the updater's sudo prompt).
pub fn run_in_terminal(command: &str) -> bool {
    let sh = format!("{command}; echo; printf 'Press Enter to close… '; read x");
    for (term, flag) in [
        ("x-terminal-emulator", Some("-e")),
        ("i3-sensible-terminal", Some("-e")),
        ("xfce4-terminal", Some("-x")),
        ("gnome-terminal", Some("--")),
        ("konsole", Some("-e")),
        ("kitty", None),
        ("alacritty", Some("-e")),
        ("xterm", Some("-e")),
    ] {
        if which(term).is_some() {
            let mut c = Command::new(term);
            if let Some(f) = flag {
                c.arg(f);
            }
            c.args(["sh", "-c", &sh]);
            detach(&mut c);
            return c.spawn().is_ok();
        }
    }
    false
}

/// Start a program in its own session, so it outlives us and gets none of our signals.
pub fn detach(c: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c
}

pub fn spawn_quiet(cmd: &[&str]) {
    let mut c = Command::new(cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    detach(&mut c);
    // a reaper thread: nobody waits for these, and zombies would pile up in a long-running daemon
    if let Ok(mut child) = c.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

// ---------------------------------------------------------------- notifications and the desktop
/// A desktop notification through notify-send. Returns what it printed (the id, then the action chosen).
pub fn notify(
    cfg: &Config,
    body: &str,
    icon: Option<&str>,
    urgency: &str,
    replace: Option<&str>,
    actions: &[(&str, &str)],
    wait: bool,
) -> Option<Vec<String>> {
    if !cfg.b("notifications") && urgency != "critical" {
        return None;
    }
    let icon_path = C::icon_path().to_string_lossy().into_owned();
    let mut cmd = vec!["notify-send", "-a", C::APP_NAME, "-i", icon.unwrap_or(&icon_path), "-u", urgency, "-p"];
    let replace_s;
    if let Some(r) = replace {
        replace_s = r.to_string();
        cmd.extend(["-r", &replace_s]);
    }
    let acts: Vec<String> = actions.iter().map(|(k, l)| format!("{k}={l}")).collect();
    for a in &acts {
        cmd.extend(["-A", a]);
    }
    if wait {
        cmd.extend(["-w", "-t", "10000"]);
    }
    cmd.extend([C::APP_NAME, body]);
    let o = run(&cmd, Duration::from_secs(if wait { 30 } else { 5 }))?;
    Some(o.stdout.split_whitespace().map(String::from).collect())
}

/// Just show a message (fire and forget).
pub fn toast(cfg: &Config, text: &str) {
    if cfg.b("notifications") {
        let icon = C::icon_path().to_string_lossy().into_owned();
        spawn_quiet(&["notify-send", "-a", C::APP_NAME, "-i", &icon, C::APP_NAME, text]);
    }
}

pub fn show_in_folder(path: &std::path::Path) {
    let uri = format!("['file://{}']", path.display());
    let ok = run(
        &[
            "gdbus", "call", "--session", "--dest", "org.freedesktop.FileManager1", "--object-path",
            "/org/freedesktop/FileManager1", "--method", "org.freedesktop.FileManager1.ShowItems", &uri, "",
        ],
        Duration::from_secs(10),
    )
    .is_some_and(|o| o.ok());
    if !ok {
        if let Some(d) = path.parent() {
            spawn_quiet(&["xdg-open", &d.to_string_lossy()]);
        }
    }
}

pub fn click(cfg: &Config) {
    let snd = C::click_sound();
    if cfg.b("click_sound") && which("paplay").is_some() && snd.exists() {
        spawn_quiet(&["paplay", "--volume=30000", &snd.to_string_lossy()]);
    }
}
