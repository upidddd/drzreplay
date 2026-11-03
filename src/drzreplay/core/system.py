"""Probing the machine: session type, monitors, encoders, audio devices."""
import glob
import json
import os
import re
import subprocess
import time

from . import config as C


def run(cmd, timeout=10):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except Exception:
        return None


def is_wayland():
    return os.environ.get("XDG_SESSION_TYPE", "").lower() == "wayland" or (
        bool(os.environ.get("WAYLAND_DISPLAY")) and not os.environ.get("DISPLAY"))


def screen_size():
    r = run(["xrandr", "--current"])
    if r and r.returncode == 0:
        m = re.search(r"current (\d+) x (\d+)", r.stdout)
        if m:
            return int(m.group(1)), int(m.group(2))
    return 1920, 1080


def monitors():
    """[{name, w, h, x, y, primary}] for every active output."""
    out = []
    r = run(["xrandr", "--listactivemonitors"])
    if r and r.returncode == 0:
        for line in r.stdout.splitlines()[1:]:
            m = re.search(r"([+*]*)(\S+)\s+(\d+)/\d+x(\d+)/\d+\+(\d+)\+(\d+)", line)
            if m:
                out.append(dict(name=m.group(2), primary="*" in m.group(1), w=int(m.group(3)),
                                h=int(m.group(4)), x=int(m.group(5)), y=int(m.group(6))))
    return out


def capture_geometry(cfg):
    """(w, h, x, y) to grab; even sizes so every encoder is happy."""
    w, h, x, y = *screen_size(), 0, 0
    if cfg.get("monitor") not in (None, "", "all"):
        for m in monitors():
            if m["name"] == cfg["monitor"]:
                w, h, x, y = m["w"], m["h"], m["x"], m["y"]
                break
    return w - w % 2, h - h % 2, x, y


def _try(cmd):
    r = run(["ffmpeg", "-hide_banner", "-v", "error"] + cmd + ["-f", "null", "-"], timeout=15)
    return bool(r) and r.returncode == 0


def detect_encoders(force=False):
    """{"available": [...], "vaapi_device": path} — cached per boot in /dev/shm."""
    if not force:
        cached = C.read_json(C.ENCODERS_CACHE)
        if cached and time.time() - cached.get("time", 0) < 24 * 3600:
            return cached
    src = ["-f", "lavfi", "-i", "color=black:s=640x360:r=30", "-frames:v", "3"]
    avail = []
    if _try(src + ["-c:v", "h264_nvenc"]):
        avail.append("nvenc")
    vdev = None
    for dev in sorted(glob.glob("/dev/dri/renderD*")):
        if _try(["-vaapi_device", dev] + src + ["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi"]):
            avail.append("vaapi")
            vdev = dev
            break
    avail.append("x264")
    res = {"available": avail, "vaapi_device": vdev, "time": time.time()}
    try:
        C.write_json(C.ENCODERS_CACHE, res)
    except OSError:
        pass
    return res


def encoder_order(cfg, info):
    avail = info["available"]
    pref = cfg.get("encoder", "auto")
    order = [e for e in ("nvenc", "vaapi", "x264") if e in avail]
    if pref in avail:
        order.remove(pref)
        order.insert(0, pref)
    return order


ENCODER_LABELS = {
    "auto": "Automatic",
    "nvenc": "NVIDIA NVENC",
    "vaapi": "AMD / Intel (VA-API)",
    "x264": "CPU (x264)",
}


def _pactl_json(kind):
    r = run(["pactl", "--format=json", "list", kind])
    if r and r.returncode == 0:
        try:
            return json.loads(r.stdout)
        except ValueError:
            pass
    r = run(["pactl", "list", "short", kind])
    items = []
    if r and r.returncode == 0:
        for line in r.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) > 1:
                items.append({"name": parts[1], "description": parts[1]})
    return items


def audio_outputs():
    """[(pulse source name of the monitor, label)] — what you hear."""
    return [(s["name"] + ".monitor", s.get("description") or s["name"]) for s in _pactl_json("sinks")]


def audio_inputs():
    """[(source name, label)] — microphones, monitors excluded."""
    return [(s["name"], s.get("description") or s["name"]) for s in _pactl_json("sources")
            if not s["name"].endswith(".monitor") and s.get("monitor_of_sink") in (None, "", "n/a")]


def pulse_defaults():
    """(default sink, default source) of the PulseAudio / PipeWire server, or (None, None)."""
    r = run(["pactl", "info"])
    sink = source = None
    if r and r.returncode == 0:
        for line in r.stdout.splitlines():
            if line.startswith("Default Sink:"):
                sink = line.split(":", 1)[1].strip()
            elif line.startswith("Default Source:"):
                source = line.split(":", 1)[1].strip()
    return sink, source


def pulse_streams(pids):
    """[(source-output index, source name)] of the recording streams opened by these processes."""
    names = {s.get("index"): s.get("name") for s in _pactl_json("sources")}
    out = []
    for so in _pactl_json("source-outputs"):
        pid = (so.get("properties") or {}).get("application.process.id")
        if pid and pid.isdigit() and int(pid) in pids:
            src = so.get("source")
            out.append((so.get("index"), names.get(src, src)))
    return out


def installed():
    """True when running from the .deb (and so can be updated by the installer)."""
    return os.path.realpath(C.PKG_DIR) == "/usr/lib/drzreplay"


def latest_version(timeout=6):
    """Newest release tag on GitHub, e.g. "1.2.0", or None when offline."""
    import urllib.request
    try:
        req = urllib.request.Request(f"https://github.com/{C.REPO}/releases/latest", method="HEAD",
                                     headers={"User-Agent": C.APP_NAME})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            tag = r.geturl().rstrip("/").rsplit("/", 1)[-1]
    except Exception:
        return None
    return tag.lstrip("v") if re.fullmatch(r"v?\d+(\.\d+)*", tag) else None


def version_tuple(v):
    return tuple(int(p) for p in re.findall(r"\d+", v or "0"))


UPDATER = "/usr/lib/drzreplay/drzreplay-update"


def self_update():
    """Install the newest release without a password, through the root helper the package ships
    (allowed by /etc/sudoers.d/drzreplay). Returns (ok, message); ok is None when the helper
    can't be used here (older install, no sudo) and the terminal installer is needed instead."""
    import shutil
    if not (os.path.exists(UPDATER) and shutil.which("sudo")):
        return None, "no updater"
    try:
        r = subprocess.run(["sudo", "-n", UPDATER], capture_output=True, text=True, timeout=900)
    except Exception as e:
        return False, str(e)
    out = (r.stdout + r.stderr).strip().splitlines()
    if r.returncode != 0 and any("password" in l.lower() for l in out):
        return None, "sudo wants a password"
    return r.returncode == 0, out[-1] if out else ""


def run_in_terminal(command):
    """Open a terminal window running a shell command (for the updater's sudo prompt)."""
    import shutil
    sh = ["sh", "-c", command + '; echo; printf "Press Enter to close… "; read x']
    for term, flag in (("x-terminal-emulator", "-e"), ("i3-sensible-terminal", "-e"), ("xfce4-terminal", "-x"),
                       ("gnome-terminal", "--"), ("konsole", "-e"), ("kitty", None), ("alacritty", "-e"),
                       ("xterm", "-e")):
        if shutil.which(term):
            subprocess.Popen([term] + ([flag] if flag else []) + sh, start_new_session=True)
            return True
    return False
