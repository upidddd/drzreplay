"""Tray icon (GTK 3): AyatanaAppIndicator when present, XEmbed StatusIcon otherwise."""
import warnings

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk  # noqa: E402

from . import config as C  # noqa: E402
from .hotkey import pretty  # noqa: E402

try:
    gi.require_version("AyatanaAppIndicator3", "0.1")
    from gi.repository import AyatanaAppIndicator3 as AppIndicator  # noqa: E402
except (ValueError, ImportError):
    AppIndicator = None


class Tray:
    def __init__(self, daemon):
        self.d = daemon
        self.menu = Gtk.Menu()
        self.item_save = Gtk.MenuItem(label="Save replay")
        self.item_save.connect("activate", self.d.save)
        self.item_pause = Gtk.MenuItem(label="Pause")
        self.item_pause.connect("activate", lambda *_: self.d.set_paused(not self.d.paused))
        self.item_intro = Gtk.CheckMenuItem(label="Intro")
        self.item_intro.set_active(bool(self.d.cfg["intro"]))
        self.item_intro.connect("toggled", lambda it: self.d.set_option("intro", it.get_active()))
        self.item_short = []
        for sec in (15, 30):
            it = Gtk.MenuItem(label=f"Save last {sec} s")
            it.connect("activate", lambda _i, s=sec: self.d.save(seconds=s))
            self.item_short.append(it)
        items = [self.item_save] + self.item_short + [Gtk.SeparatorMenuItem(), self.item_pause, self.item_intro,
                                                      Gtk.SeparatorMenuItem()]
        for label, cb in (("Open folder", self.d.open_folder), ("Settings", self.d.open_settings)):
            it = Gtk.MenuItem(label=label)
            it.connect("activate", cb)
            items.append(it)
        quit_ = Gtk.MenuItem(label="Quit")
        quit_.connect("activate", self.d.quit)
        items += [Gtk.SeparatorMenuItem(), quit_]
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

    def update(self):
        d = self.d
        self.item_pause.set_label("Resume" if d.paused else "Pause")
        if self.item_intro.get_active() != bool(d.cfg["intro"]):
            self.item_intro.set_active(bool(d.cfg["intro"]))
        self.item_save.set_label(f"Save replay ({pretty(d.cfg['hotkey'])})")
        on = not d.paused and d.state == "recording"
        for it in [self.item_save] + self.item_short:
            it.set_sensitive(on)
        if self.icon:
            self.icon.set_tooltip_text(f"{C.APP_NAME}: " + ("paused" if d.paused else "recording"))

    def hide(self):
        if self.ind:
            self.ind.set_status(AppIndicator.IndicatorStatus.PASSIVE)
        if self.icon:
            self.icon.set_visible(False)
