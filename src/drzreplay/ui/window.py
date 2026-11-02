"""DrzReplay settings window (GTK 4 + libadwaita): Record, Clips and Settings tabs."""
import glob
import hashlib
import os
import queue
import signal
import subprocess
import sys
import threading
import time
from datetime import datetime, timedelta

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, Gio, GLib, GObject, Gtk  # noqa: E402

from ..core import autostart  # noqa: E402
from ..core import config as C  # noqa: E402
from ..core import system  # noqa: E402
from ..recorder.hotkey import pretty  # noqa: E402

FPS = [(24, "24"), (30, "30"), (60, "60")]
QUALITY = [("low", "Low"), ("balanced", "Medium"), ("high", "High"), ("ultra", "Maximum")]
RESOLUTION = [(0, "Original"), (1080, "1080p"), (720, "720p")]
FORMAT = [("mp4", "MP4"), ("mkv", "MKV")]
MODIFIER_KEYS = {"Shift_L", "Shift_R", "Control_L", "Control_R", "Alt_L", "Alt_R", "Super_L", "Super_R",
                 "Meta_L", "Meta_R", "ISO_Level3_Shift", "Caps_Lock", "Num_Lock", "Hyper_L", "Hyper_R"}
SOLO_KEYS = {"Pause", "Scroll_Lock", "Print", "Insert"}
MAX_CLIPS_SHOWN = 60
THUMBS = os.path.join(C.CACHE_DIR, "thumbs")

CSS = b"""
.thumb { border-radius: 6px; background: alpha(currentColor, 0.08); }
.rec-on { color: #e01b24; }
.save-hint { margin-top: 6px; }
splitbutton.save > button { min-height: 40px; padding: 0 26px; font-weight: bold; }
splitbutton.save > menubutton > button { min-height: 40px; }
"""


def human_size(n):
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.0f} {unit}" if unit in ("B", "KB") else f"{n:.1f} {unit}"
        n /= 1024


def human_time(sec):
    sec = int(round(sec))
    return f"{sec // 60}:{sec % 60:02d}"


def tilde(path):
    home = os.path.expanduser("~")
    return "~" + path[len(home):] if path == home or path.startswith(home + "/") else path


def human_date(ts):
    d = datetime.fromtimestamp(ts)
    today = datetime.now().date()
    if d.date() == today:
        return f"Today, {d:%H:%M}"
    if d.date() == today - timedelta(days=1):
        return f"Yesterday, {d:%H:%M}"
    return f"{d:%b} {d.day}, {d:%H:%M}"


def file_provider(path):
    """Clipboard / drag-and-drop content for a file: drop it into Discord, a browser, a file manager."""
    files = Gdk.FileList.new_from_list([Gio.File.new_for_path(path)])
    return Gdk.ContentProvider.new_for_value(GObject.Value(Gdk.FileList, files))


class ThumbLoader:
    """Makes clip thumbnails and reads durations in one background thread, cached on disk."""

    def __init__(self):
        self.q = queue.Queue()
        os.makedirs(THUMBS, exist_ok=True)
        threading.Thread(target=self.work, daemon=True).start()

    def request(self, path, cb):
        self.q.put((path, cb))

    def work(self):
        while True:
            path, cb = self.q.get()
            try:
                st = os.stat(path)
            except OSError:
                continue
            key = hashlib.sha1(f"{path}|{st.st_mtime}|{st.st_size}".encode()).hexdigest()
            jpg, meta = os.path.join(THUMBS, key + ".jpg"), os.path.join(THUMBS, key + ".json")
            dur = C.read_json(meta, {}).get("duration")
            if dur is None:
                r = system.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", path])
                try:
                    dur = float(r.stdout.strip())
                except (AttributeError, ValueError):
                    dur = 0.0
                C.write_json(meta, {"duration": dur})
            if not os.path.exists(jpg):
                system.run(["nice", "-n", "15", "ffmpeg", "-v", "error", "-y", "-ss", f"{min(1.0, dur / 2):.2f}",
                            "-i", path, "-frames:v", "1", "-q:v", "3", "-vf",
                            "scale=96:54:force_original_aspect_ratio=increase,crop=96:54", jpg], timeout=20)
            GLib.idle_add(cb, jpg if os.path.exists(jpg) else None, dur)


class Window(Adw.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title=C.APP_NAME)
        self.set_default_size(640, 800)
        self.first_run = not C.exists()
        self.cfg = C.load()
        self._conf_mtime = self.conf_mtime()
        self._flush_id = None
        self._dirty = set()
        self._busy = False
        self.rows = {}
        self.enc_info = system.detect_encoders()
        self.thumbs = ThumbLoader()
        self._clips_sig = None
        self.latest = None

        css = Gtk.CssProvider()
        css.load_from_data(CSS, -1)
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), css,
                                                  Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)

        self.stack = Adw.ViewStack()
        header = Adw.HeaderBar()
        header.set_title_widget(Adw.ViewSwitcher(stack=self.stack, policy=Adw.ViewSwitcherPolicy.WIDE))
        menu = Gio.Menu()
        menu.append("Recorder log", "app.open-log")
        menu.append("About DrzReplay", "app.about")
        header.pack_end(Gtk.MenuButton(icon_name="open-menu-symbolic", menu_model=menu, tooltip_text="Menu"))

        tv = Adw.ToolbarView()
        tv.add_top_bar(header)
        if system.is_wayland():
            tv.add_top_bar(Adw.Banner(revealed=True, title="Screen capture needs an Xorg session"))
        self.update_banner = Adw.Banner(button_label="Update")
        self.update_banner.connect("button-clicked", self.run_update)
        tv.add_top_bar(self.update_banner)

        self.stack.add_titled_with_icon(self.page_record(), "record", "Record", "media-record-symbolic")
        self.stack.add_titled_with_icon(self.page_clips(), "clips", "Clips", "folder-videos-symbolic")
        self.stack.add_titled_with_icon(self.page_settings(), "settings", "Settings", "emblem-system-symbolic")
        self.toasts = Adw.ToastOverlay(child=self.stack)
        tv.set_content(self.toasts)
        self.set_content(tv)

        for name, cb, ptype in (("save", lambda *_: self.on_save(), None),
                                ("save-short", lambda _a, v: self.on_save(v.get_int32()), GLib.VariantType.new("i"))):
            a = Gio.SimpleAction.new(name, ptype)
            a.connect("activate", cb)
            self.add_action(a)

        self.update_sensitivity()
        self.poll()
        GLib.timeout_add(1000, self.poll)
        self.check_updates(manual=False)
        if self.first_run:
            GLib.idle_add(self.first_run_setup)
        self.connect("close-request", self.on_close)

    def on_close(self, *_):
        # a change made right before closing must not be lost to the save delay
        if self._flush_id:
            GLib.source_remove(self._flush_id)
            self.flush()
        return False

    # ------------------------------------------------------------------ settings plumbing
    def toast(self, text, button=None, cb=None):
        t = Adw.Toast.new(text)
        if button:
            t.set_button_label(button)
            t.connect("button-clicked", lambda *_: cb())
        self.toasts.add_toast(t)

    def conf_mtime(self):
        try:
            return os.path.getmtime(C.CONF_PATH)
        except OSError:
            return 0

    def set(self, key, value):
        if self._busy or self.cfg.get(key) == value:
            return
        self.cfg[key] = value
        self._dirty.add(key)
        if self._flush_id:
            GLib.source_remove(self._flush_id)
        self._flush_id = GLib.timeout_add(400, self.flush)
        self.update_sensitivity()

    def flush(self):
        self._flush_id = None
        # write only what was changed here, the tray may have changed other keys meanwhile
        cfg = C.load()
        for k in self._dirty:
            cfg[k] = self.cfg[k]
        self._dirty.clear()
        C.save(cfg)
        self.cfg = cfg
        self._conf_mtime = self.conf_mtime()
        pid = C.daemon_pid()
        if pid:
            os.kill(pid, signal.SIGHUP)
        return False

    def reload_external(self):
        """The config changed outside this window (tray menu, CLI): show the new values."""
        self.cfg = C.load()
        self._busy = True
        for key, (kind, row, values) in self.rows.items():
            v = self.cfg[key]
            if kind == "switch":
                row.set_active(bool(v))
            elif kind == "combo" and v in values:
                row.set_selected(values.index(v))
            elif kind in ("spin", "scale"):
                (row if kind == "spin" else row.scale).set_value(v)
        self._busy = False
        self.update_sensitivity()

    def switch(self, title, key, subtitle=None):
        row = Adw.SwitchRow(title=title, subtitle=subtitle, active=bool(self.cfg[key]))
        row.connect("notify::active", lambda r, _: self.set(key, r.get_active()))
        self.rows[key] = ("switch", row, None)
        return row

    def combo(self, title, key, options, subtitle=None):
        values = [v for v, _ in options]
        labels = [l for _, l in options]
        if self.cfg[key] not in values:
            values.append(self.cfg[key])
            labels.append(str(self.cfg[key]))
        row = Adw.ComboRow(title=title, subtitle=subtitle, model=Gtk.StringList.new(labels))
        row.set_selected(values.index(self.cfg[key]))
        row.connect("notify::selected", lambda r, _: self.set(key, values[r.get_selected()]))
        self.rows[key] = ("combo", row, values)
        return row

    def spin(self, title, subtitle, key, lo, hi, step, digits=0):
        adj = Gtk.Adjustment(value=self.cfg[key], lower=lo, upper=hi, step_increment=step, page_increment=step * 4)
        row = Adw.SpinRow(title=title, subtitle=subtitle, adjustment=adj, digits=digits)
        conv = float if digits else int
        row.connect("notify::value", lambda r, _: self.set(key, conv(r.get_value())))
        self.rows[key] = ("spin", row, None)
        return row

    def scale(self, title, key, lo, hi, unit="%"):
        row = Adw.ActionRow(title=title)
        s = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, lo, hi, 5)
        s.set_value(self.cfg[key])
        s.set_size_request(200, -1)
        s.set_draw_value(True)
        s.set_value_pos(Gtk.PositionType.LEFT)
        s.set_format_value_func(lambda _s, v: f"{v:.0f}{unit}")
        s.add_mark(100, Gtk.PositionType.BOTTOM, None)
        s.connect("value-changed", lambda w: self.set(key, int(w.get_value())))
        row.add_suffix(s)
        row.scale = s
        self.rows[key] = ("scale", row, None)
        return row

    def button(self, label=None, icon=None, cb=None, tip=None, flat=True):
        b = Gtk.Button(valign=Gtk.Align.CENTER, tooltip_text=tip)
        if icon:
            b.set_icon_name(icon)
            if flat:
                b.add_css_class("flat")
        else:
            b.set_label(label)
        if cb:
            b.connect("clicked", cb)
        return b

    # ------------------------------------------------------------------ Record tab
    def page_record(self):
        page = Adw.PreferencesPage()

        g = Adw.PreferencesGroup()
        self.rec_row = Adw.ActionRow(title="Recording")
        self.rec_dot = Gtk.Image(icon_name="media-record-symbolic")
        self.rec_row.add_prefix(self.rec_dot)
        self.rec_switch = Gtk.Switch(valign=Gtk.Align.CENTER)
        self.rec_switch.connect("notify::active", self.on_rec_switch)
        self.rec_row.add_suffix(self.rec_switch)
        self.rec_row.set_activatable_widget(self.rec_switch)
        g.add(self.rec_row)

        self.buf_row = Adw.ActionRow(title="Buffer")
        self.buf_bar = Gtk.ProgressBar(valign=Gtk.Align.CENTER)
        self.buf_bar.set_size_request(160, -1)
        self.buf_row.add_suffix(self.buf_bar)
        g.add(self.buf_row)
        page.add(g)

        g = Adw.PreferencesGroup()
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, halign=Gtk.Align.CENTER, margin_top=6)
        short = Gio.Menu()
        for sec in (15, 30):
            short.append(f"Save last {sec} s", f"win.save-short({sec})")
        self.save_btn = Adw.SplitButton(label="Save replay", menu_model=short, action_name="win.save")
        self.save_btn.add_css_class("suggested-action")
        self.save_btn.add_css_class("save")
        box.append(self.save_btn)
        self.save_hint = Gtk.Label(css_classes=["dim-label", "save-hint"])
        box.append(self.save_hint)
        g.add(box)
        page.add(g)

        g = Adw.PreferencesGroup(title="Video")
        g.add(self.spin("Length", "Seconds kept in memory", "seconds", 10, 600, 5))
        screens = [("all", "All screens")] + [(m["name"], f"{m['name']} ({m['w']}×{m['h']})")
                                              for m in system.monitors()]
        if len(screens) > 2:
            g.add(self.combo("Screen", "monitor", screens))
        g.add(self.combo("Frame rate", "fps", FPS))
        g.add(self.combo("Quality", "rec_quality", QUALITY))
        encs = [("auto", "Auto")] + [(e, system.ENCODER_LABELS[e]) for e in self.enc_info["available"]]
        g.add(self.combo("Encoder", "encoder", encs))
        g.add(self.switch("Show cursor", "draw_mouse"))
        page.add(g)

        g = Adw.PreferencesGroup(title="Sound")
        g.add(self.switch("System sound", "system_audio", "What you hear"))
        self.out_row = self.combo("Output", "system_device", [("@DEFAULT_MONITOR@", "Default")] + system.audio_outputs())
        g.add(self.out_row)
        self.out_vol = self.scale("Volume", "system_volume", 0, 200)
        g.add(self.out_vol)
        g.add(self.switch("Microphone", "mic"))
        self.mic_row = self.combo("Input", "mic_device", [("@DEFAULT_SOURCE@", "Default")] + system.audio_inputs())
        g.add(self.mic_row)
        self.mic_vol = self.scale("Volume", "mic_volume", 0, 200)
        g.add(self.mic_vol)
        page.add(g)
        return page

    # ------------------------------------------------------------------ Clips tab
    def page_clips(self):
        page = Adw.PreferencesPage()
        self.clips_group = Adw.PreferencesGroup(title="Saved clips")
        hs = Gtk.Box(spacing=6)
        hs.append(self.button(icon="view-refresh-symbolic", tip="Refresh", cb=lambda *_: self.load_clips(True)))
        hs.append(self.button(icon="folder-open-symbolic", tip="Open folder", cb=lambda *_: self.open_dir()))
        self.clips_group.set_header_suffix(hs)
        self.clips_list = Gtk.ListBox(selection_mode=Gtk.SelectionMode.NONE, css_classes=["boxed-list"])
        self.clips_group.add(self.clips_list)
        self.clips_more = Gtk.Label(css_classes=["dim-label"], margin_top=10, visible=False)
        self.clips_group.add(self.clips_more)
        page.add(self.clips_group)

        self.clips_empty = Adw.StatusPage(icon_name="folder-videos-symbolic", title="No clips yet")
        self.clips_stack = Gtk.Stack()
        self.clips_stack.add_named(page, "list")
        self.clips_stack.add_named(self.clips_empty, "empty")
        return self.clips_stack

    def clip_files(self):
        d = C.output_dir(self.cfg)
        files = [p for p in glob.glob(os.path.join(glob.escape(d), "*"))
                 if p.lower().endswith((".mp4", ".mkv")) and not os.path.basename(p).startswith(".")]
        out = []
        for p in files:
            try:
                st = os.stat(p)
                out.append((st.st_mtime, st.st_size, p))
            except OSError:
                pass
        return sorted(out, reverse=True)

    def load_clips(self, force=False):
        clips = self.clip_files()
        sig = (C.output_dir(self.cfg), tuple((p, m, s) for m, s, p in clips[:MAX_CLIPS_SHOWN]))
        if sig == self._clips_sig and not force:
            return
        self._clips_sig = sig
        total = sum(s for _, s, _ in clips)
        self.clips_group.set_description(f"{len(clips)} clip{'s' * (len(clips) != 1)} · {human_size(total)}"
                                         if clips else None)
        self.clips_empty.set_description(f"Press {pretty(self.cfg['hotkey'])} to save the last "
                                         f"{self.cfg['seconds']} seconds")
        self.clips_stack.set_visible_child_name("list" if clips else "empty")
        while (child := self.clips_list.get_first_child()) is not None:
            self.clips_list.remove(child)
        for mtime, size, path in clips[:MAX_CLIPS_SHOWN]:
            self.clips_list.append(self.clip_row(path, mtime, size))
        hidden = len(clips) - MAX_CLIPS_SHOWN
        self.clips_more.set_visible(hidden > 0)
        if hidden > 0:
            self.clips_more.set_label(f"{hidden} older clip{'s' * (hidden != 1)} in the folder")

    def clip_row(self, path, mtime, size):
        row = Adw.ActionRow(title=human_date(mtime), subtitle=human_size(size), activatable=True,
                            tooltip_text=os.path.basename(path))
        pic = Gtk.Picture(content_fit=Gtk.ContentFit.COVER, css_classes=["thumb"], valign=Gtk.Align.CENTER,
                          margin_top=6, margin_bottom=6, can_shrink=True)
        pic.set_size_request(96, 54)
        pic.set_overflow(Gtk.Overflow.HIDDEN)
        row.add_prefix(pic)

        def loaded(jpg, dur):
            if jpg:
                pic.set_filename(jpg)
            if dur:
                row.set_subtitle(f"{human_time(dur)} · {human_size(size)}")
            return False

        self.thumbs.request(path, loaded)
        row.connect("activated", lambda *_: self.open_uri(path))
        row.add_suffix(self.button(icon="edit-copy-symbolic", tip="Copy (paste into Discord, Telegram…)",
                                   cb=lambda *_: self.copy_clip(path)))
        row.add_suffix(self.button(icon="folder-open-symbolic", tip="Show in folder",
                                   cb=lambda *_: self.show_in_folder(path)))
        row.add_suffix(self.button(icon="user-trash-symbolic", tip="Move to trash",
                                   cb=lambda *_: self.trash_clip(path)))
        drag = Gtk.DragSource(actions=Gdk.DragAction.COPY)
        drag.connect("prepare", lambda *_: file_provider(path))
        drag.connect("drag-begin", lambda src, _d: src.set_icon(Gtk.WidgetPaintable.new(pic), 0, 0))
        row.add_controller(drag)
        return row

    def copy_clip(self, path):
        self.get_clipboard().set_content(file_provider(path))
        self.toast("Copied — paste it into a chat")

    def show_in_folder(self, path):
        from ..recorder.save import show_in_folder
        show_in_folder(path)

    def trash_clip(self, path):
        try:
            Gio.File.new_for_path(path).trash(None)
            self.toast("Moved to trash")
        except GLib.Error:
            # no trash on this drive (USB stick, tmpfs…): offer to delete for good
            self.toast("This drive has no trash", "Delete", lambda: (os.remove(path), self.load_clips(True)))
        self.load_clips(True)

    def open_uri(self, path):
        Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(path).get_uri(), None)

    # ------------------------------------------------------------------ Settings tab
    def page_settings(self):
        page = Adw.PreferencesPage()

        g = Adw.PreferencesGroup(title="Saving")
        self.dir_row = Adw.ActionRow(title="Folder", subtitle=tilde(C.output_dir(self.cfg)))
        self.dir_row.set_subtitle_lines(1)
        self.dir_row.add_suffix(self.button(icon="folder-open-symbolic", tip="Open",
                                            cb=lambda *_: self.open_dir()))
        self.dir_row.add_suffix(self.button("Change", cb=self.pick_folder))
        g.add(self.dir_row)
        prefix = Adw.EntryRow(title="File name", text=self.cfg["file_prefix"], show_apply_button=True)
        prefix.connect("apply", lambda r: self.set("file_prefix", r.get_text().strip().replace("/", "-")
                                                   or C.APP_NAME))
        g.add(prefix)
        g.add(self.combo("Format", "container", FORMAT))
        g.add(self.combo("Resolution", "export_height", RESOLUTION))
        g.add(self.combo("Quality", "export_quality", QUALITY, "Of the saved file"))
        page.add(g)

        g = Adw.PreferencesGroup(title="Cleanup", description="Oldest clips are deleted when a limit is reached")
        g.add(self.spin("Keep clips", "0 = keep all", "keep_max_clips", 0, 9999, 1))
        g.add(self.spin("Max space, GB", "0 = no limit", "keep_max_gb", 0, 2000, 1))
        page.add(g)

        g = Adw.PreferencesGroup(title="Controls")
        self.hk_row = Adw.ActionRow(title="Hotkey")
        self.hk_label = Gtk.Label(label=pretty(self.cfg["hotkey"]), valign=Gtk.Align.CENTER)
        self.hk_label.add_css_class("dim-label")
        self.hk_row.add_suffix(self.hk_label)
        self.hk_row.add_suffix(self.button("Change", cb=self.change_hotkey))
        g.add(self.hk_row)
        g.add(self.switch("Click sound", "click_sound", "When a replay is being saved"))
        g.add(self.switch("Notifications", "notifications"))
        page.add(g)

        g = Adw.PreferencesGroup(title="Intro", description="Animated metal gate before each replay")
        intro = self.switch("Intro before replay", "intro")
        intro.add_suffix(self.button(icon="media-playback-start-symbolic", tip="Preview", cb=self.preview_intro))
        g.add(intro)
        self.intro_vol = self.scale("Intro volume", "intro_volume", 0, 200)
        g.add(self.intro_vol)
        page.add(g)

        g = Adw.PreferencesGroup(title="Startup")
        self.auto_row = Adw.SwitchRow(title="Start with computer", active=autostart.enabled())
        self.auto_row.connect("notify::active", lambda r, _: autostart.set_enabled(r.get_active()))
        g.add(self.auto_row)
        g.add(self.switch("Tray icon", "tray"))
        page.add(g)

        g = Adw.PreferencesGroup(title="Updates")
        self.ver_row = Adw.ActionRow(title=f"{C.APP_NAME} {C.VERSION}")
        self.ver_btn = self.button("Check", cb=self.on_ver_btn)
        self.ver_row.add_suffix(self.ver_btn)
        g.add(self.ver_row)
        page.add(g)
        return page

    def update_sensitivity(self):
        if hasattr(self, "mic_row"):
            for w in (self.out_row, self.out_vol):
                w.set_sensitive(self.cfg["system_audio"])
            for w in (self.mic_row, self.mic_vol):
                w.set_sensitive(self.cfg["mic"])
        if hasattr(self, "intro_vol"):
            self.intro_vol.set_sensitive(self.cfg["intro"])
        if hasattr(self, "save_hint"):
            self.save_hint.set_label(f"or press {pretty(self.cfg['hotkey'])}")

    # ------------------------------------------------------------------ status
    def poll(self):
        if not self._flush_id and self.conf_mtime() != self._conf_mtime:
            self._conf_mtime = self.conf_mtime()
            self.reload_external()
        pid = C.daemon_pid()
        st = C.read_json(C.STATUSFILE, {}) if pid else {}
        state = st.get("state") if pid else None
        self._busy = True
        self.rec_switch.set_active(state in ("recording", "starting"))
        self._busy = False
        recording = state == "recording"
        if recording:
            self.rec_dot.add_css_class("rec-on")
        else:
            self.rec_dot.remove_css_class("rec-on")

        if not pid:
            self.rec_row.set_subtitle("Off")
        elif recording:
            g = st.get("geometry") or [0, 0]
            enc = system.ENCODER_LABELS.get(st.get("encoder"), st.get("encoder") or "")
            extra = " · no sound" if st.get("audio_off") else ""
            self.rec_row.set_subtitle(f"{g[0]}×{g[1]} · {st.get('fps')} fps · {enc}{extra}")
        elif state == "paused":
            self.rec_row.set_subtitle("Paused")
        elif state == "error":
            self.rec_row.set_subtitle(st.get("error") or "Error")
        else:
            self.rec_row.set_subtitle("Starting…")

        secs = float(st.get("seconds") or self.cfg["seconds"])
        buffered = float(st.get("buffered", 0)) if recording else 0.0
        self.buf_bar.set_fraction(min(1.0, buffered / secs) if secs else 0)
        self.buf_row.set_subtitle(f"{buffered:.0f} of {secs:.0f} s · {st.get('buffer_mb', 0):.0f} MB in memory"
                                  if recording else "Empty")
        self.save_btn.set_sensitive(recording and buffered >= 1)

        if pid and st.get("hotkey") == self.cfg["hotkey"]:
            if st.get("hotkey_ok"):
                self.hk_row.set_subtitle("")
            elif st.get("hotkey_error") not in (None, "", "starting"):
                self.hk_row.set_subtitle(f"Not working: {st['hotkey_error']}")
        self.load_clips()
        return True

    def on_rec_switch(self, sw, _):
        if self._busy:
            return
        pid = C.daemon_pid()
        st = C.read_json(C.STATUSFILE, {}) if pid else {}
        if sw.get_active():
            if not pid:
                self.start_daemon()
            elif st.get("state") in ("paused", "error"):
                self.start_daemon("restart")
        elif pid:
            os.kill(pid, signal.SIGTERM)

    def start_daemon(self, cmd="start"):
        if self._flush_id:
            GLib.source_remove(self._flush_id)
            self.flush()
        subprocess.Popen(C.launcher() + [cmd], start_new_session=True,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def on_save(self, seconds=None):
        extra = ["--seconds", str(seconds)] if seconds else []
        subprocess.Popen(C.launcher() + ["save"] + extra, start_new_session=True,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.toast("Saving…" if not seconds else f"Saving the last {seconds} s…")

    def open_dir(self):
        d = C.output_dir(self.cfg)
        os.makedirs(d, exist_ok=True)
        Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(d).get_uri(), None)

    def pick_folder(self, *_):
        dlg = Gtk.FileDialog(title="Folder")
        dlg.set_initial_folder(Gio.File.new_for_path(C.output_dir(self.cfg)))

        def done(d, res):
            try:
                f = d.select_folder_finish(res)
            except GLib.Error:
                return
            if f and f.get_path():
                self.set("output_dir", f.get_path())
                self.dir_row.set_subtitle(tilde(f.get_path()))
                self.load_clips(True)

        dlg.select_folder(self, None, done)

    def change_hotkey(self, *_):
        dlg = Adw.AlertDialog(heading="Press a new hotkey", body="Esc to cancel")
        dlg.add_response("cancel", "Cancel")
        ctl = Gtk.EventControllerKey()
        ctl.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)

        def on_key(_c, keyval, code, state):
            mods = state & Gtk.accelerator_get_default_mod_mask()
            # read the key in the first (Latin) layout, so Alt+Я is saved as Alt+Z
            ok, base, *_ = self.get_display().translate_key(code, 0, 0)
            if ok and base:
                keyval = base
            name = Gdk.keyval_name(keyval) or ""
            if name in MODIFIER_KEYS:
                return True
            if keyval == Gdk.KEY_Escape and not mods:
                dlg.close()
                return True
            if not mods and name not in SOLO_KEYS and not (name.startswith("F") and name[1:].isdigit()):
                dlg.set_body("Use it with Ctrl, Alt, Shift or Super")
                return True
            accel = Gtk.accelerator_name(Gdk.keyval_to_lower(keyval), mods)
            self.set("hotkey", accel)
            self.hk_label.set_label(pretty(accel))
            self.hk_row.set_subtitle("")
            dlg.close()
            return True

        ctl.connect("key-pressed", on_key)
        dlg.add_controller(ctl)
        dlg.present(self)

    def preview_intro(self, *_):
        if self._flush_id:
            GLib.source_remove(self._flush_id)
            self.flush()
        subprocess.Popen(C.launcher() + ["preview-intro"], start_new_session=True,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.toast("Rendering the preview…")

    # ------------------------------------------------------------------ updates
    def check_updates(self, manual):
        if not manual:
            cached = C.read_json(C.UPDATE_CACHE, {})
            if time.time() - cached.get("time", 0) < 6 * 3600:
                self.show_update(cached.get("latest"), manual)
                return
        self.ver_btn.set_sensitive(False)
        self.ver_row.set_subtitle("Checking…")

        def work():
            latest = system.latest_version()
            if latest:
                C.write_json(C.UPDATE_CACHE, {**C.read_json(C.UPDATE_CACHE, {}), "time": time.time(), "latest": latest})
            GLib.idle_add(self.show_update, latest, manual, True)

        threading.Thread(target=work, daemon=True).start()

    def show_update(self, latest, _manual, fresh=False):
        self.ver_btn.set_sensitive(True)
        self.latest = latest
        newer = latest and system.version_tuple(latest) > system.version_tuple(C.VERSION)
        if newer:
            self.ver_row.set_subtitle(f"Version {latest} is available")
            self.ver_btn.set_label("Update")
            self.update_banner.set_title(f"{C.APP_NAME} {latest} is available")
            self.update_banner.set_revealed(True)
        elif latest:
            self.ver_row.set_subtitle("Up to date")
        else:
            self.ver_row.set_subtitle("Couldn't check (offline?)" if fresh else "")
        return False

    def on_ver_btn(self, *_):
        if self.latest and system.version_tuple(self.latest) > system.version_tuple(C.VERSION):
            self.run_update()
        else:
            self.check_updates(manual=True)

    def run_update(self, *_):
        if not system.run_in_terminal(C.INSTALL_CMD):
            self.toast("No terminal found. Run: " + C.INSTALL_CMD)
            return
        self.toast("Follow the terminal, then reopen this window")

    def first_run_setup(self):
        C.save(self.cfg)
        self._conf_mtime = self.conf_mtime()
        autostart.set_enabled(True)
        self.auto_row.set_active(True)
        if not C.daemon_pid() and not system.is_wayland():
            self.start_daemon()
        self.toast(f"Recording. Press {pretty(self.cfg['hotkey'])} to save a replay.")
        return False


class App(Adw.Application):
    def __init__(self):
        flags = Gio.ApplicationFlags.DEFAULT_FLAGS
        if os.environ.get("DRZREPLAY_NON_UNIQUE"):
            flags |= Gio.ApplicationFlags.NON_UNIQUE
        super().__init__(application_id=C.APP_ID, flags=flags)
        self.win = None

    def do_startup(self):
        Adw.Application.do_startup(self)
        disp = Gdk.Display.get_default()
        Gtk.IconTheme.get_for_display(disp).add_search_path(os.path.join(C.ASSETS, "icons"))
        Gtk.Window.set_default_icon_name("drzreplay")
        for name, cb in (("about", self.on_about), ("open-log", self.on_open_log)):
            a = Gio.SimpleAction.new(name, None)
            a.connect("activate", cb)
            self.add_action(a)
        self.set_accels_for_action("window.close", ["<Control>w", "<Control>q"])

    def do_activate(self):
        if not self.win:
            self.win = Window(self)
            if autostart.is_i3():
                self.win.connect("map", lambda w: GLib.timeout_add(150, self.float_on_i3))
        self.win.present()

    def float_on_i3(self):
        """i3 would tile the settings window across the whole screen; float it like a dialog."""
        try:
            gi.require_version("GdkX11", "4.0")
            from gi.repository import GdkX11  # noqa: F401  (gives the surface its get_xid)
            xid = self.win.get_surface().get_xid()
            subprocess.run(["i3-msg", f"[id={xid}] floating enable, move position center"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3)
        except Exception:
            pass
        return False

    def on_open_log(self, *_):
        if os.path.exists(C.FFMPEG_LOG):
            Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(C.FFMPEG_LOG).get_uri(), None)

    def on_about(self, *_):
        about = Adw.AboutDialog(application_name=C.APP_NAME, application_icon="drzreplay", version=C.VERSION,
                                developer_name="DRZ", license_type=Gtk.License.MIT_X11,
                                website=f"https://github.com/{C.REPO}",
                                issue_url=f"https://github.com/{C.REPO}/issues")
        about.add_legal_section("Sounds", None, Gtk.License.CUSTOM,
                                "Sci-Fi Pack Vol 1 by JC Sounds, CC BY 4.0\n"
                                "https://opengameart.org/content/jc-sounds-sci-fi-pack-vol-1\n"
                                "Mechanical Sounds by Brian MacIntosh, CC0\nIron Door, CC0")
        about.add_legal_section("Font", None, Gtk.License.CUSTOM, "Oswald, SIL Open Font License 1.1")
        about.present(self.win)


def main(args):
    GLib.set_prgname("drzreplay")    # window class, otherwise it is "__main__.py"
    GLib.set_application_name(C.APP_NAME)
    return App().run([sys.argv[0]])
