# DrzReplay

Instant replay for Linux. DrzReplay always keeps the last minute of your screen and sound in memory.
Press **Alt+Z** and it saves that minute to a video. An optional metal-gate intro can open each clip
(off by default).

- **Light on games.** Video is encoded by the graphics card: NVIDIA NVENC, or AMD/Intel VA-API.
  If neither is available it falls back to the CPU (x264).
  The buffer lives in RAM (`/dev/shm`), so the disk is not used until you save.
- **Global hotkey.** It works in any X11 desktop (XFCE, KDE, Cinnamon, MATE, GNOME on Xorg, i3),
  including fullscreen games.
- **Settings app.** Replay length, screen, FPS, quality, encoder, system sound and microphone
  (device and volume), folder, format (MP4/MKV), export resolution, auto-cleanup, hotkey,
  sounds, notifications, autostart, tray and intro.
- **Tray icon.** Click it to save, right-click for the menu.

## Install (Debian 13 / Ubuntu 24.04 or newer)

One command, run in a terminal:

```sh
curl -fsSL https://github.com/upidddd/drzreplay/releases/latest/download/install.sh | sh
```

It downloads the latest release and asks for your password to install it.
Or download `drzreplay_*_all.deb` from [Releases](https://github.com/upidddd/drzreplay/releases)
and run `sudo apt install ./drzreplay_*_all.deb`.

Then open **DrzReplay** from the applications menu. On first launch it starts recording and
enables autostart. Press **Alt+Z** to save a replay. Clips go to `~/Videos/DrzReplay`.

To update, run the same command again.
Settings and clips are kept, and the running recorder switches to the new version by itself.

Uninstall with `sudo apt remove drzreplay`.

### i3

Everything works in i3; a few things differ from a full desktop:

- **Autostart.** If your i3 config does not run `dex --autostart`, the *Start with computer*
  switch adds one line to it (`exec --no-startup-id drzreplay daemon  # DrzReplay autostart`)
  and removes it again when switched off.
- **Notifications** need a notification daemon. The package pulls in `dunst` when none is
  installed; make sure your i3 config starts it (`exec --no-startup-id dunst`).
- **Tray icon** shows in i3bar. With `tray_output primary` the screen must be marked primary
  (`xrandr --output <name> --primary`), otherwise use `tray_output <name>`.
- The settings window opens floating. Alt+Z must not be bound in your i3 config.
- Launch it with dmenu/rofi (`DrzReplay`) or `drzreplay` from a terminal.

> DrzReplay captures the screen through X11. In a Wayland session, pick an "Xorg" session
> on the login screen.

## Command line

```
drzreplay              settings window
drzreplay save         save a replay now
drzreplay start|stop|restart
drzreplay status
drzreplay preview-intro
```

## Building

```sh
packaging/build-deb.sh          # -> dist/drzreplay_<version>_all.deb
```

The intro is pre-rendered into `src/assets/intro`. To change it, edit `tools/render_intro.py`
and run it. This needs `python3-numpy python3-scipy python3-pil`, and the fonts and sounds
in `tools/`:

```sh
python3 tools/render_intro.py --jobs 3
```

## Credits

Sound effects come from OpenGameArt.org: "Sci-Fi Pack Vol 1" by JC Sounds (CC BY 4.0),
plus CC0 sounds by Brian MacIntosh and others. The intro font is Oswald (OFL).
See `src/assets/CREDITS.txt`.
