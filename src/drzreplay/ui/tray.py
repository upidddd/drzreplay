"""Tray icon (GTK 3): AyatanaAppIndicator when present, XEmbed StatusIcon otherwise."""
import os
import time
import warnings

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk  # noqa: E402

from ..core import config as C  # noqa: E402
from ..recorder.hotkey import pretty  # noqa: E402

try:
    gi.require_version("AyatanaAppIndicator3", "0.1")
    from gi.repository import AyatanaAppIndicator3 as AppIndicator  # noqa: E402
except (ValueError, ImportError):
    AppIndicator = None


class Tray:
    def __init__(self, daemon):
        self.d = daemon
        self.icon_path = C.ICON_PATH
        self.menu = Gtk.Menu()
        self.item_save = Gtk.MenuItem(label="Save replay")
        self.item_save.connect("activate", self.d.save)
        self.item_pause = Gtk.MenuItem(label="Turn replays off")
        self.item_pause.connect("activate", lambda *_: self.d.set_paused(not self.replays_off()))
        self.item_intro = Gtk.CheckMenuItem(label="Intro")
        self.item_intro.set_active(bool(self.d.cfg["intro"]))
        self.item_intro.connect("toggled", lambda it: self.d.set_option("intro", it.get_active()))
        self.item_record = Gtk.MenuItem()     # start / stop recording a full video
        self.item_record.connect("activate", self.d.toggle_file_recording)
        self.item_update = Gtk.MenuItem(label="Update")
        self.item_update.connect("activate", self.d.run_update)
        self.item_update.set_no_show_all(True)
        items = [self.item_update, self.item_save, self.item_record, Gtk.SeparatorMenuItem(),
                 self.item_pause, self.item_intro, Gtk.SeparatorMenuItem()]
        for label, cb in (("Open folder", self.d.open_folder), ("Settings", self.d.open_settings)):
            it = Gtk.MenuItem(label=label)
            it.connect("activate", cb)
            items.append(it)
        # starts the recorder over (a running full recording is saved first): when something got stuck
        restart = Gtk.MenuItem(label="Restart")
        restart.connect("activate", self.d.restart)
        quit_ = Gtk.MenuItem(label="Quit")
        quit_.connect("activate", self.d.quit)
        items += [Gtk.SeparatorMenuItem(), restart, quit_]
        for it in items:
            self.menu.append(it)
        self.menu.show_all()

        if AppIndicator:
            self.ind = AppIndicator.Indicator.new("drzreplay", C.ICON_PATH,
                                                  AppIndicator.IndicatorCategory.APPLICATION_STATUS)
            self.ind.set_status(AppIndicator.IndicatorStatus.ACTIVE)
            self.ind.set_title(C.APP_NAME)
            self.ind.set_menu(self.menu)
            self.ind.set_secondary_activate_target(self.item_save)   # middle click saves
            self.icon = None
        else:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                self.icon = Gtk.StatusIcon.new_from_file(C.ICON_PATH)
                self.icon.set_title(C.APP_NAME)
                self.icon.connect("activate", self.d.save)
                self.icon.connect("popup-menu", lambda i, b, t: self.menu.popup(
                    None, None, Gtk.StatusIcon.position_menu, i, b, t))
            self.ind = None

    def refresh_icon(self):
        """The red dot: the screen is being captured (replay buffer or a full recording)."""
        d = self.d
        on = bool(d.filerec) or (d.state == "recording" and not d.paused and d.proc is not None)
        path = C.ICON_REC if on and os.path.exists(C.ICON_REC) else C.ICON_PATH
        if path == self.icon_path:
            return
        self.icon_path = path
        if self.ind:
            self.ind.set_icon_full(path, "Recording" if on else C.APP_NAME)
        if self.icon:
            self.icon.set_from_file(path)

    def replays_off(self):
        return self.d.paused or self.d.rec_only

    def update(self):
        d = self.d
        self.item_pause.set_label("Turn replays on" if self.replays_off() else "Turn replays off")
        if d.update:
            self.item_update.set_label(f"Update to {d.update}…")
        self.item_update.set_visible(bool(d.update))
        if self.item_intro.get_active() != bool(d.cfg["intro"]):
            self.item_intro.set_active(bool(d.cfg["intro"]))
        self.item_save.set_label(f"Save replay ({pretty(d.cfg['hotkey'])})")
        on = not d.paused and not d.rec_only and d.state == "recording"
        self.item_save.set_sensitive(on)
        if d.filerec:
            mins, secs = divmod(int(time.time() - d.filerec["t0"]), 60)
            self.item_record.set_label(f"Stop recording · {mins}:{secs:02d}")
            self.item_record.set_sensitive(True)
        else:
            key = f" ({pretty(d.cfg['record_hotkey'])})" if d.cfg["record_hotkey"] else ""
            self.item_record.set_label(f"Start recording{key}")
            self.item_record.set_sensitive(on or d.paused)    # with replays off it records just the video
        self.refresh_icon()
        if self.icon:
            self.icon.set_tooltip_text(f"{C.APP_NAME}: " + ("paused" if self.replays_off() else "recording"))

    def hide(self):
        if self.ind:
            self.ind.set_status(AppIndicator.IndicatorStatus.PASSIVE)
        if self.icon:
            self.icon.set_visible(False)
