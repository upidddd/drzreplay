//! DrzReplay: instant replay for Linux.
//!
//! usage: drzreplay [command]
//!
//!   (no command)   open Settings
//!   daemon         run the background recorder (tray + hotkey)
//!   start          start the recorder in the background if it is not running
//!   stop           stop the recorder
//!   restart        restart the recorder
//!   save [--seconds N]  save a replay now (optionally only the last N seconds)
//!   record [start|stop] start / stop recording a full video (no argument: toggle)
//!   status         show what the recorder is doing
//!   preview-intro  play the intro animation
//!   --version
mod autostart;
mod config;
mod daemon;
mod hotkey;
mod intro;
mod save;
mod system;
mod tray;
mod trim;
mod ts;
#[cfg(feature = "ui")]
mod ui;
mod wayland;
mod xtray;

use config::{self as C, Config};
use std::process::{Command, Stdio};
use std::time::Duration;

const USAGE: &str = "DrzReplay — instant replay for Linux.

usage: drzreplay [command]

  (no command)   open Settings
  daemon         run the background recorder (tray + hotkey)
  start          start the recorder in the background if it is not running
  stop           stop the recorder
  restart        restart the recorder
  save [--seconds N]  save a replay now (optionally only the last N seconds)
  record [start|stop] start / stop recording a full video (no argument: toggle)
  status         show what the recorder is doing
  preview-intro  play the intro animation
  --version";

fn cmd_start(args: &[String]) -> i32 {
    if let Some(pid) = C::daemon_pid() {
        println!("{} is running (pid {pid})", C::APP_NAME);
        return 0;
    }
    let mut c = Command::new(daemon::self_exe());
    c.arg("daemon").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    system::detach(&mut c);
    if c.spawn().is_err() {
        eprintln!("{} did not start, try: drzreplay daemon", C::APP_NAME);
        return 1;
    }
    for _ in 0..30 {
        if C::daemon_pid().is_some() {
            println!("{} started", C::APP_NAME);
            return 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("{} did not start, try: drzreplay daemon", C::APP_NAME);
    1
}

fn cmd_stop() -> i32 {
    let Some(pid) = C::daemon_pid() else {
        println!("not running");
        return 0;
    };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..60 {
        if C::daemon_pid().is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("stopped");
    0
}

fn cmd_restart(args: &[String]) -> i32 {
    cmd_stop();
    cmd_start(args)
}

fn cmd_record(args: &[String]) -> i32 {
    let mut pid = C::daemon_pid();
    let word = args.first().map(String::as_str).unwrap_or("start");
    if pid.is_none() && word != "stop" {
        // not running at all: start it with replays off, just for this video
        cmd_start(&["--paused".to_string()]);
        for _ in 0..50 {
            pid = C::daemon_pid();
            if pid.is_some() && C::read_obj(&C::statusfile()).get("pid").and_then(|p| p.as_i64()) == pid.map(|p| p as i64) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let Some(pid) = pid else {
        eprintln!("{} is not running (drzreplay start)", C::APP_NAME);
        return 1;
    };
    let cmd = match args.first().map(String::as_str) {
        Some("start") => "record-start",
        Some("stop") => "record-stop",
        _ => "record",
    };
    request(pid, cmd);
    0
}

/// leave a command for the daemon, then wake it with SIGHUP
pub fn request(pid: i32, cmd: &str) {
    let tmp = C::runtime().join("request.tmp");
    if std::fs::write(&tmp, cmd).is_ok() {
        let _ = std::fs::rename(tmp, C::request_file());
        unsafe { libc::kill(pid, libc::SIGHUP) };
    }
}

fn cmd_status() -> i32 {
    let pid = C::daemon_pid();
    let st = if pid.is_some() { C::read_obj(&C::statusfile()) } else { Default::default() };
    let cfg = Config::load();
    let s = |k: &str| st.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let f = |k: &str| st.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let state = if pid.is_some() { st.get("state").and_then(|v| v.as_str()).unwrap_or("stopped").to_string() } else { "stopped".into() };
    println!("recorder : {state}{}", pid.map(|p| format!(" (pid {p})")).unwrap_or_default());
    if !s("error").is_empty() {
        println!("error    : {}", s("error"));
    }
    if pid.is_some() {
        let g: Vec<i64> = st.get("geometry").and_then(|g| g.as_array()).map(|a| a.iter().filter_map(|v| v.as_i64()).collect()).unwrap_or_default();
        let g = if g.len() >= 2 { g } else { vec![0, 0] };
        println!("capture  : {}x{} @ {} fps, encoder {}", g[0], g[1], f("fps"), s("encoder"));
        println!("buffer   : {:.0}/{} s, {:.0} MB in RAM", f("buffered"), f("seconds"), f("buffer_mb"));
        let ok = |k: &str| if st.get(&format!("{k}_ok")).and_then(|v| v.as_bool()).unwrap_or(false) { "ok".to_string() } else { format!("NOT working ({})", s(&format!("{k}_error"))) };
        println!("hotkey   : {} — {}", hotkey::pretty(&cfg.s("hotkey")), ok("hotkey"));
        if !cfg.s("record_hotkey").is_empty() {
            println!("rec key  : {} — {}", hotkey::pretty(&cfg.s("record_hotkey")), ok("record_hotkey"));
        }
        if let Some(since) = st.get("file_recording").and_then(|v| v.as_f64()) {
            let secs = (C::now() - since) as i64;
            println!("video    : recording, {}:{:02}", secs / 60, secs % 60);
        }
    }
    println!("replays  : {}", cfg.output_dir().display());
    0
}

fn cmd_preview_intro() -> i32 {
    let cfg = Config::load();
    let _ = std::fs::create_dir_all(C::runtime());
    let (clip, out) = (C::runtime().join("intro-preview.mov"), C::runtime().join("intro-preview.mp4"));
    if let Err(e) = intro::render(&clip, intro::next_number(&cfg), chrono::Local::now(), 1080, 30) {
        eprintln!("{e}");
        return 1;
    }
    let (x, y) = intro::position(1080);
    let fc = format!(
        "gradients=s=1920x1080:c0=0x3a4552:c1=0x1d2229:x0=0:y0=0:x1=1920:y1=1080:speed=0.01:r=30:d=4[bg];[0:v]setpts=PTS-STARTPTS+{}/TB[ov];[bg][ov]overlay={x}:{y}:eof_action=pass,format=yuv420p[v]",
        intro::START
    );
    let r = system::run(
        &[
            "ffmpeg", "-nostdin", "-v", "error", "-y", "-i", &clip.to_string_lossy(), "-filter_complex", &fc, "-map", "[v]", "-t", "4", "-c:v", "libx264",
            "-preset", "veryfast", "-crf", "20", &out.to_string_lossy(),
        ],
        Duration::from_secs(60),
    );
    let _ = std::fs::remove_file(&clip);
    if !r.is_some_and(|o| o.ok()) {
        return 1;
    }
    system::spawn_quiet(&["xdg-open", &out.to_string_lossy()]);
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("settings");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    let code = match cmd {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            0
        }
        "-v" | "--version" => {
            println!("{} {}", C::APP_NAME, C::VERSION);
            0
        }
        "daemon" => daemon::main(rest),
        "save" => save::main(rest),
        #[cfg(feature = "ui")]
        "settings" => ui::main(),
        "start" => cmd_start(rest),
        "stop" => cmd_stop(),
        "restart" => cmd_restart(rest),
        "status" => {
            // like any command-line tool: quietly stop when the reader of the pipe (head…) goes away
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            cmd_status()
        }
        "record" => cmd_record(rest),
        "preview-intro" => cmd_preview_intro(),
        "debug-ts" => {
            for f in rest {
                let t = std::time::Instant::now();
                let a = ts::analyze_file(std::path::Path::new(f));
                println!("{f}: {:?} in {:?}", a.as_ref().map(|a| (a.duration, a.size, a.keys.len())), t.elapsed());
            }
            0
        }
        _ => {
            println!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
