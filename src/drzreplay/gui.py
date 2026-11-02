"""DrzReplay settings window (GTK 4 + libadwaita)."""
import os
import signal
import subprocess
import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, Gio, GLib, Gtk  # noqa: E402

from . import autostart  # noqa: E402
from . import config as C  # noqa: E402
from . import system  # noqa: E402
from .hotkey import pretty  # noqa: E402

FPS = [(24, "24"), (30, "30"), (60, "60")]
QUALITY = [("low", "Low"), ("balanced", "Medium"), ("high", "High"), ("ultra", "Maximum")]
RESOLUTION = [(0, "Original"), (1080, "1080p"), (720, "720p")]
FORMAT = [("mp4", "MP4"), ("mkv", "MKV")]
MODIFIER_KEYS = {"Shift_L", "Shift_R", "Control_L", "Control_R", "Alt_L", "Alt_R", "Super_L", "Super_R",
                 "Meta_L", "Meta_R", "ISO_Level3_Shift", "Caps_Lock", "Num_Lock", "Hyper_L", "Hyper_R"}
SOLO_KEYS = {"Pause", "Scroll_Lock", "Print", "Insert"}


class Window(Adw.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title=C.APP_NAME)
        self.set_default_size(620, 780)
        self.first_run = not C.exists()
        self.cfg = C.load()
        self._flush_id = None
        self._dirty = set()
        self._busy = False
        self.enc_info = system.detect_encoders()

        self.toasts = Adw.ToastOverlay()
        header = Adw.HeaderBar()
        menu = Gio.Menu()
        menu.append("Recorder log", "app.open-log")
        menu.append("About DrzReplay", "app.about")
        header.pack_end(Gtk.MenuButton(icon_name="open-menu-symbolic", menu_model=menu))
        tv = Adw.ToolbarView()
        tv.add_top_bar(header)
        if system.is_wayland():
            tv.add_top_bar(Adw.Banner(revealed=True, title="Screen capture needs an Xorg session"))
        page = Adw.PreferencesPage()
        self.toasts.set_child(page)
        tv.set_content(self.toasts)
        self.set_content(tv)

        page.add(self.group_main())
        page.add(self.group_video())
        page.add(self.group_sound())
        page.add(self.group_saving())
        page.add(self.group_other())

        self.update_sensitivity()
        self.poll()
        GLib.timeout_add(1000, self.poll)
        if self.first_run:
            GLib.idle_add(self.first_run_setup)
        self.connect("close-request", self.on_close)

    def on_close(self, *_):
        # a change made right before closing must not be lost to the save delay
        if self._flush_id:
            GLib.source_remove(self._flush_id)
            self.flush()
        return False

    # ------------------------------------------------------------------ helpers
    def toast(self, text):
        self.toasts.add_toast(Adw.Toast.new(text))

    def set(self, key, value):
        if self.cfg.get(key) == value:
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
        pid = C.daemon_pid()
        if pid:
            os.kill(pid, signal.SIGHUP)
        return False

    def switch(self, title, key):
        row = Adw.SwitchRow(title=title, active=bool(self.cfg[key]))
        row.connect("notify::active", lambda r, _: self.set(key, r.get_active()))
        return row

    def combo(self, title, key, options):
        values = [v for v, _ in options]
        labels = [l for _, l in options]
        if self.cfg[key] not in values:
            values.append(self.cfg[key])
            labels.append(str(self.cfg[key]))
        row = Adw.ComboRow(title=title, model=Gtk.StringList.new(labels))
        row.set_selected(values.index(self.cfg[key]))
        row.connect("notify::selected", lambda r, _: self.set(key, values[r.get_selected()]))
        return row

    def spin(self, title, subtitle, key, lo, hi, step):
        adj = Gtk.Adjustment(value=self.cfg[key], lower=lo, upper=hi, step_increment=step, page_increment=step * 4)
        row = Adw.SpinRow(title=title, subtitle=subtitle, adjustment=adj)
        row.connect("notify::value", lambda r, _: self.set(key, int(r.get_value())))
        return row

    def button(self, label=None, icon=None, cb=None, tip=None):
        b = Gtk.Button(valign=Gtk.Align.CENTER, tooltip_text=tip)
        if icon:
            b.set_icon_name(icon)
            b.add_css_class("flat")
        else:
            b.set_label(label)
        if cb:
            b.connect("clicked", cb)
        return b

    # ------------------------------------------------------------------ groups
    def group_main(self):
        g = Adw.PreferencesGroup()

        self.rec_row = Adw.ActionRow(title="Recording")
        self.rec_switch = Gtk.Switch(valign=Gtk.Align.CENTER)
        self.rec_switch.connect("notify::active", self.on_rec_switch)
        self.rec_row.add_suffix(self.rec_switch)
        self.rec_row.set_activatable_widget(self.rec_switch)
        g.add(self.rec_row)

        self.save_row = Adw.ActionRow(title="Save replay")
        self.save_btn = self.button("Save", cb=self.on_save)
        self.save_row.add_suffix(self.save_btn)
        g.add(self.save_row)

        self.dir_row = Adw.ActionRow(title="Folder", subtitle=C.output_dir(self.cfg))
        self.dir_row.set_subtitle_lines(1)
        self.dir_row.add_suffix(self.button(icon="folder-open-symbolic", tip="Open",
                                            cb=lambda *_: self.open_dir()))
        self.dir_row.add_suffix(self.button("Change", cb=self.pick_folder))
        g.add(self.dir_row)
        return g

    def group_video(self):
        g = Adw.PreferencesGroup(title="Video")
        g.add(self.spin("Length", "seconds", "seconds", 10, 600, 5))
        screens = [("all", "All screens")] + [(m["name"], f"{m['name']} ({m['w']}×{m['h']})")
                                              for m in system.monitors()]
        if len(screens) > 2:
            g.add(self.combo("Screen", "monitor", screens))
        g.add(self.combo("FPS", "fps", FPS))
        g.add(self.combo("Quality", "rec_quality", QUALITY))
        encs = [("auto", "Auto")] + [(e, system.ENCODER_LABELS[e]) for e in self.enc_info["available"]]
        g.add(self.combo("Encoder", "encoder", encs))
        g.add(self.switch("Show cursor", "draw_mouse"))
        return g

    def group_sound(self):
        g = Adw.PreferencesGroup(title="Sound")
        g.add(self.switch("System sound", "system_audio"))
        self.out_row = self.combo("Output", "system_device",
                                  [("@DEFAULT_MONITOR@", "Default")] + system.audio_outputs())
        g.add(self.out_row)
        g.add(self.switch("Microphone", "mic"))
        self.mic_row = self.combo("Input", "mic_device", [("@DEFAULT_SOURCE@", "Default")] + system.audio_inputs())
        g.add(self.mic_row)
        return g

    def group_saving(self):
        g = Adw.PreferencesGroup(title="Saving")
        g.add(self.combo("Format", "container", FORMAT))
        g.add(self.combo("Resolution", "export_height", RESOLUTION))
        g.add(self.spin("Keep clips", "0 = keep all", "keep_max_clips", 0, 9999, 1))
        return g

    def group_other(self):
        g = Adw.PreferencesGroup(title="Other")
        self.hk_row = Adw.ActionRow(title="Hotkey")
        self.hk_label = Gtk.Label(label=pretty(self.cfg["hotkey"]), valign=Gtk.Align.CENTER)
        self.hk_label.add_css_class("dim-label")
        self.hk_row.add_suffix(self.hk_label)
        self.hk_row.add_suffix(self.button("Change", cb=self.change_hotkey))
        g.add(self.hk_row)

        intro = self.switch("Intro before replay", "intro")
        intro.add_suffix(self.button(icon="media-playback-start-symbolic", tip="Preview", cb=self.preview_intro))
        g.add(intro)
        g.add(self.switch("Click sound", "click_sound"))
        g.add(self.switch("Notifications", "notifications"))
        self.auto_row = Adw.SwitchRow(title="Start with computer", active=autostart.enabled())
        self.auto_row.connect("notify::active", lambda r, _: autostart.set_enabled(r.get_active()))
        g.add(self.auto_row)
        g.add(self.switch("Tray icon", "tray"))
        return g

    def update_sensitivity(self):
        if hasattr(self, "mic_row"):
            self.out_row.set_sensitive(self.cfg["system_audio"])
            self.mic_row.set_sensitive(self.cfg["mic"])
        if hasattr(self, "save_row"):
            self.save_row.set_subtitle(f"or press {pretty(self.cfg['hotkey'])}")

    # ------------------------------------------------------------------ status
    def poll(self):
        pid = C.daemon_pid()
        st = C.read_json(C.STATUSFILE, {}) if pid else {}
        state = st.get("state") if pid else None
        self._busy = True
        self.rec_switch.set_active(bool(pid) and state != "paused")
        self._busy = False
        if not pid:
            self.rec_row.set_subtitle("Off")
        elif state == "recording":
            g = st.get("geometry") or [0, 0]
            self.rec_row.set_subtitle(f"Last {st.get('seconds')} s · {g[0]}×{g[1]} · {st.get('fps')} fps")
        elif state == "paused":
            self.rec_row.set_subtitle("Paused")
        elif state == "error":
            self.rec_row.set_subtitle(st.get("error") or "Error")
        else:
            self.rec_row.set_subtitle("Starting…")
        self.save_btn.set_sensitive(state == "recording" and st.get("buffered", 0) > 1)
        if pid and st.get("hotkey") == self.cfg["hotkey"]:
            if st.get("hotkey_ok"):
                self.hk_row.set_subtitle("")
            elif st.get("hotkey_error") not in (None, "", "starting"):
                self.hk_row.set_subtitle("Taken by another program")
        return True

    def on_rec_switch(self, sw, _):
        if self._busy:
            return
        pid = C.daemon_pid()
        if sw.get_active() and not pid:
            self.start_daemon()
        elif not sw.get_active() and pid:
            os.kill(pid, signal.SIGTERM)

    def start_daemon(self):
        if self._flush_id:
            GLib.source_remove(self._flush_id)
            self.flush()
        subprocess.Popen(C.launcher() + ["start"], start_new_session=True,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def on_save(self, *_):
        subprocess.Popen(C.launcher() + ["save"], start_new_session=True,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.toast("Saving…")

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
                self.dir_row.set_subtitle(f.get_path())

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

    def first_run_setup(self):
        C.save(self.cfg)
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
                                developer_name="DRZ", license_type=Gtk.License.MIT_X11)
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
