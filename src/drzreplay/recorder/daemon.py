"""Background recorder: ring buffer of short MPEG-TS chunks in RAM + tray + hotkey.

Signals: SIGHUP = reload settings (or run the command in RUNTIME/request), SIGUSR1 = save a replay, SIGUSR2 = restart with the
newly installed code (sent by the package after an upgrade), SIGTERM = quit.
"""
import fcntl
import glob
import math
import os
import shutil
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

    gop = str(fps)                      # a keyframe every second: full recordings can start on time
    if enc == "nvenc":
        # raw bgr0 frames go straight to the GPU, colour conversion happens there too
        cmd += ["-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ll", "-rc", "vbr", "-cq", str(q),
                "-b:v", "0", "-maxrate", "40M", "-bufsize", "40M", "-bf", "0", "-g", gop, "-forced-idr", "1"]
    elif enc == "vaapi":
        cmd += ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", str(q), "-g", gop, "-bf", "0"]
    else:
        cmd += ["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-crf", str(q),
                "-pix_fmt", "yuv420p", "-g", gop, "-threads", "2"]
    cmd += ["-force_key_frames", "expr:gte(t,n_forced)"]
    if audio:
        cmd += ["-c:a", "aac", "-b:a", "160k", "-ar", "48000", "-ac", "2"]
    return cmd + segment_args(cfg)


def segment_args(cfg):
    """The ring buffer: 5-second MPEG-TS chunks in RAM, the oldest one overwritten."""
    wrap = int(math.ceil(cfg["seconds"] / C.SEGMENT)) + 3
    return ["-f", "segment", "-segment_time", str(C.SEGMENT), "-segment_wrap", str(wrap),
            "-segment_format", "mpegts", "-reset_timestamps", "1",
            # write every packet right away, otherwise the newest seconds sit in a 256 KB buffer
            "-segment_format_options", "flush_packets=1",
            os.path.join(C.RUNTIME, "seg_%03d.ts")]


def wayland_ffmpeg_cmd(cfg):
    """ffmpeg only cuts GStreamer's finished stream into the ring buffer on Wayland."""
    # a still screen may send its first frame late: give the probe time to see the video, and insist on it
    # (without -map 0:v ffmpeg would quietly record sound only)
    audio = ["-map", "0:a"] if cfg["system_audio"] or cfg["mic"] else []
    return ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-probesize", "32M",
            "-analyzeduration", "8000000", "-f", "mpegts", "-i", "-", "-map", "0:v"] + audio + \
        ["-c", "copy"] + segment_args(cfg)


class Daemon:
    def __init__(self, use_tray=True):
        self.cfg = C.load()
        self.use_tray = use_tray
        self.proc = None
        self.hk_proc = None
        self.hk_state = ("", False, "")
        self.rk_proc = None                 # the second helper: start / stop recording
        self.rk_state = ("", False, "")
        self.filerec = None                 # recording a full video: {"dir", "t0", "copied", "n", "name"}
        self.paused = False
        self.rec_only = False       # replays are off, the screen is captured only for a full recording
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
        self.wayland = system.is_wayland()
        self.cast = None            # Wayland screen-sharing session
        self.shortcuts = None
        self.gst_proc = None
        self.info = self.wayland_encoders() if self.wayland else system.detect_encoders()

    # ------------------------------------------------------------ recorder
    def clear_buffer(self):
        for f in glob.glob(os.path.join(C.RUNTIME, "seg_*.ts")):
            try:
                os.remove(f)
            except OSError:
                pass

    def start_recorder(self):
        self.stop_recorder()
        if self.filerec:
            self.filerec_copy(final=True)   # a settings change mid-recording: keep what's there
        self.clear_buffer()
        if self.wayland:
            self.start_wayland()
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

    # ------------------------------------------------------------ Wayland
    def wayland_encoders(self):
        from . import wayland as WL
        # GStreamer's encoders, kept apart from ffmpeg's (saving still encodes with ffmpeg)
        cache = os.path.join(C.RUNTIME, "gst_encoders.json")
        cached = C.read_json(cache)
        if cached and time.time() - cached.get("time", 0) < 24 * 3600:
            return cached
        res = {"available": WL.gst_encoders() or ["x264"], "vaapi_device": None, "time": time.time()}
        try:
            C.write_json(cache, res)
        except OSError:
            pass
        return res

    def start_wayland(self):
        from . import wayland as WL
        if self.cast is None:
            need = WL.missing_parts()
            if need:
                self.state, self.error = "error", "Wayland capture needs: sudo apt install " + " ".join(need)
                return
            try:
                self.cast = WL.ScreenCast(self.on_cast_ready, self.on_cast_error, self.on_cast_closed,
                                          cursor=self.cfg["draw_mouse"])
                if not self.cast.available():
                    raise RuntimeError("no screen-sharing portal (install xdg-desktop-portal for your desktop)")
                self.cast.start()
            except Exception as e:
                self.cast = None
                self.state, self.error = "error", f"Screen sharing: {e}"
                return
            self.state, self.error = "starting", "Choose the screen to record in the dialog"
            return
        if not self.cast.streams:
            return                                # still waiting for the dialog
        node, props = self.cast.streams[0]
        order = system.encoder_order(self.cfg, self.info)
        self.enc = order[min(self.fails // 2, len(order) - 1)]
        cfg = dict(self.cfg)
        if self.audio_off:
            cfg["system_audio"] = cfg["mic"] = False
        try:
            fd = self.cast.open_remote()
        except Exception as e:
            self.state, self.error = "error", f"Screen sharing: {e}"
            self.cast = None
            return
        gst = WL.gst_cmd(cfg, self.enc, node, fd)
        ff = wayland_ffmpeg_cmd(cfg)
        with open(C.FFMPEG_LOG, "ab") as log:
            log.write(f"\n--- {time.ctime()} {' '.join(gst)}\n    | {' '.join(ff)}\n".encode())
        self.started = time.time()
        self.gst_proc = subprocess.Popen(gst, stdout=subprocess.PIPE, stderr=open(C.FFMPEG_LOG, "ab"),
                                         pass_fds=(fd,), preexec_fn=lambda: os.nice(5))
        os.close(fd)
        self.proc = subprocess.Popen(ff, stdin=self.gst_proc.stdout, stdout=subprocess.DEVNULL,
                                     stderr=open(C.FFMPEG_LOG, "ab"), preexec_fn=lambda: os.nice(5))
        self.gst_proc.stdout.close()
        w, h = props.get("size", (0, 0))
        self.geom = (w, h, 0, 0)
        self.state, self.error = "recording", ""

    def on_cast_ready(self, _streams):
        if not self.paused:
            self.start_recorder()
        self.update_tray()
        self.write_status()

    def on_cast_error(self, msg):
        self.cast = None
        self.stop_recorder()
        self.state = "error"
        self.error = "Screen sharing was not allowed" if msg == "cancelled" else f"Screen sharing: {msg}"
        self.update_tray()
        self.write_status()

    def on_cast_closed(self):
        self.cast = None
        self.stop_recorder()
        self.clear_buffer()
        self.state, self.error = "error", "Screen sharing was stopped"
        self.notify("Screen sharing was stopped. Turn recording back on in DrzReplay.")
        self.update_tray()
        self.write_status()

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
        if self.gst_proc and self.gst_proc.poll() is None:
            self.gst_proc.terminate()
        for p in (self.proc, self.gst_proc):
            if p and p.poll() is None:
                p.send_signal(signal.SIGINT)
                try:
                    p.wait(5)
                except subprocess.TimeoutExpired:
                    p.kill()
        self.proc = self.gst_proc = None

    def health(self):
        if self.paused or (self.state == "error" and self.proc is None) or (
                self.wayland and self.proc is None and self.state == "starting"):
            self.write_status()             # paused, broken, or waiting for the screen-sharing dialog
            return True
        if self.proc and self.proc.poll() is None:
            if time.time() - self.started > 20:
                self.fails = 0
            # monitor layout changed -> restart with the new geometry
            if self.filerec:
                self.update_tray()          # the elapsed time in the tray menu
            if not self.wayland and int(time.time()) % 5 == 0 and system.capture_geometry(self.cfg) != self.geom:
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

    # ------------------------------------------------------------ sound devices
    def recorder_pids(self):
        return {p.pid for p in (self.proc, getattr(self, "gst_proc", None)) if p and p.poll() is None}

    def start_audio_follow(self):
        """Keep recording what you hear when the default device changes (headphones plugged in…):
        our recording streams are moved to the new device, no restart, the buffer is kept."""
        if not shutil.which("pactl"):
            return
        self.defaults = system.pulse_defaults()
        try:
            p = subprocess.Popen(["pactl", "subscribe"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                 text=True)
        except OSError:
            return
        self.sub_proc = p

        def reader():
            for line in p.stdout:
                if "on server" in line:
                    GLib.idle_add(self.follow_default)

        threading.Thread(target=reader, daemon=True).start()

    def follow_default(self):
        new = system.pulse_defaults()
        if new == self.defaults or None in new:
            return False
        self.defaults = new
        sink, source = new
        for idx, src in system.pulse_streams(self.recorder_pids()):
            if not src:
                continue
            if src.endswith(".monitor"):
                target = sink + ".monitor" if self.cfg["system_audio"] and not self.cfg["system_device"] else None
            else:
                target = source if self.cfg["mic"] and not self.cfg["mic_device"] else None
            if target and target != src:
                system.run(["pactl", "move-source-output", str(idx), target])
        return False

    def stop_audio_follow(self):
        p = getattr(self, "sub_proc", None)
        if p and p.poll() is None:
            p.terminate()
        self.sub_proc = None

    # ------------------------------------------------------------ recording a full video
    def toggle_file_recording(self, *_):
        if self.filerec:
            self.stop_file_recording()
        else:
            self.start_file_recording()
        return False

    def start_file_recording(self):
        """Every finished ring chunk is copied to disk until stopped, then joined without re-encoding."""
        if self.filerec:
            return
        if self.paused:
            # replays are off: capture just for this video, and stop again when it ends
            self.rec_only, self.paused, self.fails = True, False, 0
            self.start_recorder()
        elif self.state != "recording":
            self.notify("The recorder isn't working, see Recorder log in the window menu.")
            return
        stamp = time.strftime("%Y-%m-%d %H-%M-%S")
        d = os.path.join(C.output_dir(self.cfg), f".recording {stamp}")
        try:
            os.makedirs(d, exist_ok=True)
        except OSError as e:
            self.notify(f"Can't record: {e.strerror}")
            return
        self.filerec = {"dir": d, "t0": time.time(), "copied": set(), "n": 0,
                        "name": f"{self.cfg['file_prefix'] or C.APP_NAME} {stamp}"}
        self.click()
        self.notify(f"Recording. Press {pretty(self.cfg['record_hotkey'])} again to stop.")
        GLib.timeout_add_seconds(1, self.filerec_tick)
        self.update_tray()
        self.write_status()

    def filerec_tick(self):
        if not self.filerec:
            return False
        self.filerec_copy()
        return True

    def filerec_copy(self, final=False):
        """Copy the chunks finished since the start (all of them, the open one too, when final)."""
        rec = self.filerec
        segs = []
        for f in glob.glob(os.path.join(C.RUNTIME, "seg_*.ts")):
            try:
                st = os.stat(f)
            except OSError:
                continue
            segs.append((st.st_mtime_ns, st.st_size, f))
        segs.sort()
        if not final:
            segs = segs[:-1]                 # the newest chunk is still being written
        for mtime, size, f in segs:
            # a chunk last written before the start holds nothing of the recording (it's 5 s long,
            # so the first one may begin up to 5 s early)
            if mtime / 1e9 < rec["t0"] or (f, mtime) in rec["copied"] or not size:
                continue
            try:
                shutil.copyfile(f, os.path.join(rec["dir"], f"{rec['n']:06d}.ts"))
            except OSError:
                continue
            if not rec["n"]:
                rec["first_end"] = mtime / 1e9          # when the first chunk was finished
            rec["copied"].add((f, mtime))
            rec["n"] += 1

    def stop_file_recording(self, wait=False):
        rec = self.filerec
        if not rec:
            return
        back_off, self.rec_only = self.rec_only, False
        if back_off:
            self.stop_recorder()             # flushes the open chunk; nothing else needs the capture
        self.filerec_copy(final=True)
        self.filerec = None
        self.click()
        if back_off:
            self.set_paused(True)            # replays stay off, as they were
        self.update_tray()
        self.write_status()
        length = time.time() - rec["t0"]

        def work():
            parts = sorted(glob.glob(os.path.join(rec["dir"], "*.ts")))
            if not parts:
                shutil.rmtree(rec["dir"], ignore_errors=True)
                GLib.idle_add(self.notify, "The recording was empty.")
                return
            from . import save as S
            # the first chunk began before the key was pressed: start at the keyframe nearest the press
            first_len = S.probe(parts[0])[0]
            offset = rec["t0"] - (rec.get("first_end", rec["t0"]) - first_len)
            key = S.keyframe_before(parts[0], offset)[0] if 0.2 < offset < first_len else None
            listfile = os.path.join(rec["dir"], "list.txt")
            with open(listfile, "w") as f:
                for i, p in enumerate(parts):
                    f.write(f"file '{p}'\n")
                    if i == 0 and key is not None:
                        f.write(f"inpoint {key:.3f}\n")
            ext = "mkv" if self.cfg["container"] == "mkv" else "mp4"
            out = os.path.join(C.output_dir(self.cfg), f"{rec['name']}.{ext}")
            tmp = os.path.join(rec["dir"], f"out.{ext}")
            cmd = ["nice", "-n", "10", "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
                   "-f", "concat", "-safe", "0", "-i", listfile, "-map", "0", "-c", "copy"]
            if ext == "mp4":
                cmd += ["-movflags", "+faststart"]
            r = subprocess.run(cmd + [tmp], capture_output=True, text=True)
            if r.returncode != 0 or not os.path.exists(tmp):
                err = (r.stderr.strip().splitlines() or ["ffmpeg failed"])[-1]
                GLib.idle_add(self.notify, f"Couldn't save the recording ({err}). The pieces are in {rec['dir']}")
                return
            os.replace(tmp, out)
            shutil.rmtree(rec["dir"], ignore_errors=True)
            size = os.path.getsize(out) / 1e9
            mins, secs = divmod(int(length), 60)
            res = S.notify(self.cfg, f"Recording saved: {os.path.basename(out)} · {mins}:{secs:02d} · "
                           f"{size:.2f} GB" if size >= 0.1 else f"Recording saved: {os.path.basename(out)} · "
                           f"{mins}:{secs:02d}", actions=[("play", "Play"), ("folder", "Show in folder")],
                           wait=not wait)
            if res and "play" in res:
                subprocess.Popen(["xdg-open", out], start_new_session=True)
            elif res and "folder" in res:
                S.show_in_folder(out)

        if wait:
            work()
        else:
            threading.Thread(target=work, daemon=True).start()

    def click(self):
        if self.cfg["click_sound"] and shutil.which("paplay") and os.path.exists(C.CLICK_SOUND):
            subprocess.Popen(["paplay", "--volume=30000", C.CLICK_SOUND], stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL, start_new_session=True)

    # ------------------------------------------------------------ hotkeys
    def start_hotkey(self):
        """Two global keys: save a replay (hotkey) and start / stop recording (record_hotkey)."""
        self.stop_hotkey()
        keys = {"save": self.cfg["hotkey"], "record": self.cfg["record_hotkey"]}
        for name, accel in keys.items():
            self.set_hk(name, (accel, False, "starting" if accel else "no hotkey"))
        if self.wayland:
            self.start_portal_hotkey({k: a for k, a in keys.items() if a})
            return
        for name, accel in keys.items():
            if accel:
                self.start_helper(name, accel)

    def set_hk(self, name, state):
        if name == "save":
            self.hk_state = state
        else:
            self.rk_state = state

    def on_hotkey(self, name):
        if name == "save":
            self.save()
        else:
            self.toggle_file_recording()
        return False

    def start_helper(self, name, accel):
        log = open(C.HOTKEY_LOG, "a")
        p = subprocess.Popen([sys.executable, "-m", "drzreplay.recorder.hotkey", accel], stdout=subprocess.PIPE,
                             stderr=log, text=True, env=C.child_env())
        if name == "save":
            self.hk_proc = p
        else:
            self.rk_proc = p

        def reader():
            failed = False
            for line in p.stdout:
                line = line.strip()
                log.write(f"{time.strftime('%H:%M:%S')} {p.pid} {name} {line}\n")
                log.flush()
                if line == "OK":
                    self.set_hk(name, (accel, True, ""))
                elif line.startswith("FAIL"):
                    failed = True
                    self.set_hk(name, (accel, False, line[5:]))
                    GLib.idle_add(self.notify, f"Hotkey {pretty(accel)} doesn't work: {line[5:]}.")
                elif line == "PRESS":
                    GLib.idle_add(self.on_hotkey, name)
                GLib.idle_add(self.write_status)
            p.wait()
            log.write(f"{time.strftime('%H:%M:%S')} {p.pid} {name} exited {p.returncode}\n")
            log.close()
            if not failed:
                GLib.timeout_add_seconds(2, self.revive_hotkey, name, p)

        threading.Thread(target=reader, daemon=True).start()

    def start_portal_hotkey(self, keys):
        from . import wayland as WL

        def state(ok, err):
            for name, accel in keys.items():
                self.set_hk(name, (accel, ok, err))
            self.write_status()

        try:
            self.shortcuts = WL.Shortcuts(keys, self.on_portal_press, state)
            self.shortcuts.start()
        except Exception as e:
            state(False, str(e))

    def on_portal_press(self, name):
        now = time.time()
        last = getattr(self, "_last_press", {})
        if now - last.get(name, 0) > 0.5:
            last[name] = now
            self._last_press = last
            self.on_hotkey(name)

    def revive_hotkey(self, name, p):
        # the helper died on its own: bring that hotkey back
        if (self.hk_proc if name == "save" else self.rk_proc) is p:
            accel = self.cfg["hotkey" if name == "save" else "record_hotkey"]
            if accel:
                self.start_helper(name, accel)
        return False

    def stop_hotkey(self):
        for p in (self.hk_proc, self.rk_proc):
            if p and p.poll() is None:
                p.terminate()
        self.hk_proc = self.rk_proc = None
        if self.shortcuts:
            self.shortcuts.close()
            self.shortcuts = None

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
        """The Update button: installs in the background, no password; the package then restarts us."""
        if getattr(self, "_updating", False):
            return False
        self._updating = True
        self.notify(f"Updating to {self.update or 'the new version'}…")

        def work():
            ok, msg = system.self_update()
            GLib.idle_add(self.update_done, ok, msg)

        threading.Thread(target=work, daemon=True).start()
        return False

    def update_done(self, ok, msg):
        self._updating = False
        if ok is None:                    # installed before the helper existed: the old way, once
            if not system.run_in_terminal(C.INSTALL_CMD):
                self.notify(f"Open a terminal and run: {C.INSTALL_CMD}")
        elif not ok:
            self.notify(f"Update failed: {msg}")
        return False

    def set_paused(self, paused):
        if not paused and self.rec_only:
            # replays turned on during a recording: the capture is already running, just keep it
            self.rec_only = False
            self.update_tray()
            self.write_status()
            return
        self.paused = paused
        if paused:
            self.stop_file_recording()
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

    def handle_request(self):
        """`drzreplay record` and the window's Record button leave a command here, then send SIGHUP."""
        try:
            with open(C.REQUEST) as f:
                cmd = f.read().strip()
            os.remove(C.REQUEST)
        except OSError:
            return False
        if cmd in ("pause", "resume"):
            self.set_paused(cmd == "pause")
        elif cmd == "record":
            self.toggle_file_recording()
        elif cmd == "record-start":
            self.start_file_recording()
        elif cmd == "record-stop":
            self.stop_file_recording()
        return True

    def reload(self, *_):
        if self.handle_request():
            return True
        old = self.cfg
        self.cfg = C.load()
        if self.state == "error" or any(old.get(k) != self.cfg.get(k) for k in C.RECORDER_KEYS):
            self.fails, self.audio_off = 0, False
            if not self.paused:
                self.start_recorder()
        if old["hotkey"] != self.cfg["hotkey"] or old["record_hotkey"] != self.cfg["record_hotkey"]:
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
            since = self.started
            last = C.read_json(C.LAST_SAVE, {}) or {}
            if self.cfg["replay_since_last"] and last.get("session") == self.started:
                since = max(since, float(last.get("end", 0)))      # a replay empties the buffer
            buffered = max(0.0, min(float(self.cfg["seconds"]), time.time() - since))
        accel, ok, err = self.hk_state
        C.write_json(C.STATUSFILE, {
            "pid": os.getpid(), "state": self.state, "error": self.error, "encoder": self.enc,
            "geometry": list(self.geom) if self.geom else None, "fps": self.cfg["fps"],
            "seconds": self.cfg["seconds"], "buffered": buffered, "segments": len(segs),
            "buffer_mb": sum(os.path.getsize(s) for s in segs if os.path.exists(s)) / 1e6,
            "hotkey": accel, "hotkey_ok": ok, "hotkey_error": err, "audio_off": self.audio_off,
            "record_hotkey": self.rk_state[0], "record_hotkey_ok": self.rk_state[1],
            "record_hotkey_error": self.rk_state[2], "started": self.started,
            "file_recording": self.filerec["t0"] if self.filerec else None,
            "replays_off": self.paused or self.rec_only,
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

    restart = upgrade       # the same thing: quit cleanly, then start again in this process

    def quit(self, *_):
        self.stop_file_recording(wait=True)      # don't lose a running recording on quit / upgrade
        if self.cast:
            self.cast.close()
            self.cast = None
        self.stop_audio_follow()
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
        if self.paused:                      # started for a full recording with replays off
            self.state = "paused"
        else:
            self.start_recorder()
        self.start_hotkey()
        self.start_audio_follow()
        cache = C.read_json(C.UPDATE_CACHE, {})
        if cache.get("running") and cache["running"] != C.VERSION:
            self.notify(f"Updated to {C.VERSION}. Still recording.")
        if cache.get("running") != C.VERSION:
            C.write_json(C.UPDATE_CACHE, {**cache, "running": C.VERSION})
        # before the first status: `drzreplay record` waits for it, then sends SIGHUP
        for sig, cb in ((signal.SIGHUP, self.reload), (signal.SIGUSR1, lambda *_: (self.save(), True)[1]),
                        (signal.SIGUSR2, self.upgrade), (signal.SIGTERM, self.quit), (signal.SIGINT, self.quit)):
            GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, sig, cb)
        self.update_tray()
        self.write_status()
        GLib.timeout_add_seconds(1, self.health)
        GLib.timeout_add_seconds(60, lambda: (self.check_update(), False)[1])
        GLib.timeout_add_seconds(6 * 3600, self.check_update)
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
    d.paused = "--paused" in args
    d.run()
    if d.reexec:
        lock.close()
        # come back the way it was: replays on, or off
        args = [a for a in args if a != "--paused"] + (["--paused"] if d.paused else [])
        os.execv(sys.executable, [sys.executable, "-m", "drzreplay", "daemon"] + args)
    return 0
