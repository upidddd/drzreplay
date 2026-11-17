//! Background recorder: ring buffer of short MPEG-TS chunks in RAM + tray + hotkey.
//!
//! Signals: SIGHUP = reload settings (or run the command in RUNTIME/request), SIGUSR1 = save a replay,
//! SIGUSR2 = restart with the newly installed program (sent by the package after an upgrade),
//! SIGTERM = quit.
use crate::config::{self as C, Config};
use crate::hotkey::{self, pretty, Hk};
use crate::save;
use crate::system::{self as sys, Encoders};
use crate::tray::{self, Act, AnyTray, View};
use crate::ts;
use crate::wayland::{self as WL, WlEv};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub enum Ev {
    Signal(i32),
    Hk(Hk),
    Tray(Act),
    Wl(WlEv),
    FollowDefault,
    UpdateFound(String),
    RunUpdate,
    UpdateDone(Option<bool>, String),
}

struct FileRec {
    dir: PathBuf,
    t0: f64,
    copied: HashSet<(PathBuf, u128)>,
    n: usize,
    name: String,
    first_end: Option<f64>,
}

pub struct Daemon {
    cfg: Config,
    use_tray: bool,
    tx: Sender<Ev>,
    rt: tokio::runtime::Runtime,
    proc: Option<Child>,
    gst: Option<Child>,
    hk_thread: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    hk_state: std::collections::HashMap<&'static str, (String, bool, String)>,
    filerec: Option<FileRec>,
    paused: bool,
    rec_only: bool, // replays are off, the screen is captured only for a full recording
    state: String,
    error: String,
    enc: Option<String>,
    geom: Option<(i64, i64, i64, i64)>,
    fails: u32,
    audio_off: bool,
    started: f64,
    tray: Option<AnyTray>,
    reexec: bool,
    update: Option<String>, // newer version on GitHub, if any
    updating: bool,
    wayland: bool,
    cast: Option<WL::ScreenCast>,
    shortcuts: Option<WL::Shortcuts>,
    info: Option<Encoders>, // the encoders: probed in run(), after the tray icon is up
    defaults: (Option<String>, Option<String>),
    sub_proc: Option<Child>,
    last_press: std::collections::HashMap<String, Instant>,
    quit: bool,
}

fn now() -> f64 {
    C::now()
}

fn spawn_logged(cmd: &mut Command) -> std::io::Result<Child> {
    let log = fs::OpenOptions::new().create(true).append(true).open(C::ffmpeg_log())?;
    unsafe {
        cmd.pre_exec(|| {
            libc::nice(5);
            Ok(())
        });
    }
    cmd.stderr(Stdio::from(log)).spawn()
}

fn log_line(text: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(C::ffmpeg_log()) {
        let _ = f.write_all(text.as_bytes());
    }
}

fn hk_log(text: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(C::hotkey_log()) {
        let _ = writeln!(f, "{} {text}", chrono::Local::now().format("%H:%M:%S"));
    }
}

fn ctime() -> String {
    chrono::Local::now().format("%a %b %e %H:%M:%S %Y").to_string()
}

pub fn self_exe() -> PathBuf {
    let p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("drzreplay"));
    // after an upgrade the old file is gone and Linux appends " (deleted)"
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
}

fn wait_exit(child: &mut Child, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn signal(child: &Child, sig: i32) {
    unsafe { libc::kill(child.id() as i32, sig) };
}

fn alive(c: &mut Option<Child>) -> bool {
    c.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
}

// ---------------------------------------------------------------- ffmpeg command lines
pub fn segment_args(cfg: &Config) -> Vec<String> {
    // the ring buffer: 5-second MPEG-TS chunks in RAM, the oldest one overwritten
    let wrap = (cfg.f("seconds") / C::SEGMENT as f64).ceil() as i64 + 3;
    let mut v: Vec<String> = ["-f", "segment", "-segment_time"].map(String::from).to_vec();
    v.push(C::SEGMENT.to_string());
    v.extend(["-segment_wrap".to_string(), wrap.to_string()]);
    v.extend(["-segment_format", "mpegts", "-reset_timestamps", "1"].map(String::from));
    // write every packet right away, otherwise the newest seconds sit in a 256 KB buffer
    v.extend(["-segment_format_options", "flush_packets=1"].map(String::from));
    v.push(C::runtime().join("seg_%03d.ts").to_string_lossy().into_owned());
    v
}

fn audio_sources(cfg: &Config) -> Vec<(String, i64)> {
    let mut audio = vec![];
    if cfg.b("system_audio") {
        let d = cfg.s("system_device");
        audio.push((if d.is_empty() { "@DEFAULT_MONITOR@".into() } else { d }, cfg.i("system_volume")));
    }
    if cfg.b("mic") {
        let d = cfg.s("mic_device");
        audio.push((if d.is_empty() { "@DEFAULT_SOURCE@".into() } else { d }, cfg.i("mic_volume")));
    }
    audio
}

pub fn build_cmd(cfg: &Config, enc: &str, geom: (i64, i64, i64, i64), info: &Encoders) -> Vec<String> {
    let (w, h, x, y) = geom;
    let fps = cfg.i("fps");
    let q = C::rec_quality(&cfg.s("rec_quality"));
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let st = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut cmd = st(&["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error"]);
    if enc == "vaapi" {
        cmd.extend(["-vaapi_device".to_string(), info.vaapi_device.clone().unwrap_or_default()]);
    }
    cmd.extend(st(&["-thread_queue_size", "32", "-f", "x11grab", "-framerate"]));
    cmd.push(fps.to_string());
    cmd.extend(["-video_size".to_string(), format!("{w}x{h}"), "-draw_mouse".to_string(), if cfg.b("draw_mouse") { "1" } else { "0" }.into()]);
    cmd.extend(["-i".to_string(), format!("{display}+{x},{y}")]);

    let audio = audio_sources(cfg);
    for (dev, _) in &audio {
        cmd.extend(st(&["-thread_queue_size", "1024", "-f", "pulse", "-i"]));
        cmd.push(dev.clone());
    }
    let mut fc: Vec<String> = vec![];
    let mut vmap = "0:v".to_string();
    if enc == "vaapi" {
        fc.push("[0:v]format=nv12,hwupload[v]".into());
        vmap = "[v]".into();
    }
    if !audio.is_empty() {
        let mut labels = vec![];
        for (i, (_, vol)) in audio.iter().enumerate() {
            let i = i + 1;
            fc.push(format!("[{i}:a]volume={:.2}[a{i}]", *vol as f64 / 100.0));
            labels.push(format!("[a{i}]"));
        }
        if labels.len() == 2 {
            fc.push(format!("{}{}amix=inputs=2:normalize=0:duration=longest[a]", labels[0], labels[1]));
        } else {
            let last = fc.len() - 1;
            fc[last] = fc[last].replace("[a1]", "[a]");
        }
    }
    if !fc.is_empty() {
        cmd.extend(["-filter_complex".to_string(), fc.join(";")]);
    }
    cmd.extend(["-map".to_string(), vmap]);
    if !audio.is_empty() {
        cmd.extend(st(&["-map", "[a]"]));
    }
    let gop = fps.to_string(); // a keyframe every second: full recordings can start on time
    match enc {
        "nvenc" => {
            // raw bgr0 frames go straight to the GPU, colour conversion happens there too
            cmd.extend(st(&["-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ll", "-rc", "vbr", "-cq"]));
            cmd.push(q.to_string());
            cmd.extend(st(&["-b:v", "0", "-maxrate", "40M", "-bufsize", "40M", "-bf", "0", "-g"]));
            cmd.extend([gop, "-forced-idr".into(), "1".into()]);
        }
        "vaapi" => {
            cmd.extend(st(&["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp"]));
            cmd.extend([q.to_string(), "-g".into(), gop, "-bf".into(), "0".into()]);
        }
        _ => {
            cmd.extend(st(&["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-crf"]));
            cmd.extend([q.to_string()]);
            cmd.extend(st(&["-pix_fmt", "yuv420p", "-g"]));
            cmd.extend([gop]);
            cmd.extend(st(&["-threads", "2"]));
        }
    }
    cmd.extend(st(&["-force_key_frames", "expr:gte(t,n_forced)"]));
    if !audio.is_empty() {
        cmd.extend(st(&["-c:a", "aac", "-b:a", "160k", "-ar", "48000", "-ac", "2"]));
    }
    cmd.extend(segment_args(cfg));
    cmd
}

/// ffmpeg only cuts GStreamer's finished stream into the ring buffer on Wayland
pub fn wayland_ffmpeg_cmd(cfg: &Config) -> Vec<String> {
    // a still screen may send its first frame late: give the probe time to see the video, and insist on it
    // (without -map 0:v ffmpeg would quietly record sound only)
    let mut v: Vec<String> = [
        "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-probesize", "32M", "-analyzeduration", "8000000",
        "-f", "mpegts", "-i", "-", "-map", "0:v",
    ]
    .map(String::from)
    .to_vec();
    if cfg.b("system_audio") || cfg.b("mic") {
        v.extend(["-map".to_string(), "0:a".to_string()]);
    }
    v.extend(["-c".to_string(), "copy".to_string()]);
    v.extend(segment_args(cfg));
    v
}

impl Daemon {
    pub fn new(use_tray: bool, paused: bool, tx: Sender<Ev>) -> Daemon {
        Daemon {
            cfg: Config::load(),
            use_tray,
            tx,
            rt: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio"),
            proc: None,
            gst: None,
            hk_thread: None,
            hk_state: Default::default(),
            filerec: None,
            paused,
            rec_only: false,
            state: "starting".into(),
            error: String::new(),
            enc: None,
            geom: None,
            fails: 0,
            audio_off: false,
            started: 0.0,
            tray: None,
            reexec: false,
            update: None,
            updating: false,
            wayland: sys::is_wayland(),
            cast: None,
            shortcuts: None,
            info: None,
            defaults: (None, None),
            sub_proc: None,
            last_press: Default::default(),
            quit: false,
        }
    }

    fn info(&self) -> Encoders {
        self.info.clone().unwrap_or(Encoders { available: vec!["x264".into()], vaapi_device: None })
    }

    // ------------------------------------------------------------ recorder
    fn clear_buffer(&self) {
        for f in save::seg_files() {
            let _ = fs::remove_file(f);
        }
    }

    fn pick_encoder(&self) -> String {
        let order = sys::encoder_order(&self.cfg, &self.info());
        order[(self.fails as usize / 2).min(order.len() - 1)].clone()
    }

    fn start_recorder(&mut self) {
        self.stop_recorder();
        if self.filerec.is_some() {
            self.filerec_copy(true); // a settings change mid-recording: keep what's there
        }
        self.clear_buffer();
        if self.wayland {
            self.start_wayland();
            return;
        }
        let enc = self.pick_encoder();
        let geom = sys::capture_geometry(&self.cfg);
        let mut cfg = self.cfg.clone();
        if self.audio_off {
            cfg.set("system_audio", json!(false));
            cfg.set("mic", json!(false));
        }
        let cmd = build_cmd(&cfg, &enc, geom, &self.info());
        log_line(&format!("\n--- {} {}\n", ctime(), cmd.join(" ")));
        self.started = now();
        match spawn_logged(Command::new(&cmd[0]).args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::null())) {
            Ok(c) => {
                self.proc = Some(c);
                self.enc = Some(enc);
                self.geom = Some(geom);
                self.state = "recording".into();
                self.error.clear();
            }
            Err(e) => {
                self.state = "error".into();
                self.error = format!("Can't start ffmpeg: {e}");
            }
        }
    }

    // ------------------------------------------------------------ Wayland
    fn wayland_encoders(&self) -> Encoders {
        // GStreamer's encoders, kept apart from ffmpeg's (saving still encodes with ffmpeg)
        let cache = C::runtime().join("gst_encoders.json");
        if let Some(v) = C::read_json(&cache) {
            if now() - v.get("time").and_then(|t| t.as_f64()).unwrap_or(0.0) < 24.0 * 3600.0 {
                let a: Vec<String> = v["available"].as_array().map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect()).unwrap_or_default();
                if !a.is_empty() {
                    return Encoders { available: a, vaapi_device: None };
                }
            }
        }
        let mut available = WL::gst_encoders();
        if available.is_empty() {
            available.push("x264".into());
        }
        let _ = C::write_json(&cache, &json!({"available": available, "vaapi_device": null, "time": now()}));
        Encoders { available, vaapi_device: None }
    }

    fn start_wayland(&mut self) {
        if self.cast.is_none() {
            let need = WL::missing_parts();
            if !need.is_empty() {
                self.state = "error".into();
                self.error = format!("Wayland capture needs: sudo apt install {}", need.join(" "));
                return;
            }
            let tx = self.tx.clone();
            match WL::ScreenCast::start(self.rt.handle(), self.cfg.b("draw_mouse"), move |e| {
                let _ = tx.send(Ev::Wl(e));
            }) {
                Ok(c) => self.cast = Some(c),
                Err(e) => {
                    self.state = "error".into();
                    self.error = format!("Screen sharing: {e}");
                    return;
                }
            }
            self.state = "starting".into();
            self.error = "Choose the screen to record in the dialog".into();
            return;
        }
        let Some((node, size)) = self.cast.as_ref().and_then(|c| c.stream()) else {
            return; // still waiting for the dialog
        };
        let enc = self.pick_encoder();
        let mut cfg = self.cfg.clone();
        if self.audio_off {
            cfg.set("system_audio", json!(false));
            cfg.set("mic", json!(false));
        }
        let fd = match self.cast.as_ref().unwrap().open_remote(self.rt.handle()) {
            Ok(fd) => fd,
            Err(e) => {
                self.state = "error".into();
                self.error = format!("Screen sharing: {e}");
                self.cast = None;
                return;
            }
        };
        let gst = WL::gst_cmd(&cfg, &enc, node, fd.as_raw_fd());
        let ff = wayland_ffmpeg_cmd(&cfg);
        log_line(&format!("\n--- {} {}\n    | {}\n", ctime(), gst.join(" "), ff.join(" ")));
        self.started = now();
        let raw = fd.as_raw_fd();
        let mut gcmd = Command::new(&gst[0]);
        gcmd.args(&gst[1..]).stdin(Stdio::null()).stdout(Stdio::piped());
        unsafe {
            // the PipeWire connection must survive the exec (Rust closes inherited descriptors otherwise)
            gcmd.pre_exec(move || {
                let flags = libc::fcntl(raw, libc::F_GETFD);
                libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                Ok(())
            });
        }
        let mut gchild = match spawn_logged(&mut gcmd) {
            Ok(c) => c,
            Err(e) => {
                self.state = "error".into();
                self.error = format!("Can't start GStreamer: {e}");
                return;
            }
        };
        drop(fd);
        let out = gchild.stdout.take().map(Stdio::from).unwrap_or_else(Stdio::null);
        let fch = spawn_logged(Command::new(&ff[0]).args(&ff[1..]).stdin(out).stdout(Stdio::null()));
        self.gst = Some(gchild);
        match fch {
            Ok(c) => self.proc = Some(c),
            Err(e) => {
                self.state = "error".into();
                self.error = format!("Can't start ffmpeg: {e}");
                return;
            }
        }
        self.enc = Some(enc);
        self.geom = Some((size.0 as i64, size.1 as i64, 0, 0));
        self.state = "recording".into();
        self.error.clear();
    }

    fn on_wl(&mut self, ev: WlEv) {
        match ev {
            WlEv::CastReady => {
                if !self.paused {
                    self.start_recorder();
                }
                self.update_tray();
                self.write_status();
            }
            WlEv::CastError(msg) => {
                self.cast = None;
                self.stop_recorder();
                self.state = "error".into();
                self.error = if msg == "cancelled" { "Screen sharing was not allowed".into() } else { format!("Screen sharing: {msg}") };
                self.update_tray();
                self.write_status();
            }
            WlEv::CastClosed => {
                self.cast = None;
                self.stop_recorder();
                self.clear_buffer();
                self.state = "error".into();
                self.error = "Screen sharing was stopped".into();
                self.notify("Screen sharing was stopped. Turn recording back on in DrzReplay.");
                self.update_tray();
                self.write_status();
            }
            WlEv::ShortcutPress(name) => self.on_portal_press(&name),
            WlEv::ShortcutState { ok, err } => {
                let keys = self.hotkeys();
                for (name, accel) in keys {
                    self.hk_state.insert(name, (accel, ok, err.clone()));
                }
                self.write_status();
            }
        }
    }

    /// An ffmpeg left behind by a recorder that was killed would keep filling the buffer.
    fn kill_stale_recorders(&self) {
        let target = format!("{}", C::runtime().join("seg_").display());
        let Ok(d) = fs::read_dir("/proc") else { return };
        for e in d.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(cmd) = fs::read(format!("/proc/{name}/cmdline")) else { continue };
            let text = String::from_utf8_lossy(&cmd);
            if text.contains("ffmpeg") && text.contains(&target) {
                if fs::metadata(format!("/proc/{name}")).map(|m| std::os::unix::fs::MetadataExt::uid(&m) == C::uid()).unwrap_or(false) {
                    unsafe { libc::kill(name.parse().unwrap_or(0), libc::SIGTERM) };
                }
            }
        }
    }

    fn stop_recorder(&mut self) {
        if alive(&mut self.gst) {
            if let Some(g) = &self.gst {
                signal(g, libc::SIGTERM);
            }
        }
        for slot in [&mut self.proc, &mut self.gst] {
            if let Some(mut p) = slot.take() {
                if matches!(p.try_wait(), Ok(None)) {
                    signal(&p, libc::SIGINT);
                    if !wait_exit(&mut p, 5) {
                        let _ = p.kill();
                        let _ = p.wait();
                    }
                }
            }
        }
    }

    fn health(&mut self) {
        self.refresh_icon();
        if self.paused || (self.state == "error" && self.proc.is_none()) || (self.wayland && self.proc.is_none() && self.state == "starting") {
            self.write_status(); // paused, broken, or waiting for the screen-sharing dialog
            return;
        }
        if alive(&mut self.proc) {
            if now() - self.started > 20.0 {
                self.fails = 0;
            }
            if self.filerec.is_some() {
                self.update_tray(); // the elapsed time in the tray menu
            }
            // monitor layout changed -> restart with the new geometry
            if !self.wayland && (now() as i64) % 5 == 0 && Some(sys::capture_geometry(&self.cfg)) != self.geom {
                self.start_recorder();
            }
            self.write_status();
            return;
        }
        // ffmpeg died: retry, then drop audio, then fall back to the next encoder
        self.fails += 1;
        if self.fails == 3 && (self.cfg.b("system_audio") || self.cfg.b("mic")) {
            self.audio_off = true;
            self.notify("Sound capture failed, recording without sound.");
        }
        if self.fails > 8 {
            self.state = "error".into();
            self.error = format!("Recorder keeps failing: {}", self.log_tail());
            self.proc = None;
            self.write_status();
            return;
        }
        self.start_recorder();
        self.write_status();
    }

    fn log_tail(&self) -> String {
        let Ok(data) = fs::read(C::ffmpeg_log()) else { return "unknown error".into() };
        let tail = &data[data.len().saturating_sub(400)..];
        String::from_utf8_lossy(tail).lines().filter(|l| !l.is_empty() && !l.starts_with("---")).last().unwrap_or("unknown error").to_string()
    }

    // ------------------------------------------------------------ sound devices
    fn recorder_pids(&mut self) -> Vec<u32> {
        let mut v = vec![];
        for p in [&mut self.proc, &mut self.gst] {
            if let Some(c) = p {
                if matches!(c.try_wait(), Ok(None)) {
                    v.push(c.id());
                }
            }
        }
        v
    }

    /// Keep recording what you hear when the default device changes (headphones plugged in…):
    /// our recording streams are moved to the new device, no restart, the buffer is kept.
    fn start_audio_follow(&mut self) {
        if sys::which("pactl").is_none() {
            return;
        }
        self.defaults = sys::pulse_defaults();
        let Ok(mut p) = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return };
        let out = p.stdout.take().unwrap();
        self.sub_proc = Some(p);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if line.contains("on server") {
                    let _ = tx.send(Ev::FollowDefault);
                }
            }
        });
    }

    fn follow_default(&mut self) {
        let new = sys::pulse_defaults();
        if new == self.defaults || new.0.is_none() || new.1.is_none() {
            return;
        }
        self.defaults = new.clone();
        let (sink, source) = (new.0.unwrap(), new.1.unwrap());
        let pids = self.recorder_pids();
        for (idx, src) in sys::pulse_streams(&pids) {
            if src.is_empty() {
                continue;
            }
            let target = if src.ends_with(".monitor") {
                (self.cfg.b("system_audio") && self.cfg.s("system_device").is_empty()).then(|| format!("{sink}.monitor"))
            } else {
                (self.cfg.b("mic") && self.cfg.s("mic_device").is_empty()).then(|| source.clone())
            };
            if let Some(t) = target.filter(|t| *t != src) {
                sys::spawn_quiet(&["pactl", "move-source-output", &idx.to_string(), &t]);
            }
        }
    }

    fn stop_audio_follow(&mut self) {
        if let Some(mut p) = self.sub_proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
    }

    // ------------------------------------------------------------ recording a full video
    fn toggle_file_recording(&mut self) {
        if self.filerec.is_some() {
            self.stop_file_recording(false);
        } else {
            self.start_file_recording();
        }
    }

    /// Every finished ring chunk is copied to disk until stopped, then joined without re-encoding.
    fn start_file_recording(&mut self) {
        if self.filerec.is_some() {
            return;
        }
        if self.paused {
            // replays are off: capture just for this video, and stop again when it ends
            self.rec_only = true;
            self.paused = false;
            self.fails = 0;
            self.start_recorder();
        } else if self.state != "recording" {
            self.notify("The recorder isn't working, see Recorder log in the window menu.");
            return;
        }
        let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string();
        let dir = self.cfg.output_dir().join(format!(".recording {stamp}"));
        if let Err(e) = fs::create_dir_all(&dir) {
            let m = e.to_string();
            self.notify(&format!("Can't record: {}", m.split(" (os error").next().unwrap_or(&m)));
            return;
        }
        self.filerec = Some(FileRec {
            dir,
            t0: now(),
            copied: HashSet::new(),
            n: 0,
            name: format!("{} {stamp}", self.cfg.prefix()),
            first_end: None,
        });
        sys::click(&self.cfg);
        self.notify(&format!("Recording. Press {} again to stop.", pretty(&self.cfg.s("record_hotkey"))));
        self.update_tray();
        self.write_status();
    }

    /// Copy the chunks finished since the start (all of them, the open one too, when final).
    fn filerec_copy(&mut self, fin: bool) {
        let Some(rec) = self.filerec.as_mut() else { return };
        let mut segs: Vec<(u128, u64, PathBuf)> = save::seg_files()
            .into_iter()
            .filter_map(|f| {
                let m = fs::metadata(&f).ok()?;
                let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
                Some((t, m.len(), f))
            })
            .collect();
        segs.sort();
        if !fin {
            segs.pop(); // the newest chunk is still being written
        }
        for (mtime, size, f) in segs {
            // a chunk last written before the start holds nothing of the recording (it's 5 s long,
            // so the first one may begin up to 5 s early)
            if (mtime as f64 / 1e9) < rec.t0 || rec.copied.contains(&(f.clone(), mtime)) || size == 0 {
                continue;
            }
            if fs::copy(&f, rec.dir.join(format!("{:06}.ts", rec.n))).is_err() {
                continue;
            }
            if rec.n == 0 {
                rec.first_end = Some(mtime as f64 / 1e9); // when the first chunk was finished
            }
            rec.copied.insert((f, mtime));
            rec.n += 1;
        }
    }

    fn stop_file_recording(&mut self, wait: bool) {
        if self.filerec.is_none() {
            return;
        }
        let back_off = std::mem::replace(&mut self.rec_only, false);
        if back_off {
            self.stop_recorder(); // flushes the open chunk; nothing else needs the capture
        }
        self.filerec_copy(true);
        let rec = self.filerec.take().unwrap();
        sys::click(&self.cfg);
        if back_off {
            self.set_paused(true); // replays stay off, as they were
        }
        self.update_tray();
        self.write_status();
        let cfg = self.cfg.clone();
        let work = move || finish_recording(&cfg, rec, wait);
        if wait {
            work();
        } else {
            std::thread::spawn(work);
        }
    }

    // ------------------------------------------------------------ hotkeys
    fn hotkeys(&self) -> Vec<(&'static str, String)> {
        [("save", self.cfg.s("hotkey")), ("record", self.cfg.s("record_hotkey"))].into_iter().filter(|(_, a)| !a.is_empty()).collect()
    }

    /// Two global keys: save a replay (hotkey) and start / stop recording (record_hotkey).
    fn start_hotkey(&mut self) {
        self.stop_hotkey();
        for (name, accel) in [("save", self.cfg.s("hotkey")), ("record", self.cfg.s("record_hotkey"))] {
            let st = if accel.is_empty() { "no hotkey" } else { "starting" };
            self.hk_state.insert(name, (accel, false, st.into()));
        }
        let keys = self.hotkeys();
        if keys.is_empty() {
            return;
        }
        if self.wayland {
            let tx = self.tx.clone();
            let map: Vec<(String, String)> = keys.into_iter().map(|(n, a)| (n.to_string(), a)).collect();
            match WL::Shortcuts::start(self.rt.handle(), map, move |e| {
                let _ = tx.send(Ev::Wl(e));
            }) {
                Ok(s) => self.shortcuts = Some(s),
                Err(e) => {
                    for (name, accel) in self.hotkeys() {
                        self.hk_state.insert(name, (accel, false, e.clone()));
                    }
                    self.write_status();
                }
            }
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let tx = self.tx.clone();
        let list: Vec<(String, String)> = keys.into_iter().map(|(n, a)| (n.to_string(), a)).collect();
        let s2 = stop.clone();
        let h = std::thread::spawn(move || hotkey::run(list, s2, move |e| {
            let _ = tx.send(Ev::Hk(e));
        }));
        self.hk_thread = Some((stop, h));
    }

    fn stop_hotkey(&mut self) {
        if let Some((stop, h)) = self.hk_thread.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
        if let Some(s) = self.shortcuts.take() {
            s.close(self.rt.handle());
        }
    }

    fn on_hk(&mut self, e: Hk) {
        match e {
            Hk::State { name, ok, err } => {
                let name: &'static str = if name == "save" { "save" } else { "record" };
                let accel = self.cfg.s(if name == "save" { "hotkey" } else { "record_hotkey" });
                hk_log(&format!("{name} {}", if ok { "OK".to_string() } else { format!("FAIL {err}") }));
                if !ok {
                    self.notify(&format!("Hotkey {} doesn't work: {err}.", pretty(&accel)));
                }
                self.hk_state.insert(name, (accel, ok, err));
                self.write_status();
            }
            Hk::Press { name } => self.on_hotkey(&name),
        }
    }

    fn on_hotkey(&mut self, name: &str) {
        if name == "save" {
            self.save(None);
        } else {
            self.toggle_file_recording();
        }
    }

    fn on_portal_press(&mut self, name: &str) {
        let n = Instant::now();
        if self.last_press.get(name).is_none_or(|t| n.duration_since(*t) > Duration::from_millis(500)) {
            self.last_press.insert(name.to_string(), n);
            self.on_hotkey(name);
        }
    }

    // ------------------------------------------------------------ actions
    fn save(&mut self, seconds: Option<u32>) {
        hk_log("save");
        let mut c = Command::new(self_exe());
        c.arg("save");
        if let Some(s) = seconds {
            c.args(["--seconds", &s.to_string()]);
        }
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        sys::detach(&mut c);
        if let Ok(mut ch) = c.spawn() {
            std::thread::spawn(move || {
                let _ = ch.wait();
            });
        }
    }

    fn notify(&self, text: &str) {
        sys::toast(&self.cfg, text);
    }

    // ------------------------------------------------------------ updates
    fn check_update(&self) {
        if !sys::installed() {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Some(latest) = sys::latest_version() {
                C::update_json(&C::update_cache(), json!({"time": now(), "latest": latest}));
                if sys::is_newer(&latest) {
                    let _ = tx.send(Ev::UpdateFound(latest));
                }
            }
        });
    }

    fn on_update_available(&mut self, latest: String) {
        self.update = Some(latest.clone());
        self.update_tray();
        let cache = C::read_obj(&C::update_cache());
        if cache.get("notified").and_then(|n| n.as_str()) == Some(&latest)
            && now() - cache.get("notified_at").and_then(|n| n.as_f64()).unwrap_or(0.0) < 24.0 * 3600.0
        {
            return; // remind at most once a day
        }
        C::update_json(&C::update_cache(), json!({"notified": latest, "notified_at": now()}));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let icon = C::icon_path().to_string_lossy().into_owned();
            let title = format!("{} {latest} is out", C::APP_NAME);
            let r = sys::run(
                &["notify-send", "-a", C::APP_NAME, "-i", &icon, "-u", "normal", "-A", "update=Update", "-A", "default=Update", "-w", "-t", "60000", &title, "A new version is ready to install."],
                Duration::from_secs(120),
            );
            if r.is_some_and(|o| o.stdout.split_whitespace().any(|w| w == "update" || w == "default")) {
                let _ = tx.send(Ev::RunUpdate); // the button or a click on the popup
            }
        });
    }

    /// The Update button: installs in the background, no password; the package then restarts us.
    fn run_update(&mut self) {
        if self.updating {
            return;
        }
        self.updating = true;
        self.notify(&format!("Updating to {}…", self.update.clone().unwrap_or_else(|| "the new version".into())));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (ok, msg) = sys::self_update();
            let _ = tx.send(Ev::UpdateDone(ok, msg));
        });
    }

    fn update_done(&mut self, ok: Option<bool>, msg: String) {
        self.updating = false;
        match ok {
            None => {
                // installed before the helper existed: the old way, once
                if !sys::run_in_terminal(&C::install_cmd()) {
                    self.notify(&format!("Open a terminal and run: {}", C::install_cmd()));
                }
            }
            Some(false) => self.notify(&format!("Update failed: {msg}")),
            Some(true) => {}
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if !paused && self.rec_only {
            // replays turned on during a recording: the capture is already running, just keep it
            self.rec_only = false;
            self.update_tray();
            self.write_status();
            return;
        }
        self.paused = paused;
        if paused {
            self.stop_file_recording(false);
            self.stop_recorder();
            self.clear_buffer();
            self.state = "paused".into();
        } else {
            self.fails = 0;
            self.start_recorder();
        }
        self.update_tray();
        self.write_status();
    }

    fn set_option(&mut self, key: &str, value: Value) {
        let mut cfg = Config::load();
        if cfg.get(key) != &value {
            cfg.set(key, value.clone());
            let _ = cfg.save();
            self.cfg.set(key, value);
        }
    }

    /// `drzreplay record` and the window's Record button leave a command here, then send SIGHUP.
    fn handle_request(&mut self) -> bool {
        let Ok(cmd) = fs::read_to_string(C::request_file()) else { return false };
        let _ = fs::remove_file(C::request_file());
        match cmd.trim() {
            "pause" => self.set_paused(true),
            "resume" => self.set_paused(false),
            "record" => self.toggle_file_recording(),
            "record-start" => self.start_file_recording(),
            "record-stop" => self.stop_file_recording(false),
            _ => {}
        }
        true
    }

    fn reload(&mut self) {
        if self.handle_request() {
            return;
        }
        let old = self.cfg.clone();
        self.cfg = Config::load();
        if self.state == "error" || C::RECORDER_KEYS.iter().any(|k| old.get(k) != self.cfg.get(k)) {
            self.fails = 0;
            self.audio_off = false;
            if !self.paused {
                self.start_recorder();
            }
        }
        if old.s("hotkey") != self.cfg.s("hotkey") || old.s("record_hotkey") != self.cfg.s("record_hotkey") {
            self.start_hotkey();
        }
        if old.b("tray") != self.cfg.b("tray") {
            self.setup_tray();
        }
        self.update_tray();
        self.write_status();
    }

    fn write_status(&mut self) {
        let segs = save::seg_files();
        let mut buffered = 0.0f64;
        if self.state == "recording" && self.started > 0.0 {
            let mut since = self.started;
            let last = C::read_obj(&C::last_save());
            if self.cfg.b("replay_since_last") && last.get("session").and_then(|s| s.as_f64()) == Some(self.started) {
                since = since.max(last.get("end").and_then(|e| e.as_f64()).unwrap_or(0.0)); // a replay empties the buffer
            }
            buffered = (now() - since).clamp(0.0, self.cfg.f("seconds"));
        }
        let hk = |n: &str| self.hk_state.get(n).cloned().unwrap_or_default();
        let (sa, so, se) = hk("save");
        let (ra, ro, re) = hk("record");
        let mb: u64 = segs.iter().map(|s| fs::metadata(s).map(|m| m.len()).unwrap_or(0)).sum();
        let st = json!({
            "pid": std::process::id(), "state": self.state, "error": self.error, "encoder": self.enc,
            "geometry": self.geom.map(|g| vec![g.0, g.1, g.2, g.3]), "fps": self.cfg.i("fps"),
            "seconds": self.cfg.i("seconds"), "buffered": buffered, "segments": segs.len(),
            "buffer_mb": mb as f64 / 1e6,
            "hotkey": sa, "hotkey_ok": so, "hotkey_error": se, "audio_off": self.audio_off,
            "record_hotkey": ra, "record_hotkey_ok": ro, "record_hotkey_error": re, "started": self.started,
            "file_recording": self.filerec.as_ref().map(|f| f.t0),
            "replays_off": self.paused || self.rec_only,
            "encoders": self.info().available, "time": now(),
        });
        let _ = C::write_json(&C::statusfile(), &st);
    }

    // ------------------------------------------------------------ tray
    fn setup_tray(&mut self) {
        if let Some(t) = self.tray.take() {
            t.shutdown();
        }
        if self.use_tray && self.cfg.b("tray") {
            let (atx, arx) = channel::<Act>();
            match tray::spawn_any(&self.rt, atx, self.view()) {
                Ok(h) => {
                    self.tray = Some(h);
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        for a in arx {
                            if tx.send(Ev::Tray(a)).is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(e) => eprintln!("tray unavailable: {e}"),
            }
        }
    }

    fn view(&self) -> View {
        let off = self.paused || self.rec_only;
        let on = !off && self.state == "recording";
        let (record_label, record_enabled) = match &self.filerec {
            Some(f) => {
                let secs = (now() - f.t0) as i64;
                (format!("Stop recording · {}:{:02}", secs / 60, secs % 60), true)
            }
            None => {
                let rk = self.cfg.s("record_hotkey");
                let key = if rk.is_empty() { String::new() } else { format!(" ({})", pretty(&rk)) };
                (format!("Start recording{key}"), on || self.paused)
            }
        };
        View {
            save_label: format!("Save replay ({})", pretty(&self.cfg.s("hotkey"))),
            save_enabled: on,
            record_label,
            record_enabled,
            pause_label: if off { "Turn replays on" } else { "Turn replays off" }.into(),
            intro: self.cfg.b("intro"),
            update_label: self.update.as_ref().map(|u| format!("Update to {u}…")),
            recording: self.filerec.is_some() || (self.state == "recording" && !self.paused && self.proc.is_some()),
            tooltip: format!("{}: {}", C::APP_NAME, if off { "paused" } else { "recording" }),
        }
    }

    fn update_tray(&self) {
        if let Some(t) = &self.tray {
            t.update(self.view());
        }
    }

    fn refresh_icon(&self) {
        self.update_tray();
    }

    fn on_tray(&mut self, act: Act) {
        match act {
            Act::Save => self.save(None),
            Act::ToggleRecord => self.toggle_file_recording(),
            Act::TogglePause => {
                let off = self.paused || self.rec_only;
                self.set_paused(!off);
            }
            Act::ToggleIntro(on) => {
                self.set_option("intro", json!(on));
                self.update_tray();
            }
            Act::OpenFolder => {
                let d = self.cfg.output_dir();
                let _ = fs::create_dir_all(&d);
                sys::spawn_quiet(&["xdg-open", &d.to_string_lossy()]);
            }
            Act::OpenSettings => {
                let mut c = Command::new(self_exe());
                c.arg("settings");
                sys::detach(&mut c);
                let _ = c.stdin(Stdio::null()).spawn().map(|mut ch| std::thread::spawn(move || ch.wait()));
            }
            Act::Update => self.run_update(),
            Act::Restart => {
                self.reexec = true;
                self.quit = true;
            }
            Act::Quit => self.quit = true,
        }
    }

    // ------------------------------------------------------------ shutdown
    fn shutdown(&mut self) {
        self.stop_file_recording(true); // don't lose a running recording on quit / upgrade
        if let Some(c) = self.cast.take() {
            c.close(self.rt.handle());
        }
        self.stop_audio_follow();
        self.stop_hotkey();
        self.stop_recorder();
        self.clear_buffer();
        if let Some(t) = self.tray.take() {
            t.shutdown();
        }
        for f in [C::pidfile(), C::statusfile()] {
            let _ = fs::remove_file(f);
        }
    }

    // ------------------------------------------------------------ main
    pub fn run(&mut self, rx: Receiver<Ev>) {
        let _ = fs::create_dir_all(C::runtime());
        let _ = fs::write(C::pidfile(), std::process::id().to_string());
        self.kill_stale_recorders();
        self.setup_tray(); // first: the icon shows up right away
        std::thread::sleep(Duration::from_millis(150)); // let the tray register before the encoder probe
        self.info = Some(if self.wayland { self.wayland_encoders() } else { sys::detect_encoders(false) });
        if self.paused {
            // started for a full recording with replays off
            self.state = "paused".into();
        } else {
            self.start_recorder();
        }
        self.start_hotkey();
        self.start_audio_follow();
        let cache = C::read_obj(&C::update_cache());
        let running = cache.get("running").and_then(|r| r.as_str()).map(String::from);
        if running.as_deref().is_some_and(|r| r != C::VERSION) {
            self.notify(&format!("Updated to {}. Still recording.", C::VERSION));
        }
        if running.as_deref() != Some(C::VERSION) {
            C::update_json(&C::update_cache(), json!({"running": C::VERSION}));
        }
        self.update_tray();
        self.write_status();
        let started = Instant::now();
        let mut next_tick = Instant::now() + Duration::from_secs(1);
        let mut update_checked = false;
        let mut next_update = Instant::now() + Duration::from_secs(6 * 3600);
        while !self.quit {
            let wait = next_tick.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(ev) => self.on_event(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= next_tick {
                next_tick += Duration::from_secs(1);
                if next_tick < Instant::now() {
                    next_tick = Instant::now() + Duration::from_secs(1);
                }
                self.health();
                if self.filerec.is_some() {
                    self.filerec_copy(false);
                }
                if !update_checked && started.elapsed() > Duration::from_secs(60) {
                    update_checked = true;
                    self.check_update();
                }
                if Instant::now() >= next_update {
                    next_update += Duration::from_secs(6 * 3600);
                    self.check_update();
                }
            }
        }
        self.shutdown();
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Signal(s) => match s {
                libc::SIGHUP => self.reload(),
                libc::SIGUSR1 => self.save(None),
                libc::SIGUSR2 => {
                    self.reexec = true;
                    self.quit = true;
                }
                _ => self.quit = true,
            },
            Ev::Hk(e) => self.on_hk(e),
            Ev::Tray(a) => self.on_tray(a),
            Ev::Wl(e) => self.on_wl(e),
            Ev::FollowDefault => self.follow_default(),
            Ev::UpdateFound(v) => self.on_update_available(v),
            Ev::RunUpdate => self.run_update(),
            Ev::UpdateDone(ok, msg) => self.update_done(ok, msg),
        }
    }
}

/// Join the copied chunks into one video file (without re-encoding) and tell the user.
fn finish_recording(cfg: &Config, rec: FileRec, wait: bool) {
    let mut parts: Vec<PathBuf> = fs::read_dir(&rec.dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "ts")).collect())
        .unwrap_or_default();
    parts.sort();
    if parts.is_empty() {
        let _ = fs::remove_dir_all(&rec.dir);
        sys::toast(cfg, "The recording was empty.");
        return;
    }
    // the first chunk began before the key was pressed: start at the keyframe nearest the press
    let first = ts::analyze_file(&parts[0]).unwrap_or_default();
    let first_len = first.duration;
    let offset = rec.t0 - (rec.first_end.unwrap_or(rec.t0) - first_len);
    let key = if offset > 0.2 && offset < first_len { save::keyframe_before(&parts[0], &first.keys, offset).map(|k| k.0) } else { None };
    let listfile = rec.dir.join("list.txt");
    let mut text = String::new();
    for (i, p) in parts.iter().enumerate() {
        text.push_str(&format!("file '{}'\n", p.display()));
        if let (0, Some(k)) = (i, key) {
            text.push_str(&format!("inpoint {k:.3}\n"));
        }
    }
    let _ = fs::write(&listfile, text);
    let ext = cfg.ext();
    let out = cfg.output_dir().join(format!("{}.{ext}", rec.name));
    let tmp = rec.dir.join(format!("out.{ext}"));
    let mut args: Vec<String> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"].map(String::from).to_vec();
    args.push(listfile.to_string_lossy().into_owned());
    args.extend(["-map", "0", "-c", "copy"].map(String::from));
    if ext == "mp4" {
        args.extend(["-movflags", "+faststart"].map(String::from));
    }
    args.push(tmp.to_string_lossy().into_owned());
    let r = sys::run_cmd(save::ffmpeg_cmd(args, Some(10)), Duration::from_secs(3600));
    match r {
        Some(r) if r.ok() && tmp.exists() => {}
        other => {
            let err = other.map(|r| r.last_err("ffmpeg failed")).unwrap_or_else(|| "ffmpeg failed".into());
            sys::toast(cfg, &format!("Couldn't save the recording ({err}). The pieces are in {}", rec.dir.display()));
            return;
        }
    }
    let _ = fs::rename(&tmp, &out);
    let _ = fs::remove_dir_all(&rec.dir);
    let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / 1e9;
    let length = (now() - rec.t0) as i64;
    let base = out.file_name().unwrap().to_string_lossy().into_owned();
    let msg = if size >= 0.1 {
        format!("Recording saved: {base} · {}:{:02} · {size:.2} GB", length / 60, length % 60)
    } else {
        format!("Recording saved: {base} · {}:{:02}", length / 60, length % 60)
    };
    let res = sys::notify(cfg, &msg, None, "normal", None, &[("play", "Play"), ("folder", "Show in folder")], !wait);
    if let Some(r) = res {
        if r.iter().any(|x| x == "play") {
            sys::spawn_quiet(&["xdg-open", &out.to_string_lossy()]);
        } else if r.iter().any(|x| x == "folder") {
            sys::show_in_folder(&out);
        }
    }
}

pub fn main(args: &[String]) -> i32 {
    let _ = fs::create_dir_all(C::runtime());
    let lock = fs::OpenOptions::new().write(true).create(true).truncate(true).open(C::runtime().join("daemon.lock"));
    if let Ok(l) = &lock {
        // i3 may start us twice (dex autostart + exec line), only one may record
        if unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            println!("{} is already running (pid {})", C::APP_NAME, C::daemon_pid().unwrap_or(0));
            return 0;
        }
    }
    let (tx, rx) = channel();
    {
        let tx = tx.clone();
        let mut sigs = signal_hook::iterator::Signals::new([libc::SIGHUP, libc::SIGUSR1, libc::SIGUSR2, libc::SIGTERM, libc::SIGINT]).expect("signals");
        std::thread::spawn(move || {
            for s in sigs.forever() {
                if tx.send(Ev::Signal(s)).is_err() {
                    break;
                }
            }
        });
    }
    let mut d = Daemon::new(!args.iter().any(|a| a == "--no-tray"), args.iter().any(|a| a == "--paused"), tx);
    d.run(rx);
    if d.reexec {
        drop(lock);
        // come back the way it was: replays on, or off
        let mut a: Vec<String> = args.iter().filter(|a| *a != "--paused").cloned().collect();
        if d.paused {
            a.push("--paused".into());
        }
        let err = Command::new(self_exe()).arg("daemon").args(a).exec();
        eprintln!("can't restart: {err}");
        return 1;
    }
    0
}
