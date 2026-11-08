<div align="center">

<img src="src/drzreplay/assets/icons/drzreplay.svg" width="96" alt="">

# DrzReplay

**Instant replay for Linux.** Your screen is always recorded into memory —
press **Alt+Z** and the last minute becomes a video file.

[![Release](https://img.shields.io/github/v/release/upidddd/drzreplay?style=flat-square&color=2b3033)](https://github.com/upidddd/drzreplay/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/upidddd/drzreplay/total?style=flat-square&color=2b3033)](https://github.com/upidddd/drzreplay/releases)
[![Platform](https://img.shields.io/badge/Linux-X11%20%7C%20Wayland-2b3033?style=flat-square&logo=linux&logoColor=white)](#requirements)
[![License](https://img.shields.io/github/license/upidddd/drzreplay?style=flat-square&color=2b3033)](LICENSE)

```sh
curl -fsSL https://github.com/upidddd/drzreplay/releases/latest/download/install.sh | sh
```

<img src="docs/record.png" width="32%" alt="Record tab"> <img src="docs/clips.png" width="32%" alt="Clips tab"> <img src="docs/settings.png" width="32%" alt="Settings tab">

</div>

## Why

Something great happened in a game, a call or on stream — and you weren't recording.
DrzReplay is always recording, but only in memory, so nothing piles up on your disk.
When you want to keep the moment, one key saves it.

## Features

- **Always on, never in the way.** Video is compressed by the graphics card (NVIDIA NVENC or
  AMD/Intel VA-API), so the recorder takes about 3 % of the CPU and games keep their FPS.
  The last minute lives in RAM (10–100 MB), the disk is only touched when you save.
- **One key.** Alt+Z (or any key you pick) works everywhere, including fullscreen games that
  grab the keyboard. Press it twice and you get two clips — nothing is lost.
- **No repeats.** If 15 seconds passed since the last replay, the next one saves just those 15 s,
  not the whole buffer again (can be turned off in Settings).
- **Full recordings too.** Alt+R starts recording a whole video, Alt+R again stops it. It uses the same
  encoder, so it costs nothing extra, and the file is ready a second after you stop.
- **Sound.** System sound and your microphone, each with its own device and volume. Works with
  PulseAudio and PipeWire; plug in headphones and recording follows them without a restart.
- **Clips tab.** Thumbnails, length and size of every clip. Click to play, **drag a clip
  straight into Discord or Telegram**, copy it, show it in the folder or move it to the trash.
- **Trim.** The scissors on a clip open it with a player: pick the start and the end, get a trimmed copy.
- **Clean up by itself.** Keep only the last N clips or at most N GB.
- **Updates.** When a new version is out you get a notification and an *Update* item in the tray;
  one click starts the installer.
- **Works on** XFCE, KDE, Cinnamon, MATE, GNOME and tiling window managers like **i3** — on Xorg
  and on **Wayland**.


## Install

```sh
curl -fsSL https://github.com/upidddd/drzreplay/releases/latest/download/install.sh | sh
```

It downloads the latest release, asks for your password and installs it. DrzReplay opens and
starts recording right away; from then on it starts with your computer.

- **Update:** press *Update* in the notification, the tray or the app — it installs in the background
  without asking for a password, and the running recorder switches to the new version by itself.
  Settings and clips are kept. (The package adds a sudo rule that allows exactly this: its own helper,
  which only installs a newer official `drzreplay` package from this page.)
- **Manual install:** download `drzreplay_*_all.deb` from
  [Releases](https://github.com/upidddd/drzreplay/releases) and run
  `sudo apt install ./drzreplay_*_all.deb`.
- **Uninstall:** `sudo apt remove drzreplay`

### Requirements

- Debian 13, Devuan 6, Ubuntu 24.04 or newer (and distros based on them: Mint, Pop!_OS…)
- An **X11 (Xorg)** or **Wayland** session. On Wayland the desktop asks once which screen to share
  (GNOME, KDE Plasma and other desktops with xdg-desktop-portal); the answer is remembered.
- Any graphics card. Without NVENC / VA-API the CPU encoder (x264) is used.

## Using it

| | |
|---|---|
| **Alt+Z** | save the last minute (length is set on the Record tab) |
| **Alt+R** | start / stop recording a full video |
| **Tray icon** | right click: save, start / stop recording, pause, folder, settings |
| **Middle click on the tray icon** | save |
| **Menu → DrzReplay** | the window with the Record, Clips and Settings tabs |

Clips go to `~/Videos/DrzReplay` as `DrzReplay 2026-11-02 17-20-10.mp4`.

## How it works

```mermaid
flowchart LR
    A["Screen<br>X11 or Wayland (PipeWire)"] -->|"30 fps"| F["ffmpeg / GStreamer"]
    S["System sound, mic"] --> F
    F -->|"GPU encoding"| R[("Ring buffer in RAM<br>5-second chunks")]
    K["Alt+Z"] --> V["Save"]
    R --> V
    V -->|"last N seconds"| O["~/Videos/DrzReplay"]
```

1. A background recorder (`drzreplay daemon`) runs ffmpeg, which grabs the screen and sound and
   writes 5-second chunks to `/dev/shm` (RAM). The oldest chunk is overwritten, so memory use stays flat.
   On Wayland the screen comes from the desktop's screen-sharing portal: GStreamer records it together
   with the sound (one clock, so they stay in sync) and ffmpeg writes the same chunks.
2. On X11 a tiny helper listens for the hotkey through raw input events — that is why it keeps working
   while a fullscreen game holds the keyboard. On Wayland the desktop's global-shortcuts portal does it.
3. On save, the chunks are copied, glued together and trimmed to exactly the last N seconds,
   and the result is encoded on the GPU with low priority.

## Troubleshooting

<details>
<summary><b>The hotkey does nothing</b></summary>

- Settings → Controls shows *Not working* next to the hotkey when another program owns it.
  Pick a different key there.
- Every press is logged in `/dev/shm/drzreplay-$(id -u)/hotkey.log`. If presses show up there
  but no clip appears, check the recorder log (window menu → *Recorder log*).
</details>

<details>
<summary><b>No sound in clips</b></summary>

Record tab → Sound: make sure *System sound* is on and *Output* is the device you listen to.
If capturing sound fails three times, the recorder continues without sound and says so in a
notification and on the Record tab.
</details>

<details>
<summary><b>Wayland: the hotkey or the screen</b></summary>

- **Screen:** the first start shows the desktop's screen-sharing dialog; pick a screen there.
  To pick another one later: Record tab → Screen → *Choose again*.
- **Hotkey:** GNOME 48+ and KDE Plasma 6 offer DrzReplay a global shortcut (confirm it once).
  On other desktops add a keyboard shortcut in the system settings that runs `drzreplay save`.
- Missing parts are named in the Record tab; on Debian / Ubuntu:
  `sudo apt install gstreamer1.0-pipewire gstreamer1.0-plugins-good gstreamer1.0-libav xdg-desktop-portal`
</details>

<details>
<summary><b>i3: no tray icon, no notifications, no autostart</b></summary>

- **Tray:** with `tray_output primary` in your bar config the monitor must be primary
  (`xrandr --output <name> --primary`), or use `tray_output <name>`.
- **Notifications** need a notification daemon: add `exec --no-startup-id dunst` to your i3 config
  (the package installs dunst if you have none).
- **Autostart:** if your config doesn't run `dex --autostart`, the *Start with computer* switch
  adds one marked line (`exec --no-startup-id drzreplay daemon  # DrzReplay autostart`) and removes
  it again when switched off.
- The window opens floating. Make sure Alt+Z isn't bound in your i3 config.
</details>

<details>
<summary><b>The game stutters while recording</b></summary>

Check that the encoder on the Record tab says *NVIDIA NVENC* or *VA-API*. With *CPU (x264)*,
lower the frame rate to 30 and the quality to *Low* or *Medium*.
</details>

## Command line

```
drzreplay                      open the window
drzreplay save                 save a replay now
drzreplay record [start|stop]  start / stop recording a full video
drzreplay start|stop|restart   control the background recorder
drzreplay status               what the recorder is doing
```

## Project layout

```
src/drzreplay/
├── __main__.py        command line: drzreplay [save|start|stop|status…]
├── core/              settings, paths, machine probing, autostart, update check
├── recorder/          background recorder, Wayland capture, hotkey, saving and trimming clips
├── ui/                window (Record / Clips / Settings) and tray icon
└── assets/            icons, sounds, intro video
packaging/
├── debian/            control file and maintainer scripts of the .deb
├── files/             launcher, .desktop entries
├── build-deb.sh       build the package
├── release.sh         build and publish a GitHub release
└── install.sh         the one-line installer
tools/intro/           renderer of the intro animation (fonts, sounds)
docs/                  screenshots for this page
```

## Building

```sh
packaging/build-deb.sh     # -> dist/drzreplay_<version>_all.deb
packaging/release.sh       # build + publish a GitHub release (needs gh)
```

Run from source without installing: `PYTHONPATH=src python3 -m drzreplay`.
It needs `python3-gi gir1.2-gtk-4.0 gir1.2-adw-1 ffmpeg x11-xserver-utils pulseaudio-utils`.

## Credits

Sounds from OpenGameArt.org: "Sci-Fi Pack Vol 1" by JC Sounds (CC BY 4.0),
"Mechanical Sounds" by Brian MacIntosh (CC0) and "Iron Door" (CC0).
Font: Oswald (SIL OFL). Details in [`src/drzreplay/assets/CREDITS.txt`](src/drzreplay/assets/CREDITS.txt).

Code: [MIT](LICENSE).
