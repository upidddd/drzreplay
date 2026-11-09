"""Clips tab: cut a piece out of a saved clip (frame-exact, re-encoded like a normal save) and rename clips."""
import os
import re
import shutil
import subprocess

from ..core import config as C
from ..core import system

# names the app gives clips itself: "<prefix> 2026-11-09 14-03-27", maybe " (trimmed)" / " (trimmed 2)"
AUTO_NAME = re.compile(r"\d{4}-\d{2}-\d{2} \d{2}-\d{2}-\d{2}(?: \(trimmed(?: \d+)?\))?$")


def is_auto_name(path):
    return bool(AUTO_NAME.search(os.path.splitext(os.path.basename(path))[0]))


def rename(path, name):
    """Give the clip a new name (the extension stays). Returns the new path; ValueError says what's wrong."""
    name = name.strip()
    ext = os.path.splitext(path)[1]
    if name.lower().endswith(ext.lower()):
        name = name[:-len(ext)].rstrip()
    if not name or name in (".", ".."):
        raise ValueError("The name is empty")
    if "/" in name or "\0" in name:
        raise ValueError("A name can't contain /")
    if name.startswith("."):
        raise ValueError("A name starting with a dot would hide the clip")
    new = os.path.join(os.path.dirname(path), name + ext)
    if new == path:
        return new
    if os.path.exists(new):
        raise ValueError("A clip with this name already exists")
    try:
        os.rename(path, new)
    except OSError as e:
        raise ValueError(e.strerror or "Couldn't rename") from None
    return new


def output_path(src):
    base, ext = os.path.splitext(src)
    path = f"{base} (trimmed){ext}"
    n = 2
    while os.path.exists(path):
        path = f"{base} (trimmed {n}){ext}"
        n += 1
    return path


def build_cmd(cfg, enc, info, src, start, end, out):
    q = C.EXPORT_QUALITY.get(cfg["export_quality"], 19)
    cmd = ["nice", "-n", "15", "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y"]
    if shutil.which("ionice"):
        cmd = ["ionice", "-c3"] + cmd
    if enc == "vaapi":
        cmd += ["-init_hw_device", f"vaapi=va:{info['vaapi_device']}", "-filter_hw_device", "va"]
    # -ss before -i with re-encoding is exact in ffmpeg: it decodes from the keyframe and drops up to `start`
    cmd += ["-ss", f"{start:.3f}", "-i", src, "-t", f"{end - start:.3f}", "-map", "0:v:0", "-map", "0:a?"]
    vf = "format=yuv420p"
    if enc == "vaapi":
        vf = "format=nv12,hwupload"
    cmd += ["-vf", vf, "-c:a", "aac", "-b:a", "192k"]
    if enc == "nvenc":
        cmd += ["-c:v", "h264_nvenc", "-preset", "p5", "-rc", "vbr", "-cq", str(q), "-b:v", "0", "-profile:v", "high"]
    elif enc == "vaapi":
        cmd += ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", str(q)]
    else:
        cmd += ["-c:v", "libx264", "-preset", "veryfast", "-crf", str(q), "-threads", "2"]
    if out.endswith(".mp4"):
        cmd += ["-movflags", "+faststart"]
    return cmd + [out]


def trim(src, start, end, delete_original=False):
    """Returns the new file's path; raises RuntimeError when every encoder failed."""
    cfg = C.load()
    info = system.detect_encoders()
    out = output_path(src)
    tmp = os.path.join(os.path.dirname(out), "." + os.path.basename(out))
    err = ""
    for enc in system.encoder_order(cfg, info):
        r = subprocess.run(build_cmd(cfg, enc, info, src, start, end, tmp), capture_output=True, text=True)
        if r.returncode == 0 and os.path.getsize(tmp) > 0:
            os.replace(tmp, out)
            if delete_original:
                os.remove(src)
            return out
        err = (r.stderr.strip().splitlines() or ["ffmpeg failed"])[-1]
    try:
        os.remove(tmp)
    except OSError:
        pass
    raise RuntimeError(err)
