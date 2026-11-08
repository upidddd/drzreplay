"""The replay intro: the replay number and the date slide out of a thin bar in the top-left corner
for a couple of seconds, no sound. Drawn with Pillow into a short transparent clip that the export
lays over the start of the replay."""
import glob
import os
import subprocess
from datetime import datetime

from ..core import config as C

FONTS = os.path.join(C.ASSETS, "fonts")
COUNTER = os.path.join(C.CONF_DIR, "replays.json")     # {"count": replays saved so far}
CREAM, RED = (236, 230, 214), (224, 27, 36)
START = 0.4             # seconds into the clip when the bar appears
LENGTH = 2.52           # how long it stays, in and out animations included
SIZE = 0.74             # against the first design (70 px title at 1080p)
X0, Y0 = 48, 40         # position at 1080p


def available():
    try:
        import PIL  # noqa: F401
    except ImportError:
        return False
    return all(os.path.exists(os.path.join(FONTS, f)) for f in ("Oswald.ttf", "Inter-Medium.ttf"))


def next_number(cfg):
    """Number of the replay being saved now. Counting starts from the clips already in the folder."""
    count = (C.read_json(COUNTER, {}) or {}).get("count")
    if count is None:
        prefix = cfg["file_prefix"] or C.APP_NAME
        count = sum(1 for p in glob.glob(os.path.join(glob.escape(C.output_dir(cfg)), f"{glob.escape(prefix)} *"))
                    if p.endswith((".mp4", ".mkv")) and "(trimmed)" not in p)
    return int(count) + 1


def commit(number):
    C.write_json(COUNTER, {"count": number})


def position(height):
    k = height / 1080
    return round(X0 * k), round(Y0 * k)


def _clamp(x):
    return max(0.0, min(1.0, x))


def _ease_out(x):
    return 1 - (1 - _clamp(x)) ** 3


def _ease_in(x):
    return _clamp(x) ** 3


def _phase(t, a, b):
    return _clamp((t - a) / (b - a))


class Painter:
    """Draws frame t (seconds from the moment the bar appears) of the intro, for a video `height` px tall."""

    def __init__(self, number, when, height):
        from PIL import ImageFont
        self.k = SIZE * height / 1080
        self.title = ImageFont.truetype(os.path.join(FONTS, "Oswald.ttf"), max(8, round(70 * self.k)))
        self.small = ImageFont.truetype(os.path.join(FONTS, "Inter-Medium.ttf"), max(6, round(30 * self.k)))
        self.number, self.date = f"#{number}", f"{when:%d.%m.%Y} · {when:%H:%M}"
        self.w = round(600 * self.k) + round(self.title.getlength(self.number))
        self.h = round(170 * self.k)

    def state(self, t):
        """(bar height, text x, date fade) at time t, rounded to what can be seen."""
        z, L = self.k, LENGTH
        sp = LENGTH / 3.6                    # the timings below were designed for a 3.6 s version
        grow = _ease_out(_phase(t, 0, 0.35 * sp)) * (1 - _ease_in(_phase(t, L - 0.25 * sp, L)))
        slide = _ease_out(_phase(t, 0.15 * sp, 0.65 * sp)) * (1 - _ease_in(_phase(t, L - 0.55 * sp, L)))
        k = _ease_out(_phase(t, 0.35 * sp, 0.8 * sp)) * (1 - _ease_in(_phase(t, L - 0.7 * sp, L - 0.3 * sp)))
        return round(118 * z * grow, 1), round(24 * z - (1 - slide) * 420 * z, 1), round(k, 3)

    def frame(self, t):
        return self.draw(*self.state(t))

    def draw(self, bar_h, x, k):
        from PIL import Image, ImageDraw, ImageFilter
        z = self.k
        img = Image.new("RGBA", (self.w, self.h), (0, 0, 0, 0))
        d = ImageDraw.Draw(img)
        if bar_h > 1:
            d.rounded_rectangle([0, 10 * z, max(2, 6 * z), 10 * z + bar_h], radius=max(1, 3 * z), fill=CREAM + (255,))
        text = Image.new("RGBA", img.size, (0, 0, 0, 0))
        td = ImageDraw.Draw(text)
        td.text((x, 0), "REPLAY", font=self.title, fill=CREAM + (255,))
        td.text((x + td.textlength("REPLAY ", font=self.title), 0), self.number, font=self.title, fill=RED + (255,))
        td.text((26 * z, (92 + (1 - k) * 12) * z), self.date, font=self.small, fill=CREAM + (int(230 * k),))
        # the text only exists to the right of the bar: that's what makes it slide out from behind it
        mask = Image.new("L", img.size, 0)
        ImageDraw.Draw(mask).rectangle([round(14 * z), 0, self.w, self.h], fill=255)
        text.putalpha(Image.composite(text.getchannel("A"), Image.new("L", img.size, 0), mask))
        img.alpha_composite(text)
        # a soft shadow so it reads on bright footage too
        shadow = Image.new("RGBA", img.size, (0, 0, 0, 0))
        shadow.putalpha(img.getchannel("A").filter(ImageFilter.GaussianBlur(max(1, 5 * z / SIZE)))
                        .point(lambda v: v * 150 // 255))
        res = Image.new("RGBA", img.size, (0, 0, 0, 0))
        res.alpha_composite(shadow, (2, 2))
        res.alpha_composite(img)
        return res


def render(path, number, when=None, height=1080, fps=30):
    """Write the animation as a transparent clip (qtrle .mov). Returns its size in pixels."""
    p = Painter(number, when or datetime.now(), height)
    cmd = ["ffmpeg", "-nostdin", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba",
           "-s", f"{p.w}x{p.h}", "-r", str(fps), "-i", "-", "-c:v", "qtrle", path]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stderr=subprocess.PIPE)
    last, data = None, b""
    try:
        for i in range(int(LENGTH * fps) + 1):
            st = p.state(i / fps)
            if st != last:                   # the middle of the animation stands still: draw it once
                last, data = st, p.draw(*st).tobytes()
            proc.stdin.write(data)
        proc.stdin.close()
    except BrokenPipeError:
        pass
    err = proc.stderr.read().decode(errors="replace").strip()
    if proc.wait() != 0:
        raise RuntimeError(err.splitlines()[-1] if err else "intro render failed")
    return p.w, p.h
