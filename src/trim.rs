//! Clips tab: cut a piece out of a saved clip (frame-exact, re-encoded like a normal save) and rename clips.
use crate::config::{self as C, Config};
use crate::save::{encoder_args, ffmpeg_cmd};
use crate::system::{self as sys, Encoders};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// names the app gives clips itself: "<prefix> 2026-11-09 14-03-27", maybe " (trimmed)" / " (trimmed 2)"
pub fn is_auto_name(path: &Path) -> bool {
    let name = stem(path);
    let mut n = name.as_str();
    if let Some(i) = n.rfind(" (trimmed") {
        let tail = &n[i + 9..];
        let ok = tail == ")" || (tail.starts_with(' ') && tail.ends_with(')') && tail[1..tail.len() - 1].chars().all(|c| c.is_ascii_digit()) && tail.len() > 2);
        if ok {
            n = &n[..i];
        }
    }
    let b = n.as_bytes();
    if b.len() < 19 {
        return false;
    }
    let t = &b[b.len() - 19..];
    // 2026-11-09 14-03-27
    t.iter().enumerate().all(|(i, c)| match i {
        4 | 7 | 13 | 16 => *c == b'-',
        10 => *c == b' ',
        _ => c.is_ascii_digit(),
    })
}

/// Give the clip a new name (the extension stays). Returns the new path; Err says what's wrong.
pub fn rename(path: &Path, name: &str) -> Result<PathBuf, String> {
    let mut name = name.trim().to_string();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    if name.to_lowercase().ends_with(&ext.to_lowercase()) && !ext.is_empty() {
        name.truncate(name.len() - ext.len());
        name = name.trim_end().to_string();
    }
    if name.is_empty() || name == "." || name == ".." {
        return Err("The name is empty".into());
    }
    if name.contains('/') || name.contains('\0') {
        return Err("A name can't contain /".into());
    }
    if name.starts_with('.') {
        return Err("A name starting with a dot would hide the clip".into());
    }
    let new = path.with_file_name(format!("{name}{ext}"));
    if new == path {
        return Ok(new);
    }
    if new.exists() {
        return Err("A clip with this name already exists".into());
    }
    fs::rename(path, &new).map_err(|e| {
        let m = e.to_string();
        m.split(" (os error").next().unwrap_or("Couldn't rename").to_string()
    })?;
    Ok(new)
}

fn output_path(src: &Path) -> PathBuf {
    let base = stem(src);
    let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut path = src.with_file_name(format!("{base} (trimmed){ext}"));
    let mut n = 2;
    while path.exists() {
        path = src.with_file_name(format!("{base} (trimmed {n}){ext}"));
        n += 1;
    }
    path
}

fn build_cmd(cfg: &Config, enc: &str, info: &Encoders, src: &Path, start: f64, end: f64, out: &Path) -> Vec<String> {
    let q = C::export_quality(&cfg.s("export_quality"));
    let mut cmd: Vec<String> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-y"].map(String::from).to_vec();
    if enc == "vaapi" {
        cmd.extend(["-init_hw_device".to_string(), format!("vaapi=va:{}", info.vaapi_device.clone().unwrap_or_default()), "-filter_hw_device".into(), "va".into()]);
    }
    // -ss before -i with re-encoding is exact in ffmpeg: it decodes from the keyframe and drops up to `start`
    cmd.extend(["-ss".to_string(), format!("{start:.3}"), "-i".into(), src.to_string_lossy().into_owned()]);
    cmd.extend(["-t".to_string(), format!("{:.3}", end - start), "-map".into(), "0:v:0".into(), "-map".into(), "0:a?".into()]);
    let vf = if enc == "vaapi" { "format=nv12,hwupload" } else { "format=yuv420p" };
    cmd.extend(["-vf", vf, "-c:a", "aac", "-b:a", "192k"].map(String::from));
    cmd.extend(encoder_args(enc, q, "p5", &["-profile:v", "high"]));
    if out.extension().is_some_and(|e| e == "mp4") {
        cmd.extend(["-movflags", "+faststart"].map(String::from));
    }
    cmd.push(out.to_string_lossy().into_owned());
    cmd
}

/// Returns the new file's path; Err when every encoder failed.
pub fn trim(src: &Path, start: f64, end: f64, delete_original: bool) -> Result<PathBuf, String> {
    let cfg = Config::load();
    let info = sys::detect_encoders(false);
    let out = output_path(src);
    let tmp = out.with_file_name(format!(".{}", out.file_name().unwrap().to_string_lossy()));
    let mut err = String::new();
    for enc in sys::encoder_order(&cfg, &info) {
        let c = ffmpeg_cmd(build_cmd(&cfg, &enc, &info, src, start, end, &tmp), Some(15));
        match sys::run_cmd(c, Duration::from_secs(3600)) {
            Some(r) if r.ok() && fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false) => {
                let _ = fs::rename(&tmp, &out);
                if delete_original {
                    let _ = fs::remove_file(src);
                }
                return Ok(out);
            }
            Some(r) => err = r.last_err("ffmpeg failed"),
            None => err = "ffmpeg failed".into(),
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auto_names() {
        for (n, ok) in [
            ("DrzReplay 2026-11-09 14-03-27.mp4", true),
            ("DrzReplay 2026-11-09 14-03-27 (trimmed).mp4", true),
            ("DrzReplay 2026-11-09 14-03-27 (trimmed 2).mp4", true),
            ("My boss fight.mp4", false),
            ("DrzReplay 2026-11-09 14-03-27 (trimmed x).mp4", false),
        ] {
            assert_eq!(is_auto_name(Path::new(n)), ok, "{n}");
        }
    }
}
