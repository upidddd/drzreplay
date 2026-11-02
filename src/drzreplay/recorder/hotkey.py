"""Global hotkey for any X11 desktop.

Presses are read from XI2 raw key events, so the hotkey keeps working while a
fullscreen game grabs the keyboard. Runs as a tiny helper process so an X error
can never take down the tray. Protocol on stdout:
  OK            grab succeeded
  FAIL <why>    grab failed, process exits
  PRESS         hotkey pressed
"""
import ctypes
import ctypes.util
import os
import re
import select
import sys
import time

SHIFT, LOCK, CONTROL, MOD1, MOD2, MOD4 = 1, 2, 4, 8, 16, 64
MODS = {"shift": SHIFT, "control": CONTROL, "ctrl": CONTROL, "primary": CONTROL,
        "alt": MOD1, "mod1": MOD1, "super": MOD4, "meta": MOD4, "mod4": MOD4}
KEY_PRESS = 2


def parse(accel):
    """'<Primary><Alt>r' -> (modmask, 'r')"""
    mods = 0
    for m in re.findall(r"<(\w+)>", accel):
        mods |= MODS.get(m.lower(), 0)
    key = re.sub(r"<\w+>", "", accel).strip()
    return mods, key


def pretty(accel):
    """'<Primary><Alt>r' -> 'Ctrl+Alt+R'"""
    names = {CONTROL: "Ctrl", MOD1: "Alt", SHIFT: "Shift", MOD4: "Super"}
    mods, key = parse(accel)
    parts = [n for bit, n in names.items() if mods & bit]
    if len(key) == 1:
        key = key.upper()
    return "+".join(parts + [key.replace("_", " ")]) if key else "—"


class XIEventMask(ctypes.Structure):
    _fields_ = [("deviceid", ctypes.c_int), ("mask_len", ctypes.c_int), ("mask", ctypes.POINTER(ctypes.c_ubyte))]


class XGenericEventCookie(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong), ("send_event", ctypes.c_int),
                ("display", ctypes.c_void_p), ("extension", ctypes.c_int), ("evtype", ctypes.c_int),
                ("cookie", ctypes.c_uint), ("data", ctypes.c_void_p)]


class XIRawEvent(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong), ("send_event", ctypes.c_int),
                ("display", ctypes.c_void_p), ("extension", ctypes.c_int), ("evtype", ctypes.c_int),
                ("time", ctypes.c_ulong), ("deviceid", ctypes.c_int), ("sourceid", ctypes.c_int),
                ("detail", ctypes.c_int), ("flags", ctypes.c_int)]


class XModifierKeymap(ctypes.Structure):
    _fields_ = [("max_keypermod", ctypes.c_int), ("modifiermap", ctypes.POINTER(ctypes.c_ubyte))]


GENERIC_EVENT, XI_ALL_MASTER, XI_RAW_KEY_PRESS, XI_RAW_KEY_RELEASE = 35, 1, 13, 14
CHECKED_MODS = SHIFT | CONTROL | MOD1 | MOD4   # Lock, NumLock, AltGr & co. don't matter


def setup_xi2(x, dpy, root):
    """Listen to raw key events on the root window. Unlike a key grab these keep coming
    while a fullscreen game grabs the keyboard or another program owns the same shortcut."""
    try:
        xi = ctypes.cdll.LoadLibrary(ctypes.util.find_library("Xi") or "libXi.so.6")
    except OSError:
        return None
    xi.XIQueryVersion.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p]
    xi.XISelectEvents.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p, ctypes.c_int]
    opcode, ev, err = ctypes.c_int(), ctypes.c_int(), ctypes.c_int()
    if not x.XQueryExtension(dpy, b"XInputExtension", ctypes.byref(opcode), ctypes.byref(ev), ctypes.byref(err)):
        return None
    major, minor = ctypes.c_int(2), ctypes.c_int(2)
    if xi.XIQueryVersion(dpy, ctypes.byref(major), ctypes.byref(minor)) != 0:
        return None
    bits = (ctypes.c_ubyte * 4)()
    for e in (XI_RAW_KEY_PRESS, XI_RAW_KEY_RELEASE):
        bits[e >> 3] |= 1 << (e & 7)
    mask = XIEventMask(XI_ALL_MASTER, 4, bits)
    xi.XISelectEvents(dpy, root, ctypes.byref(mask), 1)
    return opcode.value


def modifier_keycodes(x, dpy):
    """{modifier bit: {keycodes}} from the current keymap."""
    x.XGetModifierMapping.restype = ctypes.POINTER(XModifierKeymap)
    m = x.XGetModifierMapping(dpy)
    out = {}
    n = m.contents.max_keypermod
    for i in range(8):
        out[1 << i] = {m.contents.modifiermap[i * n + j] for j in range(n)} - {0}
    x.XFreeModifiermap(m)
    return out


def main(accel):
    out = sys.stdout

    def say(msg):
        out.write(msg + "\n")
        out.flush()

    lib = ctypes.util.find_library("X11") or "libX11.so.6"
    try:
        x = ctypes.cdll.LoadLibrary(lib)
    except OSError:
        say("FAIL libX11 not found")
        return 2
    x.XOpenDisplay.restype = ctypes.c_void_p
    x.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x.XDefaultRootWindow.restype = ctypes.c_ulong
    x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x.XStringToKeysym.restype = ctypes.c_ulong
    x.XStringToKeysym.argtypes = [ctypes.c_char_p]
    x.XKeysymToKeycode.restype = ctypes.c_ubyte
    x.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    x.XGrabKey.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_uint, ctypes.c_ulong,
                           ctypes.c_int, ctypes.c_int, ctypes.c_int]
    x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x.XPending.argtypes = [ctypes.c_void_p]
    x.XNextEvent.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    x.XConnectionNumber.argtypes = [ctypes.c_void_p]
    x.XQueryExtension.argtypes = [ctypes.c_void_p, ctypes.c_char_p] + [ctypes.c_void_p] * 3
    x.XGetModifierMapping.argtypes = [ctypes.c_void_p]
    x.XFreeModifiermap.argtypes = [ctypes.c_void_p]
    x.XQueryKeymap.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    x.XGetEventData.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    x.XFreeEventData.argtypes = [ctypes.c_void_p, ctypes.c_void_p]

    failed = []
    ERRH = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)

    def on_error(_dpy, _ev):
        failed.append(1)
        return 0

    handler = ERRH(on_error)
    x.XSetErrorHandler(handler)

    dpy = x.XOpenDisplay(None)
    if not dpy:
        say("FAIL cannot open X display")
        return 2
    root = x.XDefaultRootWindow(dpy)
    mods, key = parse(accel)
    sym = x.XStringToKeysym(key.encode()) or x.XStringToKeysym(key.lower().encode())
    code = x.XKeysymToKeycode(dpy, sym) if sym else 0
    if not code:
        say(f"FAIL unknown key '{key}'")
        return 2
    # The grab only keeps the shortcut from reaching other apps; presses are read from XI2
    # raw events on a second connection: the X server holds back raw events from the client
    # whose grab is active, so on one connection the key releases would go missing.
    for extra in (0, LOCK, MOD2, LOCK | MOD2):
        x.XGrabKey(dpy, code, mods | extra, root, 1, 1, 1)
    x.XSync(dpy, 0)
    listen = x.XOpenDisplay(None) or dpy
    xi_opcode = setup_xi2(x, listen, x.XDefaultRootWindow(listen)) if listen != dpy else None
    if xi_opcode is None:
        listen = dpy
    x.XSync(listen, 0)
    if xi_opcode is None and failed:
        say("FAIL shortcut is already used by another program")
        return 3
    say("OK")

    modkeys = modifier_keycodes(x, listen)
    keymap = ctypes.create_string_buffer(32)
    down = set()

    def held_mods():
        x.XQueryKeymap(listen, keymap)
        pressed = {k for k in range(256) if keymap.raw[k >> 3] & (1 << (k & 7))} | down
        return sum(bit for bit, codes in modkeys.items() if codes & pressed)

    parent = os.getppid()
    fds = {x.XConnectionNumber(dpy), x.XConnectionNumber(listen)}
    ev = ctypes.create_string_buffer(256)
    cookie = ctypes.cast(ev, ctypes.POINTER(XGenericEventCookie)).contents
    last = 0.0

    def fire():
        nonlocal last
        now = time.monotonic()
        if now - last > 0.5:          # one press, one save
            last = now
            say("PRESS")

    while True:
        select.select(list(fds), [], [], 1.0)
        if listen != dpy:
            while x.XPending(dpy):            # grab connection: just drain it
                x.XNextEvent(dpy, ev)
        while x.XPending(listen):
            x.XNextEvent(listen, ev)
            etype = int.from_bytes(ev.raw[:4], sys.byteorder, signed=True)
            if xi_opcode is None:
                if etype == KEY_PRESS:
                    fire()
                continue
            if etype != GENERIC_EVENT or cookie.extension != xi_opcode or not x.XGetEventData(listen, ev):
                continue
            try:
                raw = ctypes.cast(cookie.data, ctypes.POINTER(XIRawEvent)).contents
                if cookie.evtype == XI_RAW_KEY_RELEASE:
                    down.discard(raw.detail)
                elif cookie.evtype == XI_RAW_KEY_PRESS:
                    repeat = raw.detail in down
                    down.add(raw.detail)
                    if raw.detail == code and not repeat and held_mods() & CHECKED_MODS == mods & CHECKED_MODS:
                        fire()
            finally:
                x.XFreeEventData(listen, ev)
        if os.getppid() != parent:
            return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "<Alt>z"))
