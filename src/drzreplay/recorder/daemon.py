"""Background recorder: ring buffer of short MPEG-TS chunks in RAM + tray + hotkey.

Signals: SIGHUP = reload settings, SIGUSR1 = save a replay, SIGUSR2 = restart with the
newly installed code (sent by the package after an upgrade), SIGTERM = quit.
"""
import fcntl
import glob
import math
import os
import signal
import subprocess
import sys
import threading
import time

from ..core import config as C
from ..core import system
from .hotkey import pretty

try:
    import gi
    gi.require_version("GLib", "2.0")
    from gi.repository import GLib
except Exception:  # pragma: no cover
    GLib = None


def build_cmd(cfg, enc, geom, info):
    w, h, x, y = geom
    fps = int(cfg["fps"])
    q = C.REC_QUALITY.get(cfg["rec_quality"], 23)
    display = os.environ.get("DISPLAY", ":0")
    cmd = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error"]
    if enc == "vaapi":
        cmd += ["-vaapi_device", info["vaapi_device"]]
    cmd += ["-thread_queue_size", "32", "-f", "x11grab", "-framerate", str(fps),
            "-video_size", f"{w}x{h}", "-draw_mouse", "1" if cfg["draw_mouse"] else "0",
            "-i", f"{display}+{x},{y}"]

    audio = []
    if cfg["system_audio"]:
        audio.append((cfg["system_device"] or "@DEFAULT_MONITOR@", cfg["system_volume"]))
    if cfg["mic"]:
        audio.append((cfg["mic_device"] or "@DEFAULT_SOURCE@", cfg["mic_volume"]))
    for dev, _ in audio:
        cmd += ["-thread_queue_size", "1024", "-f", "pulse", "-i", dev]

    fc = []
    vmap = "0:v"
    if enc == "vaapi":
        fc.append("[0:v]format=nv12,hwupload[v]")
        vmap = "[v]"
    if audio:
        labels = []
        for i, (_, vol) in enumerate(audio, start=1):
            fc.append(f"[{i}:a]volume={vol / 100:.2f}[a{i}]")
            labels.append(f"[a{i}]")
        if len(labels) == 2:
            fc.append(f"{labels[0]}{labels[1]}amix=inputs=2:normalize=0:duration=longest[a]")
        else:
            fc[-1] = fc[-1].replace("[a1]", "[a]")
    if fc:
        cmd += ["-filter_complex", ";".join(fc)]
    cmd += ["-map", vmap]
    if audio:
        cmd += ["-map", "[a]"]

    gop = str(fps * C.SEGMENT)
    if enc == "nvenc":
        # raw bgr0 frames go straight to the GPU, colour conversion happens there too
        cmd += ["-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ll", "-rc", "vbr", "-cq", str(q),
                "-b:v", "0", "-maxrate", "40M", "-bufsize", "40M", "-bf", "0", "-g", gop, "-forced-idr", "1"]
    elif enc == "vaapi":
        cmd += ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", str(q), "-g", gop, "-bf", "0"]
    else:
        cmd += ["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-crf", str(q),
                "-pix_fmt", "yuv420p", "-g", gop, "-threads", "2"]
    cmd += ["-force_key_frames", f"expr:gte(t,n_forced*{C.SEGMENT})"]
    if audio:
        cmd += ["-c:a", "aac", "-b:a", "160k", "-ar", "48000", "-ac", "2"]
    wrap = int(math.ceil(cfg["seconds"] / C.SEGMENT)) + 3
    cmd += ["-f", "segment", "-segment_time", str(C.SEGMENT), "-segment_wrap", str(wrap),
            "-segment_format", "mpegts", "-reset_timestamps", "1",
            # write every packet right away, otherwise the newest seconds sit in a 256 KB buffer
            "-segment_format_options", "flush_packets=1",
            os.path.join(C.RUNTIME, "seg_%03d.ts")]
    return cmd


class Daemon:
    def __init__(self, use_tray=True):
        self.cfg = C.load()
        self.use_tray = use_tray
        self.proc = None
        self.hk_proc = None
        self.hk_state = ("", False, "")
        self.paused = False
        self.state = "starting"
        self.error = ""
        self.enc = None
        self.geom = None
        self.fails = 0
        self.audio_off = False
        self.started = 0.0
        self.tray = None
        self.reexec = False
        self.update = None          # newer version on GitHub, if any
        self.info = system.detect_encoders()

    # ------------------------------------------------------------ recorder
    def clear_buffer(self):
        for f in glob.glob(os.path.join(C.RUNTIME, "seg_*.ts")):
            try:
                os.remove(f)
            except OSError:
                pass

    def start_recorder(self):
        self.stop_recorder()
        self.clear_buffer()
        if system.is_wayland():
            self.state, self.error = "error", "Wayland session: screen capture needs an X11 (Xorg) session."
            return
        order = system.encoder_order(self.cfg, self.info)
        self.enc = order[min(self.fails // 2, len(order) - 1)]
        self.geom = system.capture_geometry(self.cfg)
        cfg = dict(self.cfg)
        if self.audio_off:
            cfg["system_audio"] = cfg["mic"] = False
        cmd = build_cmd(cfg, self.enc, self.geom, self.info)
        with open(C.FFMPEG_LOG, "ab") as log:
            log.write(f"\n--- {time.ctime()} {' '.join(cmd)}\n".encode())
        self.started = time.time()
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=open(C.FFMPEG_LOG, "ab"),
                                     preexec_fn=lambda: os.nice(5))
        self.state, self.error = "recording", ""

    def kill_stale_recorders(self):
        """An ffmpeg left behind by a recorder that was killed would keep filling the buffer."""
        target = os.path.join(C.RUNTIME, "seg_").encode()
        for pid in filter(str.isdigit, os.listdir("/proc")):
            try:
                with open(f"/proc/{pid}/cmdline", "rb") as f:
                    cmd = f.read()
                if b"ffmpeg" in cmd and target in cmd and os.stat(f"/proc/{pid}").st_uid == os.getuid():
                    os.kill(int(pid), signal.SIGTERM)
            except OSError:
                pass

    def stop_recorder(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGINT)
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.proc = None

    def health(self):
        if self.paused or (self.state == "error" and self.proc is None):
            self.write_status()
            return True
        if self.proc and self.proc.poll() is None:
            if time.time() - self.started > 20:
                self.fails = 0
            # monitor layout changed -> restart with the new geometry
            if int(time.time()) % 5 == 0 and system.capture_geometry(self.cfg) != self.geom:
                self.start_recorder()
            self.write_status()
            return True
        # ffmpeg died: retry, then drop audio, then fall back to the next encoder
        self.fails += 1
        if self.fails == 3 and (self.cfg["system_audio"] or self.cfg["mic"]):
            self.audio_off = True
            self.notify("Sound capture failed, recording without sound.")
        if self.fails > 8:
            self.state, self.error = "error", "Recorder keeps failing: " + self.log_tail()
            self.proc = None
            self.write_status()
            return True
        self.start_recorder()
        self.write_status()
        return True

    def log_tail(self):
        try:
            with open(C.FFMPEG_LOG, "rb") as f:
                f.seek(max(0, os.path.getsize(C.FFMPEG_LOG) - 400))
                lines = [l for l in f.read().decode(errors="replace").splitlines() if l and not l.startswith("---")]
                return lines[-1] if lines else "unknown error"
        except OSError:
            return "unknown error"

    # ------------------------------------------------------------ hotkey
    def start_hotkey(self):
        self.stop_hotkey()
        accel = self.cfg["hotkey"]
        if not accel or system.is_wayland():
            self.hk_state = (accel, False, "no hotkey")
            return
        self.hk_state = (accel, False, "starting")
        log = open(C.HOTKEY_LOG, "a")
        p = subprocess.Popen([sys.executable, "-m", "drzreplay.recorder.hotkey", accel], stdout=subprocess.PIPE,
                             stderr=log, text=True, env=C.child_env())
        self.hk_proc = p

        def reader():
            failed = False
            for line in p.stdout:
                line = line.strip()
                log.write(f"{time.strftime('%H:%M:%S')} {p.pid} {line}\n")
                log.flush()
                if line == "OK":
                    self.hk_state = (accel, True, "")
                elif line.startswith("FAIL"):
                    failed = True
                    self.hk_state = (accel, False, line[5:])
                    GLib.idle_add(self.notify, f"Hotkey {pretty(accel)} doesn't work: {line[5:]}.")
                elif line == "PRESS":
                    GLib.idle_add(self.save)
                GLib.idle_add(self.write_status)
            p.wait()
            log.write(f"{time.strftime('%H:%M:%S')} {p.pid} exited {p.returncode}\n")
            log.close()
            if not failed:
                GLib.timeout_add_seconds(2, self.revive_hotkey, p)

        threading.Thread(target=reader, daemon=True).start()

    def revive_hotkey(self, p):
        if self.hk_proc is p:      # the helper died on its own: bring the hotkey back
            self.start_hotkey()
        return False

    def stop_hotkey(self):
        if self.hk_proc and self.hk_proc.poll() is None:
            self.hk_proc.terminate()
        self.hk_proc = None

    # ------------------------------------------------------------ actions
    def save(self, *_, seconds=None):
        with open(C.HOTKEY_LOG, "a") as f:
            f.write(f"{time.strftime('%H:%M:%S')} save\n")
        extra = ["--seconds", str(seconds)] if seconds else []
        subprocess.Popen([sys.executable, "-m", "drzreplay", "save"] + extra, env=C.child_env(),
                         start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return False

    def notify(self, text):
        if self.cfg["notifications"]:
            subprocess.Popen(["notify-send", "-a", C.APP_NAME, "-i", C.ICON_PATH, C.APP_NAME, text],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return False

    # ------------------------------------------------------------ updates
    def check_update(self):
        """Look for a new release in the background; installs from the .deb only."""
        if not system.installed():
            return True

        def work():
            latest = system.latest_version()
            if latest:
                C.write_json(C.UPDATE_CACHE, {**C.read_json(C.UPDATE_CACHE, {}), "time": time.time(), "latest": latest})
                if system.version_tuple(latest) > system.version_tuple(C.VERSION):
                    GLib.idle_add(self.on_update_available, latest)

        threading.Thread(target=work, daemon=True).start()
        return True

    def on_update_available(self, latest):
        self.update = latest
        self.update_tray()
        cache = C.read_json(C.UPDATE_CACHE, {})
        if cache.get("notified") == latest and time.time() - cache.get("notified_at", 0) < 24 * 3600:
            return False        # remind at most once a day
        C.write_json(C.UPDATE_CACHE, {**cache, "notified": latest, "notified_at": time.time()})

        def ask():
            try:
                r = subprocess.run(["notify-send", "-a", C.APP_NAME, "-i", C.ICON_PATH, "-u", "normal",
                                    "-A", "update=Update", "-A", "default=Update", "-w", "-t", "60000",
                                    f"{C.APP_NAME} {latest} is out", "A new version is ready to install."],
                                   capture_output=True, text=True, timeout=120)
            except Exception:
                return
            if {"update", "default"} & set(r.stdout.split()):     # the button or a click on the popup
                GLib.idle_add(self.run_update)

        threading.Thread(target=ask, daemon=True).start()
        return False

    def run_update(self, *_):
        if not system.run_in_terminal(C.INSTALL_CMD):
            self.notify(f"Open a terminal and run: {C.INSTALL_CMD}")
        return False

    def set_paused(self, paused):
        self.paused = paused
        if paused:
            self.stop_recorder()
            self.clear_buffer()
            self.state = "paused"
        else:
            self.fails = 0
            self.start_recorder()
        self.update_tray()
        self.write_status()

    def set_option(self, key, value):
        cfg = C.load()
        if cfg.get(key) != value:
            cfg[key] = value
            C.save(cfg)
            self.cfg[key] = value

    def reload(self, *_):
        old = self.cfg
        self.cfg = C.load()
        if self.state == "error" or any(old.get(k) != self.cfg.get(k) for k in C.RECORDER_KEYS):
            self.fails, self.audio_off = 0, False
            if not self.paused:
                self.start_recorder()
        if old["hotkey"] != self.cfg["hotkey"]:
            self.start_hotkey()
        if old["tray"] != self.cfg["tray"]:
            self.setup_tray()
        self.update_tray()
        self.write_status()
        return True

    def write_status(self, *_):
        segs = glob.glob(os.path.join(C.RUNTIME, "seg_*.ts"))
        buffered = 0.0
        if self.state == "recording" and self.started:
            buffered = min(float(self.cfg["seconds"]), time.time() - self.started)
        accel, ok, err = self.hk_state
        C.write_json(C.STATUSFILE, {
            "pid": os.getpid(), "state": self.state, "error": self.error, "encoder": self.enc,
            "geometry": list(self.geom) if self.geom else None, "fps": self.cfg["fps"],
            "seconds": self.cfg["seconds"], "buffered": buffered, "segments": len(segs),
            "buffer_mb": sum(os.path.getsize(s) for s in segs if os.path.exists(s)) / 1e6,
            "hotkey": accel, "hotkey_ok": ok, "hotkey_error": err, "audio_off": self.audio_off,
            "encoders": self.info["available"], "time": time.time()})
        return False

    # ------------------------------------------------------------ tray
    def setup_tray(self):
        if self.tray:
            self.tray.hide()
            self.tray = None
        if self.use_tray and self.cfg["tray"]:
            try:
                from ..ui.tray import Tray
                self.tray = Tray(self)
            except Exception as e:
                print(f"tray unavailable: {e}", file=sys.stderr)

    def update_tray(self):
        if self.tray:
            self.tray.update()

    def open_settings(self, *_):
        subprocess.Popen([sys.executable, "-m", "drzreplay", "settings"], env=C.child_env(),
                         start_new_session=True)

    def open_folder(self, *_):
        d = C.output_dir(self.cfg)
        os.makedirs(d, exist_ok=True)
        subprocess.Popen(["xdg-open", d], start_new_session=True)

    def upgrade(self, *_):
        self.reexec = True
        return self.quit()

    def quit(self, *_):
        self.stop_hotkey()
        self.stop_recorder()
        self.clear_buffer()
        for f in (C.PIDFILE, C.STATUSFILE):
            try:
                os.remove(f)
            except OSError:
                pass
        if self.loop:
            self.loop.quit()
        return False

    # ------------------------------------------------------------ main
    def run(self):
        os.makedirs(C.RUNTIME, exist_ok=True)
        with open(C.PIDFILE, "w") as f:
            f.write(str(os.getpid()))
        self.loop = GLib.MainLoop()
        self.kill_stale_recorders()
        self.setup_tray()
        self.start_recorder()
        self.start_hotkey()
        self.update_tray()
        self.write_status()
        GLib.timeout_add_seconds(1, self.health)
        GLib.timeout_add_seconds(60, lambda: (self.check_update(), False)[1])
        GLib.timeout_add_seconds(6 * 3600, self.check_update)
        for sig, cb in ((signal.SIGHUP, self.reload), (signal.SIGUSR1, lambda *_: (self.save(), True)[1]),
                        (signal.SIGUSR2, self.upgrade), (signal.SIGTERM, self.quit), (signal.SIGINT, self.quit)):
            GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, sig, cb)
        try:
            self.loop.run()
        finally:
            self.stop_hotkey()
            self.stop_recorder()


def main(args):
    os.makedirs(C.RUNTIME, exist_ok=True)
    lock = open(os.path.join(C.RUNTIME, "daemon.lock"), "w")
    try:
        # i3 may start us twice (dex autostart + exec line), only one may record
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print(f"{C.APP_NAME} is already running (pid {C.daemon_pid()})")
        return 0
    if GLib is None:
        print("python3-gi is required", file=sys.stderr)
        return 1
    GLib.set_prgname("drzreplay")
    d = Daemon(use_tray="--no-tray" not in args)
    d.run()
    if d.reexec:
        lock.close()
        os.execv(sys.executable, [sys.executable, "-m", "drzreplay", "daemon"] + args)
    return 0
