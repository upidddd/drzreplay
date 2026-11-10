"""Turn the ring buffer into a finished clip (with the intro on top when it is on)."""
import fcntl
import glob
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime

from ..core import config as C
from ..core import system
from . import intro


def notify(cfg, body, icon=None, urgency="normal", replace=None, actions=None, wait=False):
    if not cfg["notifications"] and urgency != "critical":
        return None
    cmd = ["notify-send", "-a", C.APP_NAME, "-i", icon or C.ICON_PATH, "-u", urgency, "-p"]
    if replace:
        cmd += ["-r", str(replace)]
    for key, label in (actions or []):
        cmd += ["-A", f"{key}={label}"]
    if wait:
        cmd += ["-w", "-t", "10000"]
    cmd += [C.APP_NAME, body]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=30 if wait else 5).stdout.split()
    except Exception:
        return None
    return out


def probe(path):
    r = system.run(["ffprobe", "-v", "error", "-show_entries",
                    "format=duration:stream=codec_type,width,height", "-of", "json", path])
    try:
        j = json.loads(r.stdout)
        streams = j.get("streams", [])
        v = next((s for s in streams if s["codec_type"] == "video"), {})
        return (float(j["format"]["duration"]), {s["codec_type"] for s in streams},
                (v.get("width"), v.get("height")))
    except Exception:
        return 0.0, set(), (None, None)


def keyframe_before(path, offset):
    """For a concat `inpoint`: the keyframe at or before `offset` seconds into the file, in the file's own
    timestamps (mpegts chunks don't start at 0), and how many seconds before `offset` it is."""
    r = system.run(["ffprobe", "-v", "error", "-select_streams", "v", "-skip_frame", "nokey",
                    "-show_entries", "frame=pts_time", "-of", "csv=p=0", path])
    keys = []
    for line in (r.stdout.split() if r else []):
        try:
            keys.append(float(line.strip(",")))
        except ValueError:
            pass
    if not keys:
        return None, 0.0
    target = keys[0] + offset
    key = max((k for k in keys if k <= target + 0.02), default=keys[0])
    return key, max(0.0, target - key)


def cleanup(cfg, keep):
    """Delete the oldest clips beyond the limits. Only touches files DrzReplay made."""
    d = C.output_dir(cfg)
    prefix = cfg["file_prefix"] or C.APP_NAME
    clips = sorted((p for p in glob.glob(os.path.join(d, f"{glob.escape(prefix)} *.*"))
                    if p.endswith((".mp4", ".mkv")) and os.path.isfile(p)), key=os.path.getmtime)
    clips = [p for p in clips if os.path.abspath(p) != os.path.abspath(keep)]
    removed = 0
    max_n = int(cfg["keep_max_clips"] or 0)
    if max_n > 0:
        while len(clips) + 1 > max_n and clips:
            os.remove(clips.pop(0))
            removed += 1
    max_b = float(cfg["keep_max_gb"] or 0) * 1e9
    if max_b > 0:
        total = sum(os.path.getsize(p) for p in clips) + os.path.getsize(keep)
        while total > max_b and clips:
            p = clips.pop(0)
            total -= os.path.getsize(p)
            os.remove(p)
            removed += 1
    return removed


def show_in_folder(path):
    uri = "file://" + path
    r = system.run(["gdbus", "call", "--session", "--dest", "org.freedesktop.FileManager1",
                    "--object-path", "/org/freedesktop/FileManager1",
                    "--method", "org.freedesktop.FileManager1.ShowItems", f"['{uri}']", ""])
    if not r or r.returncode != 0:
        subprocess.Popen(["xdg-open", os.path.dirname(path)], start_new_session=True)


def export_size(cfg, native):
    nw, nh = native
    eh = int(cfg["export_height"] or 0)
    if eh and eh < nh:
        return int(round(nw * eh / nh / 2)) * 2, eh
    return nw, nh


def build_cmd(cfg, enc, hwdec, info, listfile, start, length, has_audio, native, out_path, overlay=None):
    """Re-encode: exact cut, resize, intro. `overlay` is (clip, x, y) of the rendered intro or None."""
    w, h = export_size(cfg, native)
    fps = int(cfg["fps"])

    cmd = ["nice", "-n", "15", "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y"]
    if shutil.which("ionice"):
        cmd = ["ionice", "-c3"] + cmd
    if enc == "vaapi":
        cmd += ["-init_hw_device", f"vaapi=va:{info['vaapi_device']}", "-filter_hw_device", "va"]
    if hwdec:
        cmd += ["-hwaccel", "cuda"]
    # no input seeking: concat + -ss lands on chunk boundaries and loses seconds, trim is exact
    cmd += ["-f", "concat", "-safe", "0", "-i", listfile]
    vcut = f"trim=start={start:.3f}:duration={length:.3f},setpts=PTS-STARTPTS"
    acut = f"atrim=start={start:.3f}:duration={length:.3f},asetpts=PTS-STARTPTS"

    fc = []
    vout = f"[0:v]{vcut},fps={fps},scale={w}:{h},format=yuv420p"
    if overlay:
        clip, x, y = overlay
        cmd += ["-i", clip]
        fc.append(f"{vout}[bg]")
        # shifted to its start; before that the overlay has no frame and the replay shows as is
        fc.append(f"[1:v]setpts=PTS-STARTPTS+{intro.START}/TB[ov]")
        vout = f"[bg][ov]overlay={x}:{y}:eof_action=pass,format=yuv420p"
    if has_audio:
        fc.append(f"[0:a]{acut},aresample=48000[a]")
    if enc == "vaapi":
        vout += ",format=nv12,hwupload"
    fc.append(vout + "[v]")

    q = C.EXPORT_QUALITY.get(cfg["export_quality"], 19)
    cmd += ["-filter_complex", ";".join(fc), "-map", "[v]"]
    if has_audio:
        cmd += ["-map", "[a]", "-c:a", "aac", "-b:a", "192k"]
    cmd += ["-t", f"{length:.3f}"]
    if enc == "nvenc":
        cmd += ["-c:v", "h264_nvenc", "-preset", "p5", "-rc", "vbr", "-cq", str(q), "-b:v", "0",
                "-maxrate", "60M", "-bufsize", "60M", "-profile:v", "high", "-pix_fmt", "yuv420p"]
    elif enc == "vaapi":
        cmd += ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", str(q)]
    else:
        cmd += ["-c:v", "libx264", "-preset", "veryfast", "-crf", str(q), "-pix_fmt", "yuv420p", "-threads", "2"]
    if out_path.endswith(".mp4"):
        cmd += ["-movflags", "+faststart"]
    cmd.append(out_path)
    return cmd


def copy_cmd(listfile, length, out_path):
    """No re-encode: the recording is copied as is, starting at the keyframe (one a second) before the cut.
    Instant and with no load on the graphics card while a game runs."""
    cmd = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
           "-f", "concat", "-safe", "0", "-i", listfile, "-map", "0:v", "-map", "0:a?", "-c", "copy",
           "-t", f"{length:.3f}"]
    if out_path.endswith(".mp4"):
        cmd += ["-movflags", "+faststart"]
    return cmd + [out_path]


MIN_SINCE_LAST = 1.0     # a quick second press still gets a moment, not an empty clip


def since_last_save(cfg, pressed):
    """Seconds recorded since the previous replay ended, or None when it doesn't apply
    (option off, first replay, or the recorder restarted since — then its buffer is new anyway)."""
    if not cfg.get("replay_since_last"):
        return None
    last = C.read_json(C.LAST_SAVE, {}) or {}
    session = (C.read_json(C.STATUSFILE, {}) or {}).get("started")
    if not last or not session or last.get("session") != session:
        return None
    return max(MIN_SINCE_LAST, pressed - float(last.get("end", 0)))


def main(args):
    pressed = time.time()           # before waiting for an earlier save: this is when the key was hit
    cfg = C.load()
    os.makedirs(C.RUNTIME, exist_ok=True)
    lock = open(os.path.join(C.RUNTIME, "save.lock"), "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        # pressed again while a clip is being written: save another one right after it
        notify(cfg, "Saving after the current clip…")
        fcntl.flock(lock, fcntl.LOCK_EX)

    if cfg["click_sound"] and shutil.which("paplay") and os.path.exists(C.CLICK_SOUND):
        subprocess.Popen(["paplay", "--volume=30000", C.CLICK_SOUND], stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)

    if C.daemon_pid() and (C.read_json(C.STATUSFILE, {}) or {}).get("replays_off"):
        # paused, or capturing only for a full recording
        notify(cfg, "Replays are off. Turn them on in the window or the tray.", "dialog-error", "critical")
        return 1
    segs = sorted(glob.glob(os.path.join(C.RUNTIME, "seg_*.ts")), key=os.path.getmtime)
    if segs and sum(os.path.getsize(s) for s in segs) < 200_000:
        time.sleep(1.5)        # recorder just started: give it a moment to write something
        segs = sorted(glob.glob(os.path.join(C.RUNTIME, "seg_*.ts")), key=os.path.getmtime)
    if not segs:
        paused = C.daemon_pid() and C.read_json(C.STATUSFILE, {}).get("state") == "paused"
        notify(cfg, "Recording is paused." if paused else "Recording is off.", "dialog-error", "critical")
        return 1
    want = float(cfg["seconds"])
    if "--seconds" in args:            # "save last 15 s" from the tray / settings
        want = min(want, float(args[args.index("--seconds") + 1]))
    else:
        since = since_last_save(cfg, pressed)
        if since is not None:          # 15 s after the last replay: save those 15 s, not the whole buffer again
            want = min(want, since)

    # copy the needed chunks (newest first) so the recorder cannot overwrite them mid-export
    snap = os.path.join(C.RUNTIME, "snap")
    shutil.rmtree(snap, ignore_errors=True)
    os.makedirs(snap)
    picked, total, kinds, native = [], 0.0, set(), None
    for i, s in enumerate(reversed(segs)):
        dst = os.path.join(snap, f"{len(segs) - i:04d}.ts")
        try:
            shutil.copyfile(s, dst)
        except FileNotFoundError:
            continue
        d, k, size = probe(dst)
        if d <= 0.05 or not size[0]:
            continue
        if native and size != native:      # resolution changed mid-buffer: stop here
            break
        native = size
        picked.append((dst, d))
        kinds |= k
        total += d
        if total >= want + 0.5:
            break
    picked.reverse()
    if not picked:
        notify(cfg, "Nothing recorded yet.", "dialog-error")
        return 1
    listfile = os.path.join(snap, "list.txt")
    with open(listfile, "w") as f:
        f.writelines(f"file '{p}'\n" for p, _ in picked)
    start, length = max(0.0, total - want), min(want, total)
    # the same cut for a plain copy: skip whole chunks before it, start inside the first one
    # it starts on the keyframe before the cut, so it's up to a second longer
    copylist, copy_len = os.path.join(snap, "copy.txt"), length
    with open(copylist, "w") as f:
        skip = start
        for p, d in picked:
            if skip >= d:
                skip -= d
                continue
            f.write(f"file '{p}'\n")
            if skip > 0.05:
                key, extra = keyframe_before(p, skip)
                if key is not None:
                    f.write(f"inpoint {key:.3f}\n")
                    copy_len += extra
            skip = 0
    # the buffer counts as empty from the press on (the window's buffer bar drops to 0 right away);
    # put the old mark back if this save fails
    prev_mark = C.read_json(C.LAST_SAVE, None)
    C.write_json(C.LAST_SAVE, {"end": pressed, "session": (C.read_json(C.STATUSFILE, {}) or {}).get("started")})
    nid = notify(cfg, "Saving…")
    nid = nid[0] if nid else None

    outdir = C.output_dir(cfg)
    os.makedirs(outdir, exist_ok=True)
    ext = "mkv" if cfg["container"] == "mkv" else "mp4"
    name = f"{cfg['file_prefix'] or C.APP_NAME} {datetime.now():%Y-%m-%d %H-%M-%S}.{ext}"
    out = os.path.join(outdir, name)
    tmp = os.path.join(outdir, f".{name}.part.{ext}")

    number, overlay = intro.next_number(cfg), None      # every replay counts, intro or not
    if cfg["intro"] and intro.available():
        try:
            clip = os.path.join(snap, "intro.mov")
            intro.render(clip, number, datetime.fromtimestamp(pressed), export_size(cfg, native)[1], int(cfg["fps"]))
            overlay = (clip, *intro.position(export_size(cfg, native)[1]))
        except Exception as e:             # a broken intro must not cost the replay
            print(f"intro: {e}", file=sys.stderr)

    info = system.detect_encoders()
    attempts = []
    if not overlay and export_size(cfg, native) == native:
        attempts.append(copy_cmd(copylist, copy_len, tmp))
    for e in system.encoder_order(cfg, info):
        if e == "nvenc":
            attempts.append(build_cmd(cfg, e, True, info, listfile, start, length, "audio" in kinds, native, tmp,
                                      overlay))
        attempts.append(build_cmd(cfg, e, False, info, listfile, start, length, "audio" in kinds, native, tmp,
                                  overlay))
    err = "unknown error"
    for cmd in attempts:
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode == 0 and os.path.exists(tmp) and os.path.getsize(tmp) > 1000:
            break
        err = (r.stderr.strip().splitlines() or ["unknown error"])[-1]
    else:
        shutil.rmtree(snap, ignore_errors=True)
        if os.path.exists(tmp):
            os.remove(tmp)
        if prev_mark is None:
            try:
                os.remove(C.LAST_SAVE)
            except OSError:
                pass
        else:
            C.write_json(C.LAST_SAVE, prev_mark)
        notify(cfg, f"Save failed: {err}", "dialog-error", "critical", nid)
        return 1
    os.replace(tmp, out)
    intro.commit(number)
    shutil.rmtree(snap, ignore_errors=True)
    cleanup(cfg, out)
    fcntl.flock(lock, fcntl.LOCK_UN)
    print(out)

    res = notify(cfg, f"Saved: {name}", replace=nid,
                 actions=[("play", "Play"), ("folder", "Show in folder")], wait=True)
    if res and "play" in res:
        subprocess.Popen(["xdg-open", out], start_new_session=True)
    elif res and "folder" in res:
        show_in_folder(out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
