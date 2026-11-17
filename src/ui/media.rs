//! Clip metadata for the Clips tab: durations and thumbnails, made one by one in the background
//! and cached on disk.
use crate::config as C;
use crate::save::{ffmpeg_cmd, ffprobe};
use crate::system as sys;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

pub fn thumbs_dir() -> PathBuf {
    C::cache_dir().join("thumbs")
}

/// duration in seconds from the movie header of an MP4 (no process needed), 0 when it isn't one
fn mp4_duration(path: &Path) -> f64 {
    let Ok(mut f) = fs::File::open(path) else { return 0.0 };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut pos = 0u64;
    let mut moov = None;
    while pos + 8 <= len {
        let mut h = [0u8; 16];
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read(&mut h[..8]).unwrap_or(0) < 8 {
            break;
        }
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let kind = [h[4], h[5], h[6], h[7]];
        if size == 1 {
            if f.read_exact(&mut h[8..16]).is_err() {
                break;
            }
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
        } else if size == 0 {
            size = len - pos;
        }
        if &kind == b"moov" {
            moov = Some((pos + 8, pos + size));
            break;
        }
        if size < 8 {
            break;
        }
        pos += size;
    }
    let Some((mut p, end)) = moov else { return 0.0 };
    while p + 8 <= end {
        let mut h = [0u8; 8];
        if f.seek(SeekFrom::Start(p)).is_err() || f.read_exact(&mut h).is_err() {
            break;
        }
        let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        if &h[4..8] == b"mvhd" {
            let mut b = [0u8; 32];
            let n = f.read(&mut b).unwrap_or(0);
            if n < 20 {
                return 0.0;
            }
            let (scale, dur) = if b[0] == 1 {
                if n < 28 {
                    return 0.0;
                }
                (u32::from_be_bytes(b[20..24].try_into().unwrap()) as f64, u64::from_be_bytes(b[24..32].try_into().unwrap()) as f64)
            } else {
                (u32::from_be_bytes(b[12..16].try_into().unwrap()) as f64, u32::from_be_bytes(b[16..20].try_into().unwrap()) as f64)
            };
            return if scale > 0.0 { dur / scale } else { 0.0 };
        }
        if size < 8 {
            break;
        }
        p += size;
    }
    0.0
}

/// seconds of video in a file (any container)
pub fn duration(path: &Path) -> f64 {
    let d = mp4_duration(path);
    if d > 0.0 {
        d
    } else {
        ffprobe(path).duration
    }
}

fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

struct Request {
    id: u64,
    path: PathBuf,
}

type Callback = Box<dyn Fn(Option<PathBuf>, f64)>;

thread_local! {
    static CALLBACKS: RefCell<HashMap<u64, Callback>> = RefCell::new(HashMap::new());
}

/// Makes clip thumbnails and reads durations in one background thread, cached on disk.
pub struct ThumbLoader {
    tx: Sender<Request>,
    pending: RefCell<HashMap<PathBuf, ()>>,
    next: RefCell<u64>,
}

impl ThumbLoader {
    pub fn new() -> ThumbLoader {
        let _ = fs::create_dir_all(thumbs_dir());
        let (tx, rx) = channel::<Request>();
        std::thread::spawn(move || {
            for req in rx {
                let (jpg, dur) = make(&req.path);
                let id = req.id;
                glib::MainContext::default().invoke(move || {
                    CALLBACKS.with(|c| {
                        if let Some(cb) = c.borrow_mut().remove(&id) {
                            cb(jpg, dur);
                        }
                    });
                });
            }
        });
        ThumbLoader { tx, pending: RefCell::new(HashMap::new()), next: RefCell::new(0) }
    }

    pub fn request(&self, path: &Path, cb: impl Fn(Option<PathBuf>, f64) + 'static) {
        let mut pending = self.pending.borrow_mut();
        pending.retain(|p, _| p != path);
        let id = {
            let mut n = self.next.borrow_mut();
            *n += 1;
            *n
        };
        CALLBACKS.with(|c| c.borrow_mut().insert(id, Box::new(cb)));
        pending.insert(path.to_path_buf(), ());
        let _ = self.tx.send(Request { id, path: path.to_path_buf() });
    }
}

fn make(path: &Path) -> (Option<PathBuf>, f64) {
    let Ok(m) = fs::metadata(path) else { return (None, 0.0) };
    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let key = fnv(&format!("{}|{mtime}|{}", path.display(), m.len()));
    let (jpg, meta) = (thumbs_dir().join(format!("{key}.jpg")), thumbs_dir().join(format!("{key}.json")));
    let dur = match C::read_obj(&meta).get("duration").and_then(|d| d.as_f64()) {
        Some(d) => d,
        None => {
            let d = duration(path);
            let _ = C::write_json(&meta, &serde_json::json!({"duration": d}));
            d
        }
    };
    if !jpg.exists() {
        let args: Vec<String> = [
            "-v", "error", "-y", "-ss", &format!("{:.2}", (dur / 2.0).min(1.0)), "-i", &path.to_string_lossy(), "-frames:v", "1", "-q:v", "3", "-vf",
            "scale=96:54:force_original_aspect_ratio=increase,crop=96:54", &jpg.to_string_lossy(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let _ = sys::run_cmd(ffmpeg_cmd(args, Some(15)), Duration::from_secs(20));
    }
    (jpg.exists().then_some(jpg), dur)
}
