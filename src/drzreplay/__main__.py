"""DrzReplay — instant replay for Linux.

usage: drzreplay [command]

  (no command)   open Settings
  daemon         run the background recorder (tray + hotkey)
  start          start the recorder in the background if it is not running
  stop           stop the recorder
  restart        restart the recorder
  save [--seconds N]  save a replay now (optionally only the last N seconds)
  record [start|stop] start / stop recording a full video (no argument: toggle)
  status         show what the recorder is doing
  preview-intro  play the intro animation
  --version
"""
import os
import signal
import subprocess
import sys
import time

from .core import config as C


def cmd_start(args):
    if C.daemon_pid():
        print(f"{C.APP_NAME} is running (pid {C.daemon_pid()})")
        return 0
    subprocess.Popen([sys.executable, "-m", "drzreplay", "daemon"] + args, env=C.child_env(),
                     start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(30):
        if C.daemon_pid():
            print(f"{C.APP_NAME} started")
            return 0
        time.sleep(0.1)
    print(f"{C.APP_NAME} did not start, try: drzreplay daemon", file=sys.stderr)
    return 1


def cmd_stop(args):
    pid = C.daemon_pid()
    if not pid:
        print("not running")
        return 0
    os.kill(pid, signal.SIGTERM)
    for _ in range(60):
        if not C.daemon_pid():
            break
        time.sleep(0.1)
    print("stopped")
    return 0


def cmd_restart(args):
    cmd_stop(args)
    return cmd_start(args)


def cmd_record(args):
    pid = C.daemon_pid()
    if not pid:
        print(f"{C.APP_NAME} is not running (drzreplay start)", file=sys.stderr)
        return 1
    cmd = {"start": "record-start", "stop": "record-stop"}.get(args[0] if args else "", "record")
    tmp = C.REQUEST + ".tmp"
    with open(tmp, "w") as f:
        f.write(cmd)
    os.replace(tmp, C.REQUEST)
    os.kill(pid, signal.SIGHUP)
    return 0


def cmd_status(args):
    from .recorder.hotkey import pretty
    pid = C.daemon_pid()
    st = C.read_json(C.STATUSFILE, {}) if pid else {}
    cfg = C.load()
    print(f"recorder : {st.get('state', 'stopped') if pid else 'stopped'}" + (f" (pid {pid})" if pid else ""))
    if st.get("error"):
        print(f"error    : {st['error']}")
    if pid:
        g = st.get("geometry") or [0, 0, 0, 0]
        print(f"capture  : {g[0]}x{g[1]} @ {st.get('fps')} fps, encoder {st.get('encoder')}")
        print(f"buffer   : {st.get('buffered', 0):.0f}/{st.get('seconds')} s, {st.get('buffer_mb', 0):.0f} MB in RAM")
        ok = "ok" if st.get("hotkey_ok") else f"NOT working ({st.get('hotkey_error')})"
        print(f"hotkey   : {pretty(cfg['hotkey'])} — {ok}")
        if cfg["record_hotkey"]:
            ok = "ok" if st.get("record_hotkey_ok") else f"NOT working ({st.get('record_hotkey_error')})"
            print(f"rec key  : {pretty(cfg['record_hotkey'])} — {ok}")
        if st.get("file_recording"):
            mins, secs = divmod(int(time.time() - st["file_recording"]), 60)
            print(f"video    : recording, {mins}:{secs:02d}")
    print(f"replays  : {C.output_dir(cfg)}")
    return 0


def cmd_preview_intro(args):
    from .recorder.save import intro_ready
    if not intro_ready():
        print("intro files are missing", file=sys.stderr)
        return 1
    cfg = C.load()
    os.makedirs(C.RUNTIME, exist_ok=True)
    out = os.path.join(C.RUNTIME, "intro-preview.mp4")
    d = C.INTRO_DIR
    iv = int(cfg["intro_volume"]) / 100
    fc = ("color=c=0x15191e:s=1920x1080:r=30:d=6[bg];"
          "[0:v]format=yuv444p[gc];[1:v]format=gray[ga];[gc][ga]alphamerge[g];"
          f"[bg][g]overlay=eof_action=pass,format=yuv420p[v];[2:a]volume={iv:.2f},apad[a]")
    r = subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", os.path.join(d, "gate_color.mp4"),
                        "-i", os.path.join(d, "gate_alpha.mkv"), "-i", os.path.join(d, "intro_sfx.wav"),
                        "-filter_complex", fc, "-map", "[v]", "-map", "[a]", "-t", "6",
                        "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-c:a", "aac", out])
    if r.returncode != 0:
        return 1
    subprocess.Popen(["xdg-open", out], start_new_session=True)
    return 0


def main():
    args = sys.argv[1:]
    cmd = args[0] if args else "settings"
    rest = args[1:]
    if cmd in ("-h", "--help", "help"):
        print(__doc__)
        return 0
    if cmd in ("-v", "--version"):
        print(f"{C.APP_NAME} {C.VERSION}")
        return 0
    if cmd == "daemon":
        from .recorder import daemon
        return daemon.main(rest)
    if cmd == "save":
        from .recorder import save
        return save.main(rest)
    if cmd == "settings":
        from .ui import window
        return window.main(rest)
    table = {"start": cmd_start, "stop": cmd_stop, "restart": cmd_restart, "status": cmd_status, "record": cmd_record,
             "preview-intro": cmd_preview_intro}
    if cmd not in table:
        print(__doc__)
        return 2
    return table[cmd](rest)


if __name__ == "__main__":
    sys.exit(main() or 0)
