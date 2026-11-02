"""Start-with-system toggle.

The .deb ships /etc/xdg/autostart/drzreplay.desktop; a user turns it off with a
Hidden=true override in ~/.config/autostart. Run from source, we write a full entry.

i3 only runs XDG autostart entries when its config calls `dex --autostart`. When the
user's i3 config doesn't, we add (or remove) one marked `exec` line to it instead.
"""
import os
import re
import shlex

from . import config as C

CONF_HOME = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
SYSTEM_FILE = "/etc/xdg/autostart/drzreplay.desktop"
USER_FILE = os.path.join(CONF_HOME, "autostart", "drzreplay.desktop")
I3_MARK = "# DrzReplay autostart"


def _exe():
    return " ".join(shlex.quote(p) for p in C.launcher())


def _entry(hidden=False):
    return f"""[Desktop Entry]
Type=Application
Name={C.APP_NAME}
Comment=Instant replay recorder
Exec={_exe()} daemon
Icon=drzreplay
Terminal=false
NoDisplay=true
X-GNOME-Autostart-enabled={'false' if hidden else 'true'}
Hidden={'true' if hidden else 'false'}
"""


def is_i3():
    return bool(os.environ.get("I3SOCK")) or "i3" in os.environ.get("XDG_CURRENT_DESKTOP", "").lower().split(":")


def i3_config():
    """The user's own i3 config, or None when i3 runs the system one (which calls dex)."""
    for p in (os.path.join(CONF_HOME, "i3", "config"), os.path.expanduser("~/.i3/config")):
        if os.path.isfile(p):
            return os.path.realpath(p)   # dotfiles are often symlinks: edit the real file
    return None


def _i3_lines():
    p = i3_config() if is_i3() else None
    if not p:
        return None, []
    with open(p) as f:
        lines = f.read().splitlines(keepends=True)
    if any(re.match(r"\s*exec\b.*\bdex\b.*--autostart", l) for l in lines if I3_MARK not in l):
        return None, []       # dex handles XDG autostart for us
    return p, lines


def _xdg_enabled():
    if os.path.exists(USER_FILE):
        with open(USER_FILE) as f:
            text = f.read()
        return "Hidden=true" not in text and "X-GNOME-Autostart-enabled=false" not in text
    return os.path.exists(SYSTEM_FILE)


def enabled():
    p, lines = _i3_lines()
    if p:
        return any(I3_MARK in l and not l.lstrip().startswith("#") for l in lines)
    return _xdg_enabled()


def set_enabled(on):
    os.makedirs(os.path.dirname(USER_FILE), exist_ok=True)
    if on and os.path.exists(SYSTEM_FILE):
        if os.path.exists(USER_FILE):
            os.remove(USER_FILE)
    else:
        with open(USER_FILE, "w") as f:
            f.write(_entry(hidden=not on))

    p, lines = _i3_lines()
    if p:
        lines = [l for l in lines if I3_MARK not in l]
        if on:
            if lines and not lines[-1].endswith("\n"):
                lines[-1] += "\n"
            lines.append(f"exec --no-startup-id {_exe()} daemon  {I3_MARK}\n")
        tmp = p + ".drzreplay.tmp"
        with open(tmp, "w") as f:
            f.writelines(lines)
        os.replace(tmp, p)
