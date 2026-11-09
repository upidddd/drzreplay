"""Settings, paths and constants shared by every DrzReplay process."""
import json
import os
import shutil
import subprocess
import sys

APP_NAME = "DrzReplay"
APP_ID = "com.drz.DrzReplay"
VERSION = "1.5.1"
REPO = "upidddd/drzreplay"
INSTALL_CMD = f"curl -fsSL https://github.com/{REPO}/releases/latest/download/install.sh | sh"

# the directory that holds the drzreplay package (goes on PYTHONPATH for child processes)
PKG_DIR = os.path.dirname(os.path.dirname(os.path.dirname(os.path.realpath(__file__))))
ASSETS = os.path.join(PKG_DIR, "drzreplay", "assets")
ICON_PATH = os.path.join(ASSETS, "icons", "drzreplay.svg")
CLICK_SOUND = os.path.join(ASSETS, "sounds", "click.wav")

CONF_DIR = os.path.join(os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config"), "drzreplay")
CONF_PATH = os.path.join(CONF_DIR, "config.json")
CACHE_DIR = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "drzreplay")
UPDATE_CACHE = os.path.join(CACHE_DIR, "update.json")   # {"time", "latest", "notified", "notified_at"}
RUNTIME = os.environ.get("DRZREPLAY_RUNTIME") or f"/dev/shm/drzreplay-{os.getuid()}"
PIDFILE = os.path.join(RUNTIME, "daemon.pid")
STATUSFILE = os.path.join(RUNTIME, "status.json")
ENCODERS_CACHE = os.path.join(RUNTIME, "encoders.json")
FFMPEG_LOG = os.path.join(RUNTIME, "ffmpeg.log")
HOTKEY_LOG = os.path.join(RUNTIME, "hotkey.log")
LAST_SAVE = os.path.join(RUNTIME, "last_save.json")    # {"end": time, "session": recorder start time}
REQUEST = os.path.join(RUNTIME, "request")              # a command for the daemon, read on SIGHUP

SEGMENT = 5  # seconds per ring-buffer chunk

# CQ / CRF / QP per preset — lower is better quality, bigger files
REC_QUALITY = {"low": 28, "balanced": 23, "high": 19, "ultra": 16}
EXPORT_QUALITY = {"low": 26, "balanced": 22, "high": 19, "ultra": 16}
# rough recording bitrate at 1080p30 per preset, Mbit/s — for the RAM estimate in the UI
REC_MBPS = {"low": 5, "balanced": 10, "high": 18, "ultra": 30}


def default_video_dir():
    try:
        d = subprocess.run(["xdg-user-dir", "VIDEOS"], capture_output=True, text=True, timeout=3).stdout.strip()
        if d and d != os.path.expanduser("~"):
            return os.path.join(d, APP_NAME)
    except Exception:
        pass
    return os.path.join(os.path.expanduser("~/Videos"), APP_NAME)


DEFAULTS = {
    # capture
    "seconds": 60,
    "fps": 30,
    "monitor": "all",
    "draw_mouse": True,
    "encoder": "auto",            # auto | nvenc | vaapi | x264
    "rec_quality": "balanced",
    # audio
    "system_audio": True,
    "system_device": "@DEFAULT_MONITOR@",
    "system_volume": 100,
    "mic": False,
    "mic_device": "@DEFAULT_SOURCE@",
    "mic_volume": 100,
    # saving
    "output_dir": "",
    "file_prefix": "DrzReplay",
    "container": "mp4",
    "export_height": 0,           # 0 = native
    "export_quality": "high",
    "keep_max_clips": 0,          # 0 = unlimited
    "keep_max_gb": 0,
    # controls
    "hotkey": "<Alt>z",
    "click_sound": True,
    "notifications": True,
    "tray": True,
    # intro
    "intro": False,                 # replay number and date in the corner for the first seconds
    "replay_since_last": True,  # a replay doesn't repeat what the previous one already saved
    "record_hotkey": "<Alt>r",  # start / stop recording a full video
}

# changing any of these needs the recorder (ffmpeg) to restart
RECORDER_KEYS = {"seconds", "fps", "monitor", "draw_mouse", "encoder", "rec_quality", "system_audio",
                 "system_device", "system_volume", "mic", "mic_device", "mic_volume"}


def exists():
    return os.path.exists(CONF_PATH)


def load():
    cfg = dict(DEFAULTS)
    try:
        with open(CONF_PATH) as f:
            data = json.load(f)
        cfg.update({k: v for k, v in data.items() if k in DEFAULTS})
    except FileNotFoundError:
        pass
    except Exception as e:
        print(f"{APP_NAME}: config is broken ({e}), using defaults", file=sys.stderr)
    if not cfg["output_dir"]:
        cfg["output_dir"] = default_video_dir()
    return cfg


def save(cfg):
    os.makedirs(CONF_DIR, exist_ok=True)
    tmp = CONF_PATH + ".tmp"
    with open(tmp, "w") as f:
        json.dump({k: cfg[k] for k in DEFAULTS}, f, indent=2, ensure_ascii=False)
    os.replace(tmp, CONF_PATH)


def output_dir(cfg):
    return os.path.expanduser(cfg["output_dir"])


def launcher():
    """Command that starts this program, installed or run from source."""
    exe = shutil.which("drzreplay")
    if exe and os.path.realpath(os.path.dirname(exe)) != os.path.realpath(PKG_DIR):
        return [exe]
    return ["env", f"PYTHONPATH={PKG_DIR}", sys.executable, "-m", "drzreplay"]


def child_env():
    env = dict(os.environ)
    env["PYTHONPATH"] = PKG_DIR + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    return env


def read_json(path, default=None):
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:
        return default


def write_json(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f)
    os.replace(tmp, path)


def daemon_pid():
    try:
        pid = int(open(PIDFILE).read().strip())
        os.kill(pid, 0)
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            if b"drzreplay" not in f.read():
                return None
        return pid
    except Exception:
        return None
