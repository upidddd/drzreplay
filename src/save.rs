//! Turn the ring buffer into a finished clip (with the intro on top when it is on).
use crate::config::{self as C, Config};
use crate::intro;
use crate::system::{self as sys, Encoders};
use crate::ts;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// a program that runs at low priority: nice, and idle disk time
pub fn low_priority(mut c: Command, nice: i32) -> Command {
    use std::os::unix::process::CommandExt;
    unsafe {
        c.pre_exec(move || {
            libc::setpriority(libc::PRIO_PROCESS, 0, nice);
            // ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << 13)
            libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
            Ok(())
        });
    }
    c
}

pub fn ffmpeg_cmd(args: Vec<String>, nice: Option<i32>) -> Command {
    let mut c = Command::new("ffmpeg");
    c.args(args);
    match nice {
        Some(n) => low_priority(c, n),
        None => c,
    }
}

fn mtime(p: &Path) -> SystemTime {
    fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

// ---------------------------------------------------------------- probing
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub duration: f64,
    pub has_audio: bool,
    pub size: Option<(u32, u32)>,
    pub keys: Vec<f64>,
}

impl Probe {
    fn from(a: ts::Analysis) -> Probe {
        Probe { duration: a.duration, has_audio: a.has_audio, size: a.size, keys: a.keys }
    }
}

/// duration, streams and size of a media file (any container): ffprobe
pub fn ffprobe(path: &Path) -> Probe {
    let p = path.to_string_lossy();
    let Some(o) = sys::run(
        &["ffprobe", "-v", "error", "-show_entries", "format=duration:stream=codec_type,width,height", "-of", "json", &p],
        Duration::from_secs(10),
    ) else {
        return Probe::default();
    };
    let Ok(j) = serde_json::from_str::<serde_json::Value>(&o.stdout) else { return Probe::default() };
    let streams = j["streams"].as_array().cloned().unwrap_or_default();
    let v = streams.iter().find(|s| s["codec_type"] == "video");
    Probe {
        duration: j["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0),
        has_audio: streams.iter().any(|s| s["codec_type"] == "audio"),
        size: v.and_then(|v| Some((v["width"].as_u64()? as u32, v["height"].as_u64()? as u32))),
        keys: vec![],
    }
}

/// A ring-buffer chunk is read straight from its packets; anything else (or a broken chunk) asks ffprobe.
pub fn probe_ts(path: &Path, data: Option<&[u8]>) -> Probe {
    let a = match data {
        Some(d) => ts::analyze(d),
        None => ts::analyze_file(path),
    };
    match a {
        Some(a) if a.duration > 0.0 && a.size.is_some() => Probe::from(a),
        Some(a) if a.duration > 0.0 => {
            // no picture size in the stream header: ask ffprobe for just that
            let mut p = Probe::from(a);
            p.size = ffprobe(path).size;
            p
        }
        _ => ffprobe(path),
    }
}

/// For a concat `inpoint`: the keyframe at or before `offset` seconds into the file, in the file's own
/// timestamps (mpegts chunks don't start at 0), and how many seconds before `offset` it is.
pub fn keyframe_before(path: &Path, keys: &[f64], offset: f64) -> Option<(f64, f64)> {
    let mut keys = keys.to_vec();
    if keys.is_empty() {
        let p = path.to_string_lossy();
        let o = sys::run(
            &["ffprobe", "-v", "error", "-select_streams", "v", "-skip_frame", "nokey", "-show_entries", "frame=pts_time", "-of", "csv=p=0", &p],
            Duration::from_secs(10),
        )?;
        keys = o.stdout.split_whitespace().filter_map(|l| l.trim_matches(',').parse().ok()).collect();
    }
    let first = *keys.first()?;
    let target = first + offset;
    let key = keys.iter().copied().filter(|k| *k <= target + 0.02).fold(first, f64::max);
    Some((key, (target - key).max(0.0)))
}

// ---------------------------------------------------------------- housekeeping
/// Delete the oldest clips beyond the limits. Only touches files DrzReplay made.
pub fn cleanup(cfg: &Config, keep: &Path) -> usize {
    let d = cfg.output_dir();
    let prefix = format!("{} ", cfg.prefix());
    let mut clips: Vec<PathBuf> = fs::read_dir(&d)
        .map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    clips.retain(|p| {
        let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        n.starts_with(&prefix) && n.contains('.') && (n.ends_with(".mp4") || n.ends_with(".mkv")) && p.is_file()
    });
    clips.sort_by_key(|p| mtime(p));
    let keep_abs = keep.canonicalize().unwrap_or(keep.to_path_buf());
    clips.retain(|p| p.canonicalize().unwrap_or(p.clone()) != keep_abs);
    let mut removed = 0;
    let max_n = cfg.i("keep_max_clips");
    if max_n > 0 {
        while clips.len() as i64 + 1 > max_n && !clips.is_empty() {
            let _ = fs::remove_file(clips.remove(0));
            removed += 1;
        }
    }
    let max_b = cfg.f("keep_max_gb") * 1e9;
    if max_b > 0.0 {
        let size = |p: &Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0) as f64;
        let mut total: f64 = clips.iter().map(|p| size(p)).sum::<f64>() + size(keep);
        while total > max_b && !clips.is_empty() {
            let p = clips.remove(0);
            total -= size(&p);
            let _ = fs::remove_file(p);
            removed += 1;
        }
    }
    removed
}

pub fn export_size(cfg: &Config, native: (u32, u32)) -> (u32, u32) {
    let (nw, nh) = native;
    let eh = cfg.i("export_height");
    if eh > 0 && (eh as u32) < nh {
        let w = (nw as f64 * eh as f64 / nh as f64 / 2.0).round() as u32 * 2;
        (w, eh as u32)
    } else {
        (nw, nh)
    }
}

fn s(x: impl Into<String>) -> String {
    x.into()
}

/// the encoder part of an export command, shared by saving and trimming
pub fn encoder_args(enc: &str, q: i64, preset_nv: &str, tail_nv: &[&str]) -> Vec<String> {
    match enc {
        "nvenc" => {
            let mut v: Vec<String> = ["-c:v", "h264_nvenc", "-preset", preset_nv, "-rc", "vbr", "-cq", &q.to_string(), "-b:v", "0"]
                .iter()
                .map(|x| x.to_string())
                .collect();
            v.extend(tail_nv.iter().map(|x| x.to_string()));
            v
        }
        "vaapi" => ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", &q.to_string()].iter().map(|x| x.to_string()).collect(),
        _ => ["-c:v", "libx264", "-preset", "veryfast", "-crf", &q.to_string(), "-pix_fmt", "yuv420p", "-threads", "2"]
            .iter()
            .map(|x| x.to_string())
            .collect(),
    }
}

/// Re-encode: exact cut, resize, intro. `overlay` is (clip, x, y) of the rendered intro or None.
#[allow(clippy::too_many_arguments)]
fn build_cmd(
    cfg: &Config,
    enc: &str,
    hwdec: bool,
    info: &Encoders,
    listfile: &Path,
    start: f64,
    length: f64,
    has_audio: bool,
    native: (u32, u32),
    out: &Path,
    overlay: Option<&(PathBuf, i64, i64)>,
) -> Vec<String> {
    let (w, h) = export_size(cfg, native);
    let fps = cfg.i("fps");
    let mut cmd: Vec<String> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-y"].map(s).to_vec();
    if enc == "vaapi" {
        cmd.extend([s("-init_hw_device"), format!("vaapi=va:{}", info.vaapi_device.clone().unwrap_or_default()), s("-filter_hw_device"), s("va")]);
    }
    if hwdec {
        cmd.extend([s("-hwaccel"), s("cuda")]);
    }
    // no input seeking: concat + -ss lands on chunk boundaries and loses seconds, trim is exact
    cmd.extend([s("-f"), s("concat"), s("-safe"), s("0"), s("-i"), listfile.to_string_lossy().into_owned()]);
    let vcut = format!("trim=start={start:.3}:duration={length:.3},setpts=PTS-STARTPTS");
    let acut = format!("atrim=start={start:.3}:duration={length:.3},asetpts=PTS-STARTPTS");

    let mut fc: Vec<String> = vec![];
    let mut vout = format!("[0:v]{vcut},fps={fps},scale={w}:{h},format=yuv420p");
    if let Some((clip, x, y)) = overlay {
        cmd.extend([s("-i"), clip.to_string_lossy().into_owned()]);
        fc.push(format!("{vout}[bg]"));
        // shifted to its start; before that the overlay has no frame and the replay shows as is
        fc.push(format!("[1:v]setpts=PTS-STARTPTS+{}/TB[ov]", intro::START));
        vout = format!("[bg][ov]overlay={x}:{y}:eof_action=pass,format=yuv420p");
    }
    if has_audio {
        fc.push(format!("[0:a]{acut},aresample=48000[a]"));
    }
    if enc == "vaapi" {
        vout.push_str(",format=nv12,hwupload");
    }
    fc.push(format!("{vout}[v]"));

    let q = C::export_quality(&cfg.s("export_quality"));
    cmd.extend([s("-filter_complex"), fc.join(";"), s("-map"), s("[v]")]);
    if has_audio {
        cmd.extend(["-map", "[a]", "-c:a", "aac", "-b:a", "192k"].map(s));
    }
    cmd.extend([s("-t"), format!("{length:.3}")]);
    cmd.extend(encoder_args(enc, q, "p5", &["-maxrate", "60M", "-bufsize", "60M", "-profile:v", "high", "-pix_fmt", "yuv420p"]));
    if out.extension().is_some_and(|e| e == "mp4") {
        cmd.extend(["-movflags", "+faststart"].map(s));
    }
    cmd.push(out.to_string_lossy().into_owned());
    cmd
}

/// No re-encode: the recording is copied as is, starting at the keyframe (one a second) before the cut.
/// Instant and with no load on the graphics card while a game runs.
fn copy_cmd(listfile: &Path, length: f64, out: &Path) -> Vec<String> {
    let mut cmd: Vec<String> =
        ["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"].map(s).to_vec();
    cmd.push(listfile.to_string_lossy().into_owned());
    cmd.extend(["-map", "0:v", "-map", "0:a?", "-c", "copy", "-t"].map(s));
    cmd.push(format!("{length:.3}"));
    if out.extension().is_some_and(|e| e == "mp4") {
        cmd.extend(["-movflags", "+faststart"].map(s));
    }
    cmd.push(out.to_string_lossy().into_owned());
    cmd
}

/// a quick second press still gets a moment, not an empty clip
const MIN_SINCE_LAST: f64 = 1.0;

/// Seconds recorded since the previous replay ended, or None when it doesn't apply
/// (option off, first replay, or the recorder restarted since: then its buffer is new anyway).
pub fn since_last_save(cfg: &Config, pressed: f64) -> Option<f64> {
    if !cfg.b("replay_since_last") {
        return None;
    }
    let last = C::read_obj(&C::last_save());
    let status = C::read_obj(&C::statusfile());
    let session = status.get("started")?.as_f64()?;
    if last.get("session")?.as_f64()? != session {
        return None;
    }
    Some(MIN_SINCE_LAST.max(pressed - last.get("end").and_then(|e| e.as_f64()).unwrap_or(0.0)))
}

pub fn seg_files() -> Vec<PathBuf> {
    let mut segs: Vec<PathBuf> = fs::read_dir(C::runtime())
        .map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    segs.retain(|p| {
        p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.starts_with("seg_") && n.ends_with(".ts")
        })
    });
    segs.sort_by_key(|p| mtime(p));
    segs
}

fn lock_file(name: &str) -> Option<fs::File> {
    let _ = fs::create_dir_all(C::runtime());
    fs::OpenOptions::new().write(true).create(true).truncate(true).open(C::runtime().join(name)).ok()
}

pub fn main(args: &[String]) -> i32 {
    let pressed = C::now(); // before waiting for an earlier save: this is when the key was hit
    let cfg = Config::load();
    let notify = |body: &str, icon: Option<&str>, urgency: &str| sys::notify(&cfg, body, icon, urgency, None, &[], false);
    let lock = lock_file("save.lock");
    if let Some(l) = &lock {
        let fd = l.as_raw_fd();
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            // pressed again while a clip is being written: save another one right after it
            notify("Saving after the current clip…", None, "normal");
            unsafe { libc::flock(fd, libc::LOCK_EX) };
        }
    }

    sys::click(&cfg);

    let status = C::read_obj(&C::statusfile());
    if C::daemon_pid().is_some() && status.get("replays_off").and_then(|v| v.as_bool()).unwrap_or(false) {
        // paused, or capturing only for a full recording
        notify("Replays are off. Turn them on in the window or the tray.", Some("dialog-error"), "critical");
        return 1;
    }
    let size_of = |segs: &[PathBuf]| segs.iter().map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum::<u64>();
    let mut segs = seg_files();
    if !segs.is_empty() && size_of(&segs) < 200_000 {
        std::thread::sleep(Duration::from_millis(1500)); // recorder just started: give it a moment to write something
        segs = seg_files();
    }
    if segs.is_empty() {
        let paused = C::daemon_pid().is_some() && status.get("state").and_then(|v| v.as_str()) == Some("paused");
        notify(if paused { "Recording is paused." } else { "Recording is off." }, Some("dialog-error"), "critical");
        return 1;
    }
    let mut want = cfg.f("seconds");
    if let Some(i) = args.iter().position(|a| a == "--seconds") {
        // "save last 15 s" from the tray / settings
        if let Some(n) = args.get(i + 1).and_then(|n| n.parse::<f64>().ok()) {
            want = want.min(n);
        }
    } else if let Some(since) = since_last_save(&cfg, pressed) {
        // 15 s after the last replay: save those 15 s, not the whole buffer again
        want = want.min(since);
    }

    // copy the needed chunks (newest first) so the recorder cannot overwrite them mid-export
    let snap = C::runtime().join("snap");
    let _ = fs::remove_dir_all(&snap);
    if fs::create_dir_all(&snap).is_err() {
        notify("Save failed: can't write to the memory folder", Some("dialog-error"), "critical");
        return 1;
    }
    struct Piece {
        path: PathBuf,
        dur: f64,
        keys: Vec<f64>,
    }
    let (mut picked, mut total, mut has_audio, mut native): (Vec<Piece>, f64, bool, Option<(u32, u32)>) =
        (vec![], 0.0, false, None);
    for (i, seg) in segs.iter().enumerate().rev() {
        let dst = snap.join(format!("{:04}.ts", i + 1));
        let Ok(data) = fs::read(seg) else { continue };
        if fs::write(&dst, &data).is_err() {
            continue;
        }
        let p = probe_ts(&dst, Some(&data));
        let Some(size) = p.size else { continue };
        if p.duration <= 0.05 {
            continue;
        }
        if native.is_some_and(|n| n != size) {
            break; // resolution changed mid-buffer: stop here
        }
        native = Some(size);
        has_audio |= p.has_audio;
        total += p.duration;
        picked.push(Piece { path: dst, dur: p.duration, keys: p.keys });
        if total >= want + 0.5 {
            break;
        }
    }
    picked.reverse();
    let Some(native) = native.filter(|_| !picked.is_empty()) else {
        notify("Nothing recorded yet.", Some("dialog-error"), "normal");
        return 1;
    };
    let listfile = snap.join("list.txt");
    let _ = fs::write(&listfile, picked.iter().map(|p| format!("file '{}'\n", p.path.display())).collect::<String>());
    let start = (total - want).max(0.0);
    let length = want.min(total);
    // the same cut for a plain copy: skip whole chunks before it, start inside the first one
    // it starts on the keyframe before the cut, so it's up to a second longer
    let copylist = snap.join("copy.txt");
    let mut copy_len = length;
    {
        let mut f = String::new();
        let mut skip = start;
        for p in &picked {
            if skip >= p.dur {
                skip -= p.dur;
                continue;
            }
            f.push_str(&format!("file '{}'\n", p.path.display()));
            if skip > 0.05 {
                if let Some((key, extra)) = keyframe_before(&p.path, &p.keys, skip) {
                    f.push_str(&format!("inpoint {key:.3}\n"));
                    copy_len += extra;
                }
            }
            skip = 0.0;
        }
        let _ = fs::write(&copylist, f);
    }
    // the buffer counts as empty from the press on (the window's buffer bar drops to 0 right away);
    // put the old mark back if this save fails
    let prev_mark = C::read_json(&C::last_save());
    let session = status.get("started").cloned().unwrap_or(serde_json::Value::Null);
    let _ = C::write_json(&C::last_save(), &json!({"end": pressed, "session": session}));
    let nid = sys::notify(&cfg, "Saving…", None, "normal", None, &[], false).and_then(|v| v.into_iter().next());

    let outdir = cfg.output_dir();
    let _ = fs::create_dir_all(&outdir);
    let ext = cfg.ext();
    let name = format!("{} {}.{ext}", cfg.prefix(), chrono::Local::now().format("%Y-%m-%d %H-%M-%S"));
    let out = outdir.join(&name);
    let tmp = outdir.join(format!(".{name}.part.{ext}"));

    let number = intro::next_number(&cfg); // every replay counts, intro or not
    let mut overlay = None;
    if cfg.b("intro") {
        let clip = snap.join("intro.mov");
        let height = export_size(&cfg, native).1;
        let when = chrono::DateTime::from_timestamp(pressed as i64, 0).map(|d| d.with_timezone(&chrono::Local));
        match intro::render(&clip, number, when.unwrap_or_else(chrono::Local::now), height, cfg.i("fps") as u32) {
            Ok(_) => {
                let (x, y) = intro::position(height);
                overlay = Some((clip, x, y));
            }
            Err(e) => eprintln!("intro: {e}"), // a broken intro must not cost the replay
        }
    }

    let info = sys::detect_encoders(false);
    // (command line, run at low priority)
    let mut attempts: Vec<(Vec<String>, bool)> = vec![];
    if overlay.is_none() && export_size(&cfg, native) == native {
        attempts.push((copy_cmd(&copylist, copy_len, &tmp), false));
    }
    for e in sys::encoder_order(&cfg, &info) {
        if e == "nvenc" {
            attempts.push((build_cmd(&cfg, &e, true, &info, &listfile, start, length, has_audio, native, &tmp, overlay.as_ref()), true));
        }
        attempts.push((build_cmd(&cfg, &e, false, &info, &listfile, start, length, has_audio, native, &tmp, overlay.as_ref()), true));
    }
    let mut err = s("unknown error");
    let mut done = false;
    for (cmd, low) in attempts {
        let c = ffmpeg_cmd(cmd, if low { Some(15) } else { None });
        match sys::run_cmd(c, Duration::from_secs(3600)) {
            Some(r) if r.ok() && fs::metadata(&tmp).map(|m| m.len() > 1000).unwrap_or(false) => {
                done = true;
                break;
            }
            Some(r) => err = r.last_err("unknown error"),
            None => err = s("ffmpeg didn't finish"),
        }
    }
    if !done {
        let _ = fs::remove_dir_all(&snap);
        let _ = fs::remove_file(&tmp);
        match prev_mark {
            None => {
                let _ = fs::remove_file(C::last_save());
            }
            Some(m) => {
                let _ = C::write_json(&C::last_save(), &m);
            }
        }
        sys::notify(&cfg, &format!("Save failed: {err}"), Some("dialog-error"), "critical", nid.as_deref(), &[], false);
        return 1;
    }
    let _ = fs::rename(&tmp, &out);
    intro::commit(number);
    let _ = fs::remove_dir_all(&snap);
    cleanup(&cfg, &out);
    if let Some(l) = &lock {
        unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_UN) };
    }
    println!("{}", out.display());
    let _ = std::io::stdout().flush();

    let res = sys::notify(
        &cfg,
        &format!("Saved: {name}"),
        None,
        "normal",
        nid.as_deref(),
        &[("play", "Play"), ("folder", "Show in folder")],
        true,
    );
    if let Some(r) = res {
        if r.iter().any(|x| x == "play") {
            sys::spawn_quiet(&["xdg-open", &out.to_string_lossy()]);
        } else if r.iter().any(|x| x == "folder") {
            sys::show_in_folder(&out);
        }
    }
    0
}
