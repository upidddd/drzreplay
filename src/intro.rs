//! The replay intro: the replay number and the date slide out of a thin bar in the top-left corner
//! for a couple of seconds, no sound. Drawn into a short transparent clip that the export lays over
//! the start of the replay.
use crate::config::{self as C, Config};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use chrono::{DateTime, Local};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const OSWALD: &[u8] = include_bytes!("../assets/fonts/Oswald.ttf");
const INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Medium.ttf");

const CREAM: [f32; 3] = [236.0, 230.0, 214.0];
const RED: [f32; 3] = [224.0, 27.0, 36.0];
/// seconds into the clip when the bar appears
pub const START: f64 = 0.4;
/// how long it stays, in and out animations included
pub const LENGTH: f64 = 2.52;
/// against the first design (70 px title at 1080p)
const SIZE: f64 = 0.74;
const X0: f64 = 48.0;
const Y0: f64 = 40.0;

pub fn counter_path() -> std::path::PathBuf {
    C::conf_dir().join("replays.json") // {"count": replays saved so far}
}

/// Number of the replay being saved now. Counting starts from the clips already in the folder.
pub fn next_number(cfg: &Config) -> i64 {
    let count = C::read_obj(&counter_path()).get("count").and_then(|c| c.as_i64()).unwrap_or_else(|| {
        let prefix = format!("{} ", cfg.prefix());
        fs::read_dir(cfg.output_dir())
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(&prefix) && (n.ends_with(".mp4") || n.ends_with(".mkv")) && !n.contains("(trimmed)"))
                    .count() as i64
            })
            .unwrap_or(0)
    });
    count + 1
}

pub fn commit(number: i64) {
    let _ = C::write_json(&counter_path(), &json!({"count": number}));
}

pub fn position(height: u32) -> (i64, i64) {
    let k = height as f64 / 1080.0;
    ((X0 * k).round() as i64, (Y0 * k).round() as i64)
}

fn clamp(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}
fn ease_out(x: f64) -> f64 {
    1.0 - (1.0 - clamp(x)).powi(3)
}
fn ease_in(x: f64) -> f64 {
    clamp(x).powi(3)
}
fn phase(t: f64, a: f64, b: f64) -> f64 {
    clamp((t - a) / (b - a))
}

/// RGBA, straight alpha, f32 per channel (0..255 for colour, 0..1 for alpha)
struct Layer {
    w: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

impl Layer {
    fn new(w: usize, h: usize) -> Layer {
        Layer { w, h, px: vec![[0.0; 4]; w * h] }
    }
    /// `src` over the pixel
    fn over(&mut self, x: i64, y: i64, rgb: [f32; 3], a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || a <= 0.0 {
            return;
        }
        let d = &mut self.px[y as usize * self.w + x as usize];
        let out_a = a + d[3] * (1.0 - a);
        if out_a <= 0.0 {
            return;
        }
        for i in 0..3 {
            d[i] = (rgb[i] * a + d[i] * d[3] * (1.0 - a)) / out_a;
        }
        d[3] = out_a;
    }
    fn blend(&mut self, other: &Layer, dx: i64, dy: i64) {
        for y in 0..other.h {
            for x in 0..other.w {
                let s = other.px[y * other.w + x];
                self.over(x as i64 + dx, y as i64 + dy, [s[0], s[1], s[2]], s[3]);
            }
        }
    }
}

struct Fonts<'a> {
    title: ab_glyph::PxScale,
    small: ab_glyph::PxScale,
    oswald: FontRef<'a>,
    inter: FontRef<'a>,
}

/// pixel scale for a font "size" the way image libraries mean it: the em is that many pixels tall
fn em_scale(f: &FontRef, size: f32) -> PxScale {
    let upem = f.units_per_em().unwrap_or(1000.0);
    PxScale::from(size * f.height_unscaled() / upem)
}

fn text_len(font: &FontRef, scale: PxScale, text: &str) -> f32 {
    let sf = font.as_scaled(scale);
    let mut x = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        x += sf.h_advance(id);
        prev = Some(id);
    }
    x
}

/// draw text with its top (the ascender line) at y, starting at x
fn draw_text(layer: &mut Layer, font: &FontRef, scale: PxScale, x: f32, y: f32, text: &str, rgb: [f32; 3], alpha: f32) {
    let sf = font.as_scaled(scale);
    let base = y + sf.ascent();
    let mut cx = x;
    let mut prev = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            cx += sf.kern(p, id);
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(cx, base));
        if let Some(o) = font.outline_glyph(glyph) {
            let b = o.px_bounds();
            o.draw(|gx, gy, c| {
                layer.over(b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, rgb, c * alpha);
            });
        }
        cx += sf.h_advance(id);
        prev = Some(id);
    }
}

fn blur_alpha(a: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let r = (sigma * 3.0).ceil().max(1.0) as i64;
    let mut k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= sum);
    let mut tmp = vec![0.0; a.len()];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, kv) in k.iter().enumerate() {
                let xx = x as i64 + i as i64 - r;
                if xx >= 0 && (xx as usize) < w {
                    acc += a[y * w + xx as usize] * kv;
                }
            }
            tmp[y * w + x] = acc;
        }
    }
    let mut out = vec![0.0; a.len()];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, kv) in k.iter().enumerate() {
                let yy = y as i64 + i as i64 - r;
                if yy >= 0 && (yy as usize) < h {
                    acc += tmp[yy as usize * w + x] * kv;
                }
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// Draws frame t (seconds from the moment the bar appears) of the intro, for a video `height` px tall.
pub struct Painter<'a> {
    k: f64,
    fonts: Fonts<'a>,
    number: String,
    date: String,
    pub w: usize,
    pub h: usize,
}

impl<'a> Painter<'a> {
    pub fn new(number: i64, when: DateTime<Local>, height: u32) -> Painter<'a> {
        let k = SIZE * height as f64 / 1080.0;
        let oswald = FontRef::try_from_slice(OSWALD).expect("Oswald");
        let inter = FontRef::try_from_slice(INTER).expect("Inter");
        let title = em_scale(&oswald, ((70.0 * k).round() as f32).max(8.0));
        let small = em_scale(&inter, ((30.0 * k).round() as f32).max(6.0));
        let number = format!("#{number}");
        let date = format!("{} · {}", when.format("%d.%m.%Y"), when.format("%H:%M"));
        let w = (600.0 * k).round() as usize + text_len(&oswald, title, &number).round() as usize;
        let h = (170.0 * k).round() as usize;
        Painter { k, fonts: Fonts { title, small, oswald, inter }, number, date, w, h }
    }

    /// (bar height, text x, date fade) at time t, rounded to what can be seen
    pub fn state(&self, t: f64) -> (f64, f64, f64) {
        let z = self.k;
        let l = LENGTH;
        let sp = LENGTH / 3.6; // the timings below were designed for a 3.6 s version
        let grow = ease_out(phase(t, 0.0, 0.35 * sp)) * (1.0 - ease_in(phase(t, l - 0.25 * sp, l)));
        let slide = ease_out(phase(t, 0.15 * sp, 0.65 * sp)) * (1.0 - ease_in(phase(t, l - 0.55 * sp, l)));
        let k = ease_out(phase(t, 0.35 * sp, 0.8 * sp)) * (1.0 - ease_in(phase(t, l - 0.7 * sp, l - 0.3 * sp)));
        let r1 = |v: f64| (v * 10.0).round() / 10.0;
        (r1(118.0 * z * grow), r1(24.0 * z - (1.0 - slide) * 420.0 * z), (k * 1000.0).round() / 1000.0)
    }

    pub fn draw(&self, bar_h: f64, x: f64, k: f64) -> Vec<u8> {
        let z = self.k;
        let (w, h) = (self.w, self.h);
        let mut img = Layer::new(w, h);
        if bar_h > 1.0 {
            // the bar: a rounded rectangle, 6 px wide at design size
            let (x1, y0) = ((6.0 * z).max(2.0), 10.0 * z);
            let y1 = y0 + bar_h;
            let r = (3.0 * z).max(1.0);
            for py in 0..h {
                for px in 0..w.min(x1.ceil() as usize + 2) {
                    let (cx, cy) = (px as f64 + 0.5, py as f64 + 0.5);
                    // distance from the rounded rectangle (negative inside)
                    let (hx, hy) = ((x1) / 2.0, (y1 - y0) / 2.0);
                    let (qx, qy) = ((cx - hx).abs() - (hx - r), (cy - (y0 + hy)).abs() - (hy - r));
                    let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                    img.over(px as i64, py as i64, CREAM, (0.5 - d).clamp(0.0, 1.0) as f32);
                }
            }
        }
        let mut text = Layer::new(w, h);
        let f = &self.fonts;
        draw_text(&mut text, &f.oswald, f.title, x as f32, 0.0, "REPLAY", CREAM, 1.0);
        let nx = x as f32 + text_len(&f.oswald, f.title, "REPLAY ");
        draw_text(&mut text, &f.oswald, f.title, nx, 0.0, &self.number, RED, 1.0);
        draw_text(&mut text, &f.inter, f.small, (26.0 * z) as f32, ((92.0 + (1.0 - k) * 12.0) * z) as f32, &self.date, CREAM, (230.0 / 255.0 * k) as f32);
        // the text only exists to the right of the bar: that's what makes it slide out from behind it
        let cut = (14.0 * z).round() as usize;
        for y in 0..h {
            for x in 0..cut.min(w) {
                text.px[y * w + x][3] = 0.0;
            }
        }
        img.blend(&text, 0, 0);
        // a soft shadow so it reads on bright footage too
        let alpha: Vec<f32> = img.px.iter().map(|p| p[3]).collect();
        let blurred = blur_alpha(&alpha, w, h, ((5.0 * z / SIZE) as f32).max(1.0));
        let mut res = Layer::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let a = blurred[y * w + x] * (150.0 / 255.0);
                res.over(x as i64 + 2, y as i64 + 2, [0.0, 0.0, 0.0], a);
            }
        }
        res.blend(&img, 0, 0);
        let mut out = Vec::with_capacity(w * h * 4);
        for p in &res.px {
            out.extend_from_slice(&[
                p[0].round().clamp(0.0, 255.0) as u8,
                p[1].round().clamp(0.0, 255.0) as u8,
                p[2].round().clamp(0.0, 255.0) as u8,
                (p[3] * 255.0).round().clamp(0.0, 255.0) as u8,
            ]);
        }
        out
    }

}

/// Write the animation as a transparent clip (qtrle .mov). Returns its size in pixels.
pub fn render(path: &Path, number: i64, when: DateTime<Local>, height: u32, fps: u32) -> Result<(usize, usize), String> {
    let p = Painter::new(number, when, height);
    let mut child = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{}x{}", p.w, p.h)])
        .args(["-r", &fps.to_string(), "-i", "-", "-c:v", "qtrle"])
        .arg(path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let mut r = stderr;
        let _ = std::io::Read::read_to_string(&mut r, &mut s);
        s
    });
    let (mut last, mut data) = (None, Vec::new());
    for i in 0..=((LENGTH * fps as f64) as usize) {
        let st = p.state(i as f64 / fps as f64);
        if last != Some(st) {
            // the middle of the animation stands still: draw it once
            last = Some(st);
            data = p.draw(st.0, st.1, st.2);
        }
        if stdin.write_all(&data).is_err() {
            break;
        }
    }
    drop(stdin);
    let err = reader.join().unwrap_or_default();
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(err.trim().lines().last().unwrap_or("intro render failed").to_string());
    }
    Ok((p.w, p.h))
}
