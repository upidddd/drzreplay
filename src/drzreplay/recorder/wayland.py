"""Wayland: the screen comes from the ScreenCast portal (PipeWire), the hotkey from the GlobalShortcuts portal.

The desktop asks once which screen to share; the answer is remembered (restore token), so later
starts are silent. GStreamer records the PipeWire stream and the sound with one clock and hands
an MPEG-TS stream to ffmpeg, which writes the same ring buffer as on X11.
"""
import os
import secrets

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

from ..core import config as C  # noqa: E402

PORTAL = "org.freedesktop.portal.Desktop"
PATH = "/org/freedesktop/portal/desktop"
TOKEN_FILE = os.path.join(C.CACHE_DIR, "screencast.json")


def _s(v):
    return GLib.Variant("s", v)


class _Portal:
    """Request / Response plumbing shared by both portals."""
    iface = ""

    def __init__(self):
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.sender = self.bus.get_unique_name()[1:].replace(".", "_")
        self.session = None
        self._subs = []

    def prop(self, name, default=0):
        try:
            v = self.bus.call_sync(PORTAL, PATH, "org.freedesktop.DBus.Properties", "Get",
                                   GLib.Variant("(ss)", (self.iface, name)), GLib.VariantType("(v)"),
                                   Gio.DBusCallFlags.NONE, 3000, None)
            return v.unpack()[0]
        except GLib.Error:
            return default

    def available(self):
        return self.prop("version", 0) > 0

    def request(self, method, args, options, cb, fail):
        """Call a portal method that answers with a Request::Response signal."""
        token = "drz" + secrets.token_hex(6)
        path = f"{PATH}/request/{self.sender}/{token}"
        sub = None

        def on_response(_conn, _sender, _path, _iface, _signal, params):
            self.bus.signal_unsubscribe(sub)
            code, results = params.unpack()
            if code == 0:
                cb(results)
            else:
                fail("cancelled" if code == 1 else f"{method} failed")

        sub = self.bus.signal_subscribe(PORTAL, "org.freedesktop.portal.Request", "Response", path, None,
                                        Gio.DBusSignalFlags.NONE, on_response)
        options = dict(options, handle_token=_s(token))
        sig = "(" + "".join(t for t, _ in args) + "a{sv})"
        params = GLib.Variant(sig, tuple(v for _, v in args) + (options,))

        def done(bus, res):
            try:
                bus.call_finish(res)
            except GLib.Error as e:
                self.bus.signal_unsubscribe(sub)
                fail(e.message)

        self.bus.call(PORTAL, PATH, self.iface, method, params, GLib.VariantType("(o)"),
                      Gio.DBusCallFlags.NONE, -1, None, done)

    def create_session(self, cb, fail):
        def created(res):
            self.session = res["session_handle"]
            self._subs.append(self.bus.signal_subscribe(
                PORTAL, "org.freedesktop.portal.Session", "Closed", self.session, None,
                Gio.DBusSignalFlags.NONE, lambda *_: self.on_closed()))
            cb()
        self.request("CreateSession", [], {"session_handle_token": _s("drz" + secrets.token_hex(6))}, created, fail)

    def on_closed(self):
        self.session = None

    def close(self):
        for sub in self._subs:
            self.bus.signal_unsubscribe(sub)
        self._subs = []
        if self.session:
            try:
                self.bus.call_sync(PORTAL, self.session, "org.freedesktop.portal.Session", "Close", None, None,
                                   Gio.DBusCallFlags.NONE, 2000, None)
            except GLib.Error:
                pass
        self.session = None


class ScreenCast(_Portal):
    """on_ready(streams) once the user picked a screen; on_error(text); on_closed() if sharing is stopped."""
    iface = "org.freedesktop.portal.ScreenCast"

    def __init__(self, on_ready, on_error, on_closed, cursor=True):
        super().__init__()
        self.on_ready, self.on_error, self._on_closed = on_ready, on_error, on_closed
        self.cursor = cursor
        self.streams = []          # [(node_id, {"size": (w, h), ...})]

    def start(self):
        self.create_session(self._select, self.on_error)

    def _select(self):
        modes = self.prop("AvailableCursorModes", 1)
        opts = {"types": GLib.Variant("u", 1), "multiple": GLib.Variant("b", False),
                "cursor_mode": GLib.Variant("u", 2 if self.cursor and modes & 2 else 1)}
        if self.prop("version", 1) >= 4:
            opts["persist_mode"] = GLib.Variant("u", 2)          # until the user revokes it
            token = (C.read_json(TOKEN_FILE, {}) or {}).get("token")
            if token:
                opts["restore_token"] = _s(token)
        self.request("SelectSources", [("o", self.session)], opts,
                     lambda _r: self.request("Start", [("o", self.session), ("s", "")], {}, self._started,
                                             self.on_error),
                     self.on_error)

    def _started(self, res):
        if res.get("restore_token"):
            C.write_json(TOKEN_FILE, {"token": res["restore_token"]})
        self.streams = [(node, dict(props)) for node, props in res.get("streams", [])]
        if not self.streams:
            self.on_error("no screen was shared")
            return
        self.on_ready(self.streams)

    def open_remote(self):
        """A new PipeWire connection (fd) for one recorder process."""
        res, fds = self.bus.call_with_unix_fd_list_sync(
            PORTAL, PATH, self.iface, "OpenPipeWireRemote", GLib.Variant("(oa{sv})", (self.session, {})),
            GLib.VariantType("(h)"), Gio.DBusCallFlags.NONE, 5000, None, None)
        return fds.get(res.unpack()[0])

    def on_closed(self):
        super().on_closed()
        self._on_closed()

    @staticmethod
    def forget():
        """Ask which screen to share again next time."""
        try:
            os.remove(TOKEN_FILE)
        except OSError:
            pass


def portal_trigger(accel):
    """GTK accelerator "<Alt>z" → shortcut trigger "ALT+z" (XDG shortcuts spec)."""
    names = {"Control": "CTRL", "Primary": "CTRL", "Alt": "ALT", "Shift": "SHIFT", "Super": "LOGO", "Meta": "LOGO"}
    mods, key = [], accel
    while key.startswith("<"):
        end = key.index(">")
        mods.append(names.get(key[1:end], key[1:end].upper()))
        key = key[end + 1:]
    return "+".join(mods + [key])


class Shortcuts(_Portal):
    """Global hotkey through the desktop (GNOME 48+, KDE Plasma 6): on_press() when it's pressed."""
    iface = "org.freedesktop.portal.GlobalShortcuts"

    def __init__(self, accel, on_press, on_state):
        super().__init__()
        self.accel, self.on_press, self.on_state = accel, on_press, on_state

    def start(self):
        if not self.available():
            self.on_state(False, "this desktop has no global shortcuts; bind “drzreplay save” in its keyboard settings")
            return
        self._subs.append(self.bus.signal_subscribe(PORTAL, self.iface, "Activated", PATH, None,
                                                    Gio.DBusSignalFlags.NONE, self._activated))
        self.create_session(self._bind, lambda e: self.on_state(False, e))

    def _bind(self):
        shortcut = ("save", {"description": _s("Save a replay"), "preferred_trigger": _s(portal_trigger(self.accel))})
        self.request("BindShortcuts", [("o", self.session), ("a(sa{sv})", [shortcut]), ("s", "")], {},
                     lambda _r: self.on_state(True, ""), lambda e: self.on_state(False, e))

    def _activated(self, _conn, _sender, _path, _iface, _signal, params):
        session, shortcut = params.unpack()[:2]
        if session == self.session and shortcut == "save":
            self.on_press()


# ---------------------------------------------------------------------- GStreamer
def _gst_encoder(enc, fps, q):
    k = fps * C.SEGMENT
    if enc == "nvenc":
        return f"videoconvert n-threads=2 ! video/x-raw,format=NV12 ! nvh264enc gop-size={k} bframes=0 rc-mode=vbr " \
               f"const-quality={q} ! video/x-h264,profile=high"
    if enc == "vaapi":
        return f"videoconvert n-threads=2 ! video/x-raw,format=NV12 ! vah264enc key-int-max={k} b-frames=0 " \
               f"rate-control=cqp qpi={q} qpp={q} ! video/x-h264,profile=high"
    return f"videoconvert n-threads=2 ! video/x-raw,format=I420 ! x264enc speed-preset=ultrafast tune=zerolatency " \
           f"pass=qual quantizer={q} key-int-max={k} threads=2 ! video/x-h264,profile=high"


GST_ELEMENTS = {"nvenc": "nvh264enc", "vaapi": "vah264enc", "x264": "x264enc"}


def gst_cmd(cfg, enc, node, fd):
    """gst-launch-1.0 pipeline: screen (+ sound) → H.264 / AAC → MPEG-TS on stdout."""
    fps = int(cfg["fps"])
    q = C.REC_QUALITY.get(cfg["rec_quality"], 23)
    # the compositor only sends a frame when something changes (and keepalive-time once a second);
    # videorate turns that into a steady frame rate. Late frames are dropped before it, never after.
    v = (f"pipewiresrc fd={fd} path={node} do-timestamp=true keepalive-time=1000 ! "
         f"queue max-size-buffers=3 leaky=downstream ! videorate ! video/x-raw,framerate={fps}/1 ! "
         f"{_gst_encoder(enc, fps, q)} ! h264parse config-interval=-1 ! queue max-size-time=3000000000 ! mux.")
    audio = []
    if cfg["system_audio"]:
        audio.append((cfg["system_device"] or "@DEFAULT_MONITOR@", cfg["system_volume"]))
    if cfg["mic"]:
        audio.append((cfg["mic_device"] or "@DEFAULT_SOURCE@", cfg["mic_volume"]))
    a = ""
    if audio:
        a = " audiomixer name=am ! audioconvert ! avenc_aac bitrate=160000 ! aacparse ! queue ! mux."
        for dev, vol in audio:
            a += (f" pulsesrc device={dev} do-timestamp=true provide-clock=false client-name=drzreplay ! "
                  f"audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! volume volume={vol / 100:.2f} "
                  f"! queue ! am.")
    pipeline = f"mpegtsmux name=mux alignment=7 ! fdsink fd=1 sync=false {v}{a}"
    return ["gst-launch-1.0", "-q", "-e"] + pipeline.split()


def gst_has(element):
    from ..core import system
    r = system.run(["gst-inspect-1.0", "--exists", element], timeout=10)
    return bool(r) and r.returncode == 0


def gst_encoders():
    """Encoders GStreamer can use here, best first."""
    from ..core import system
    out = []
    for enc in ("nvenc", "vaapi", "x264"):
        if not gst_has(GST_ELEMENTS[enc]):
            continue
        test = (f"videotestsrc num-buffers=10 ! video/x-raw,width=640,height=360,framerate=30/1 ! "
                f"{_gst_encoder(enc, 30, 23)} ! fakesink")
        r = system.run(["gst-launch-1.0", "-q"] + test.split(), timeout=20)
        if r and r.returncode == 0:
            out.append(enc)
    return out


def missing_parts():
    """Packages needed for Wayland capture that aren't installed, for the error message."""
    need = []
    if not gst_has("pipewiresrc"):
        need.append("gstreamer1.0-pipewire")
    if not gst_has("pulsesrc"):
        need.append("gstreamer1.0-plugins-good")
    if not gst_has("avenc_aac"):
        need.append("gstreamer1.0-libav")
    return need
