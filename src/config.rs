//! Settings, paths and constants shared by every DrzReplay process.
use serde_json::{json, Map, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "DrzReplay";
pub const APP_ID: &str = "com.drz.DrzReplay";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO: &str = "upidddd/drzreplay";
pub const SEGMENT: u32 = 5; // seconds per ring-buffer chunk

pub fn install_cmd() -> String {
    format!("curl -fsSL https://github.com/{REPO}/releases/latest/download/install.sh | sh")
}

pub fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn env_dir(var: &str, fallback: &str) -> PathBuf {
    match env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn conf_home() -> PathBuf {
    env_dir("XDG_CONFIG_HOME", ".config")
}
pub fn conf_dir() -> PathBuf {
    conf_home().join("drzreplay")
}
pub fn conf_path() -> PathBuf {
    conf_dir().join("config.json")
}
pub fn cache_dir() -> PathBuf {
    env_dir("XDG_CACHE_HOME", ".cache").join("drzreplay")
}
pub fn update_cache() -> PathBuf {
    cache_dir().join("update.json")
}
pub fn runtime() -> PathBuf {
    match env::var_os("DRZREPLAY_RUNTIME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(format!("/dev/shm/drzreplay-{}", uid())),
    }
}
pub fn pidfile() -> PathBuf {
    runtime().join("daemon.pid")
}
pub fn statusfile() -> PathBuf {
    runtime().join("status.json")
}
pub fn encoders_cache() -> PathBuf {
    runtime().join("encoders.json")
}
pub fn ffmpeg_log() -> PathBuf {
    runtime().join("ffmpeg.log")
}
pub fn hotkey_log() -> PathBuf {
    runtime().join("hotkey.log")
}
pub fn last_save() -> PathBuf {
    runtime().join("last_save.json")
}
pub fn request_file() -> PathBuf {
    runtime().join("request")
}

// ---------------------------------------------------------------- assets
/// Where the icons and the click sound live: next to the program in a package
/// (/usr/share/drzreplay/assets), or in the source tree while developing.
pub fn assets_dir() -> PathBuf {
    if let Some(d) = env::var_os("DRZREPLAY_ASSETS") {
        return PathBuf::from(d);
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(bin) = exe.parent() {
            for rel in ["../share/drzreplay/assets", "../../assets", "../../../assets"] {
                let p = bin.join(rel);
                if p.join("icons").is_dir() {
                    return p.canonicalize().unwrap_or(p);
                }
            }
        }
    }
    PathBuf::from("/usr/share/drzreplay/assets")
}
pub fn icon_path() -> PathBuf {
    assets_dir().join("icons/drzreplay.svg")
}
pub fn click_sound() -> PathBuf {
    assets_dir().join("sounds/click.wav")
}

// ---------------------------------------------------------------- quality tables
/// CQ / CRF / QP per preset: lower is better quality, bigger files
pub fn rec_quality(name: &str) -> i64 {
    match name {
        "low" => 28,
        "balanced" => 23,
        "high" => 19,
        "ultra" => 16,
        _ => 23,
    }
}
pub fn export_quality(name: &str) -> i64 {
    match name {
        "low" => 26,
        "balanced" => 22,
        "high" => 19,
        "ultra" => 16,
        _ => 19,
    }
}

// ---------------------------------------------------------------- config
pub fn default_video_dir() -> String {
    if let Ok(o) = std::process::Command::new("xdg-user-dir").arg("VIDEOS").output() {
        let d = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !d.is_empty() && Path::new(&d) != home() {
            return format!("{d}/{APP_NAME}");
        }
    }
    format!("{}/Videos/{APP_NAME}", home().display())
}

/// The settings, kept as the JSON map they are stored in: the window sets keys by name.
#[derive(Clone, Debug)]
pub struct Config(pub Map<String, Value>);

fn defaults() -> Vec<(&'static str, Value)> {
    vec![
        // capture
        ("seconds", json!(60)),
        ("fps", json!(30)),
        ("monitor", json!("all")),
        ("draw_mouse", json!(true)),
        ("encoder", json!("auto")), // auto | nvenc | vaapi | x264
        ("rec_quality", json!("balanced")),
        // audio
        ("system_audio", json!(true)),
        ("system_device", json!("@DEFAULT_MONITOR@")),
        ("system_volume", json!(100)),
        ("mic", json!(false)),
        ("mic_device", json!("@DEFAULT_SOURCE@")),
        ("mic_volume", json!(100)),
        // saving
        ("output_dir", json!("")),
        ("file_prefix", json!("DrzReplay")),
        ("container", json!("mp4")),
        ("export_height", json!(0)), // 0 = native
        ("export_quality", json!("high")),
        ("keep_max_clips", json!(0)), // 0 = unlimited
        ("keep_max_gb", json!(0)),
        // controls
        ("hotkey", json!("<Alt>z")),
        ("click_sound", json!(true)),
        ("notifications", json!(true)),
        ("tray", json!(true)),
        // intro
        ("intro", json!(false)),
        ("replay_since_last", json!(true)),
        ("record_hotkey", json!("<Alt>r")),
    ]
}

/// changing any of these needs the recorder (ffmpeg) to restart
pub const RECORDER_KEYS: &[&str] = &[
    "seconds", "fps", "monitor", "draw_mouse", "encoder", "rec_quality", "system_audio", "system_device",
    "system_volume", "mic", "mic_device", "mic_volume",
];

impl Config {
    pub fn defaults() -> Config {
        Config(defaults().into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn exists() -> bool {
        conf_path().exists()
    }

    pub fn load() -> Config {
        let mut cfg = Config::defaults();
        match fs::read_to_string(conf_path()) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(data)) => {
                    for (k, v) in data {
                        if cfg.0.contains_key(&k) && same_kind(&cfg.0[&k], &v) {
                            cfg.0.insert(k, v);
                        }
                    }
                }
                _ => eprintln!("{APP_NAME}: config is broken, using defaults"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("{APP_NAME}: config is broken ({e}), using defaults"),
        }
        if cfg.s("output_dir").is_empty() {
            cfg.0.insert("output_dir".into(), json!(default_video_dir()));
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        fs::create_dir_all(conf_dir())?;
        let mut out = Map::new();
        for (k, _) in defaults() {
            out.insert(k.to_string(), self.0[k].clone());
        }
        let tmp = conf_dir().join("config.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(out)).unwrap())?;
        fs::rename(tmp, conf_path())
    }

    pub fn get(&self, key: &str) -> &Value {
        self.0.get(key).unwrap_or(&Value::Null)
    }
    pub fn set(&mut self, key: &str, v: Value) {
        self.0.insert(key.to_string(), v);
    }
    pub fn b(&self, key: &str) -> bool {
        match self.get(key) {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
            _ => false,
        }
    }
    pub fn f(&self, key: &str) -> f64 {
        self.get(key).as_f64().unwrap_or(0.0)
    }
    pub fn i(&self, key: &str) -> i64 {
        let v = self.get(key);
        v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0)
    }
    pub fn s(&self, key: &str) -> String {
        self.get(key).as_str().unwrap_or("").to_string()
    }

    pub fn output_dir(&self) -> PathBuf {
        expand_user(&self.s("output_dir"))
    }
    /// the clip file name prefix
    pub fn prefix(&self) -> String {
        let p = self.s("file_prefix");
        if p.is_empty() {
            APP_NAME.to_string()
        } else {
            p
        }
    }
    pub fn ext(&self) -> &'static str {
        if self.s("container") == "mkv" {
            "mkv"
        } else {
            "mp4"
        }
    }
}

/// The saved value may only replace a default of the same JSON kind (a number for a number…).
fn same_kind(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::Bool(_), Value::Bool(_)) | (Value::Number(_), Value::Number(_)) | (Value::String(_), Value::String(_))
    )
}

pub fn expand_user(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

// ---------------------------------------------------------------- json helpers
pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

pub fn read_obj(path: &Path) -> Map<String, Value> {
    match read_json(path) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

pub fn write_json(path: &Path, data: &Value) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp, serde_json::to_string(data).unwrap())?;
    fs::rename(tmp, path)
}

/// Merge `extra` into the object stored at `path`.
pub fn update_json(path: &Path, extra: Value) {
    let mut m = read_obj(path);
    if let Value::Object(e) = extra {
        for (k, v) in e {
            m.insert(k, v);
        }
    }
    let _ = write_json(path, &Value::Object(m));
}

// ---------------------------------------------------------------- daemon pid
pub fn daemon_pid() -> Option<i32> {
    let pid: i32 = fs::read_to_string(pidfile()).ok()?.trim().parse().ok()?;
    if unsafe { libc::kill(pid, 0) } != 0 {
        return None;
    }
    let cmd = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if cmd.windows(9).any(|w| w == b"drzreplay") {
        Some(pid)
    } else {
        None
    }
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
