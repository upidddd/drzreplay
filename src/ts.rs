//! A small MPEG-TS reader: duration, frame size, sound and keyframes of a chunk, straight from the
//! packets. Saving used to start ffprobe for every 5-second chunk; this reads the same file in a
//! few milliseconds.
use std::fs;
use std::path::Path;

const TS: usize = 188;
const PTS_WRAP: i64 = 1 << 33;
const HZ: f64 = 90000.0;

#[derive(Debug, Default, Clone)]
pub struct Analysis {
    /// seconds of video / sound in the file
    pub duration: f64,
    pub has_audio: bool,
    pub has_video: bool,
    /// width x height of the picture, when the stream header could be read
    pub size: Option<(u32, u32)>,
    /// presentation times (seconds, in the file's own clock) of the keyframes
    pub keys: Vec<f64>,
}

fn pid_of(p: &[u8]) -> usize {
    (((p[1] & 0x1f) as usize) << 8) | p[2] as usize
}

/// where the payload of a TS packet starts, or None when it has none
fn payload_off(p: &[u8]) -> Option<usize> {
    let afc = (p[3] >> 4) & 3;
    if afc & 1 == 0 {
        return None;
    }
    let off = if afc & 2 != 0 { 5 + p[4] as usize } else { 4 };
    (off < TS).then_some(off)
}

fn pes_pts(p: &[u8], off: usize) -> Option<i64> {
    if p.len() < off + 14 || p[off..off + 3] != [0, 0, 1] || p[off + 7] & 0x80 == 0 {
        return None;
    }
    let b = &p[off + 9..off + 14];
    Some(
        (((b[0] >> 1) & 7) as i64) << 30
            | (b[1] as i64) << 22
            | ((b[2] >> 1) as i64) << 15
            | (b[3] as i64) << 7
            | (b[4] >> 1) as i64,
    )
}

#[derive(Default)]
struct Stream {
    pts: Vec<i64>,
}

fn unwrap_pts(v: &mut [i64]) {
    // PTS counts in 33 bits: a chunk that crosses the wrap point continues past 2^33
    let mut add = 0;
    let mut prev = None;
    for x in v.iter_mut() {
        if let Some(p) = prev {
            if p - *x > PTS_WRAP / 2 {
                add += PTS_WRAP;
            } else if *x - p > PTS_WRAP / 2 && add > 0 {
                add -= PTS_WRAP;
            }
        }
        prev = Some(*x);
        *x += add;
    }
}

/// seconds between the frames, taken from the smallest step
fn step(sorted: &[i64]) -> f64 {
    sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 0)
        .min()
        .map(|d| d as f64 / HZ)
        .unwrap_or(0.0)
}

pub fn analyze_file(path: &Path) -> Option<Analysis> {
    analyze(&fs::read(path).ok()?)
}

pub fn analyze(data: &[u8]) -> Option<Analysis> {
    let (mut video_pid, mut audio_pid) = (None, None);
    let mut pmt_pid = None;
    let (mut video, mut audio) = (Stream::default(), Stream::default());
    let mut keys: Vec<i64> = vec![];
    let mut collecting: Option<Vec<u8>> = None; // payload of the first keyframe, to find the SPS
    let mut size = None;

    let mut start = 0;
    // find the first sync byte (a chunk should begin on one)
    while start + TS < data.len() && !(data[start] == 0x47 && data[start + TS] == 0x47) {
        start += 1;
    }
    for p in data[start..].chunks_exact(TS) {
        if p[0] != 0x47 {
            continue;
        }
        let pid = pid_of(p);
        let pusi = p[1] & 0x40 != 0;
        if pid == 0 {
            if pmt_pid.is_none() {
                if let Some(off) = payload_off(p) {
                    pmt_pid = parse_pat(&p[off..]);
                }
            }
            continue;
        }
        if Some(pid) == pmt_pid {
            if video_pid.is_none() && audio_pid.is_none() {
                if let Some(off) = payload_off(p) {
                    (video_pid, audio_pid) = parse_pmt(&p[off..]);
                }
            }
            continue;
        }
        // without a PMT (it is written at the start of a chunk, but be forgiving) ffmpeg's numbering
        if video_pid.is_none() && pmt_pid.is_none() && pid == 0x100 {
            video_pid = Some(0x100);
        }
        if audio_pid.is_none() && pmt_pid.is_none() && pid == 0x101 {
            audio_pid = Some(0x101);
        }
        let is_video = Some(pid) == video_pid;
        let is_audio = Some(pid) == audio_pid;
        if !is_video && !is_audio {
            continue;
        }
        let Some(off) = payload_off(p) else { continue };
        if is_video {
            // the SPS sits in the first bytes of a keyframe: collect a few hundred bytes of it
            if pusi {
                if let Some(b) = collecting.take() {
                    size = size.or_else(|| find_sps_size(&b));
                }
            }
            if let Some(b) = collecting.as_mut() {
                b.extend_from_slice(&p[off..]);
                if b.len() >= 600 {
                    size = size.or_else(|| find_sps_size(b));
                    collecting = None;
                }
            }
        }
        if pusi {
            if let Some(pts) = pes_pts(p, off) {
                if is_video {
                    video.pts.push(pts);
                    if p[3] & 0x20 != 0 && p[4] > 0 && p[5] & 0x40 != 0 {
                        keys.push(pts);
                        if size.is_none() {
                            collecting = Some(p[off..].to_vec());
                        }
                    }
                } else {
                    audio.pts.push(pts);
                }
            }
        }
    }
    if let Some(b) = collecting.take() {
        size = size.or_else(|| find_sps_size(&b));
    }
    if video.pts.is_empty() && audio.pts.is_empty() {
        return None;
    }
    unwrap_pts(&mut video.pts);
    unwrap_pts(&mut audio.pts);
    let mut k = keys;
    unwrap_pts(&mut k);

    let span = |s: &Stream| -> Option<(f64, f64)> {
        if s.pts.is_empty() {
            return None;
        }
        let mut v = s.pts.clone();
        v.sort_unstable();
        let first = v[0] as f64 / HZ;
        let last = *v.last().unwrap() as f64 / HZ;
        Some((first, last + step(&v)))
    };
    // the picture sets the length (sound is cut into coarser frames); sound alone only if there is no picture
    let (lo, hi) = span(&video).or_else(|| span(&audio)).unwrap_or((0.0, 0.0));
    Some(Analysis {
        duration: (hi - lo).max(0.0),
        has_audio: !audio.pts.is_empty(),
        has_video: !video.pts.is_empty(),
        size,
        keys: k.iter().map(|p| *p as f64 / HZ).collect(),
    })
}

fn parse_pat(payload: &[u8]) -> Option<usize> {
    let ptr = *payload.first()? as usize;
    let t = payload.get(1 + ptr..)?;
    if t.len() < 12 || t[0] != 0 {
        return None;
    }
    let len = (((t[1] & 0x0f) as usize) << 8 | t[2] as usize).min(t.len() - 3);
    let mut i = 8;
    while i + 4 <= len + 3 - 4 && i + 4 <= t.len() {
        let prog = (t[i] as usize) << 8 | t[i + 1] as usize;
        let pid = ((t[i + 2] & 0x1f) as usize) << 8 | t[i + 3] as usize;
        if prog != 0 {
            return Some(pid);
        }
        i += 4;
    }
    None
}

fn parse_pmt(payload: &[u8]) -> (Option<usize>, Option<usize>) {
    let (mut v, mut a) = (None, None);
    let Some(ptr) = payload.first().map(|p| *p as usize) else { return (v, a) };
    let Some(t) = payload.get(1 + ptr..) else { return (v, a) };
    if t.len() < 12 || t[0] != 2 {
        return (v, a);
    }
    let len = ((t[1] & 0x0f) as usize) << 8 | t[2] as usize;
    let end = (3 + len).min(t.len()).saturating_sub(4);
    let pinfo = ((t[10] & 0x0f) as usize) << 8 | t[11] as usize;
    let mut i = 12 + pinfo;
    while i + 5 <= end {
        let ty = t[i];
        let pid = ((t[i + 1] & 0x1f) as usize) << 8 | t[i + 2] as usize;
        let es = ((t[i + 3] & 0x0f) as usize) << 8 | t[i + 4] as usize;
        match ty {
            0x01 | 0x02 | 0x1b | 0x24 if v.is_none() => v = Some(pid),
            0x03 | 0x04 | 0x0f | 0x11 | 0x81 if a.is_none() => a = Some(pid),
            _ => {}
        }
        i += 5 + es;
    }
    (v, a)
}

// ---------------------------------------------------------------- H.264 SPS
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos >> 3)?;
        let b = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = v << 1 | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

/// size of the picture from the first SPS NAL unit in an Annex B byte stream
fn find_sps_size(buf: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0;
    while i + 4 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && buf[i + 3] & 0x1f == 7 {
            // remove emulation prevention bytes (00 00 03)
            let mut rbsp = Vec::with_capacity(64);
            let mut zeros = 0;
            for &b in &buf[i + 4..(i + 4 + 120).min(buf.len())] {
                if zeros >= 2 && b == 3 {
                    zeros = 0;
                    continue;
                }
                zeros = if b == 0 { zeros + 1 } else { 0 };
                rbsp.push(b);
            }
            return sps_size(&rbsp);
        }
        i += 1;
    }
    None
}

fn sps_size(d: &[u8]) -> Option<(u32, u32)> {
    let mut r = Bits { d, pos: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags + level
    r.ue()?; // sps id
    let mut chroma = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            for i in 0..(if chroma != 3 { 8 } else { 12 }) {
                if r.bit()? == 1 {
                    let n = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..n {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?;
    let w_mbs = r.ue()? + 1;
    let h_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?; // direct_8x8_inference
    let (mut l, mut rt, mut t, mut b) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        l = r.ue()?;
        rt = r.ue()?;
        t = r.ue()?;
        b = r.ue()?;
    }
    let (sub_w, sub_h) = match chroma {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let unit_y = sub_h * (2 - frame_mbs_only);
    let w = (w_mbs * 16).checked_sub(sub_w * (l + rt))?;
    let h = (h_units * 16 * (2 - frame_mbs_only)).checked_sub(unit_y * (t + b))?;
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sps_1080p() {
        // x264 baseline-ish High profile SPS for 1920x1080 (cropped from 1088)
        let sps = [
            0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04,
            0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
        ];
        let mut buf = vec![0, 0, 0, 1, 0x67];
        buf.extend_from_slice(&sps);
        assert_eq!(find_sps_size(&buf), Some((1920, 1080)));
    }
}
