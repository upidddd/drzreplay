#!/usr/bin/env python3
"""ДРЗ ОТКАТЫ — рендер заставки «механические ворота» и звука к ней.

Ворота рисуются процедурно: карта высот -> нормали -> освещение точечными
лампами и вращающимся маячком, износ краски, ржавчина, пар, искры, motion blur.
Результат: gate_color.mp4 (цвет) + gate_alpha.mkv (альфа) + intro_sfx.wav,
которые потом накладываются на начало каждого отката.
"""
import argparse
import json
import math
import os
import subprocess
import sys
from multiprocessing import Pool

import numpy as np
from PIL import Image, ImageDraw, ImageFont
from scipy import ndimage, signal

HERE = os.path.dirname(os.path.abspath(__file__))
FONTS = os.path.join(HERE, "fonts")
SOUNDS = os.path.join(HERE, "sounds")

# ---- таймлайн (секунды) ----
T_LIGHTS = 0.30          # щёлкает рубильник, лампы моргают и загораются
T_BEACON = 0.80          # стартует маячок
T_BOLTS = (1.25, 1.45, 1.65)
BOLT_TRAVEL = 0.13
T_HISS = 1.90            # сброс давления
T_OPEN = 2.15            # створки срываются с места; с этого момента играет откат
T_OPEN_END = 3.95        # створки ушли за экран
DURATION = 4.60

W = H = S = 0
DOORS = None
FX = None


# ---------------------------------------------------------------- helpers
def smoothstep(e0, e1, x):
    t = np.clip((x - e0) / (e1 - e0), 0.0, 1.0)
    return t * t * (3 - 2 * t)


def blur(a, sigma):
    """sigma — число или (sy, sx)."""
    sy, sx = sigma if isinstance(sigma, tuple) else (sigma, sigma)
    if max(sy, sx) < 0.3:
        return a
    sig = (sy, sx, 0) if a.ndim == 3 else (sy, sx)
    return ndimage.gaussian_filter(a, sig, mode="nearest")


def resize(a, w, h):
    if a.ndim == 3:
        return np.stack([resize(a[..., i], w, h) for i in range(a.shape[2])], -1)
    return np.asarray(Image.fromarray(a.astype(np.float32)).resize((w, h), Image.BICUBIC))


def value_noise(rng, h, w, cell):
    cell = max(1.0, cell)
    gh, gw = int(h / cell) + 3, int(w / cell) + 3
    g = rng.random((gh, gw), dtype=np.float32)
    return resize(g, int(gw * cell), int(gh * cell))[:h, :w]


def fbm(rng, h, w, cell, octaves=5, gain=0.5):
    out = np.zeros((h, w), np.float32)
    amp, tot = 1.0, 0.0
    for _ in range(octaves):
        out += amp * value_noise(rng, h, w, cell)
        tot += amp
        amp *= gain
        cell /= 2.0
        if cell < 1:
            break
    out /= tot
    lo, hi = np.percentile(out, (0.5, 99.5))
    return np.clip((out - lo) / (hi - lo + 1e-6), 0, 1)


def srgb_to_lin(c):
    return np.power(np.clip(c, 0, 1), 2.2)


def lin_to_srgb(c):
    return np.power(np.clip(c, 0, 1), 1 / 2.2)


def aces(x):
    return np.clip((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), 0, 1)


def font(name, size, variation=None):
    f = ImageFont.truetype(os.path.join(FONTS, name), int(size))
    if variation:
        try:
            f.set_variation_by_name(variation)
        except Exception:
            pass
    return f


def text_mask(text, fnt, cx, cy, tracking=0.0, cap_ref="Н"):
    """Маска текста (H,W) c разрядкой, центр по (cx, cy) по высоте заглавных."""
    img = Image.new("L", (W, H), 0)
    d = ImageDraw.Draw(img)
    widths = [d.textlength(ch, font=fnt) for ch in text]
    total = sum(widths) + tracking * (len(text) - 1)
    bb = d.textbbox((0, 0), cap_ref, font=fnt)
    x = cx - total / 2
    y = cy - (bb[1] + bb[3]) / 2
    for ch, wch in zip(text, widths):
        d.text((x, y), ch, font=fnt, fill=255)
        x += wch + tracking
    return np.asarray(img, np.float32) / 255.0


# ---------------------------------------------------------------- build gate
def build_gate(seed=7):
    rng = np.random.default_rng(seed)
    s = S
    yy, xx = np.mgrid[0:H, 0:W].astype(np.float32)
    u, v = xx / W, yy / H
    seam = W / 2

    # --- уровни конструкции (до размытия => фаски)
    lvl = np.zeros((H, W), np.float32)

    def rect(u0, v0, u1, v1, val, op="set"):
        sl = (slice(int(v0 * H), int(v1 * H)), slice(int(u0 * W), int(u1 * W)))
        if op == "add":
            lvl[sl] += val
        else:
            lvl[sl] = val

    rect(0, 0.14, 1, 0.685, -3.5 * s)                       # утопленная филёнка
    rect(0, 0.03, 1, 0.118, 9 * s)                           # верхнее ребро
    rect(0, 0.705, 1, 0.838, 10 * s)                         # пояс замка
    rect(0, 0.872, 1, 0.962, 7 * s)                          # нижний пояс (полосы)
    for a, b in ((0, 0.032), (0.968, 1), (0.152, 0.176), (0.824, 0.848)):
        rect(a, 0.03, b, 0.962, 9 * s)                       # вертикальные рёбра
    rect(0.535, 0.722, 0.632, 0.821, 26 * s)                 # корпус засовов (правая)
    rect(0.368, 0.722, 0.465, 0.821, 22 * s)                 # ответная часть (левая)
    hgt = blur(lvl, 2.2 * s)

    # шов: V-образная фаска на обеих створках
    dseam = np.abs(xx - seam)
    hgt -= 18 * s * smoothstep(9 * s, 0, dseam)

    # вмятины и неровности листа
    hgt += (fbm(rng, H, W, 260 * s, 4) - 0.5) * 2.2 * s
    for _ in range(14):
        cx, cy = rng.uniform(0, W), rng.uniform(0.15 * H, 0.95 * H)
        r = rng.uniform(10, 45) * s
        hgt -= rng.uniform(0.6, 2.2) * s * np.exp(-((xx - cx) ** 2 + (yy - cy) ** 2) / (2 * r * r))

    # --- заклёпки / болты
    rivets = []

    def row(y, x0, x1, step):
        for x in np.arange(x0, x1, step):
            if abs(x - seam) > 34 * s:
                rivets.append((x, y))

    st = 44 * s
    row(0.05 * H, 0.05 * W, 0.95 * W, st)
    row(0.098 * H, 0.05 * W, 0.95 * W, st)
    row(0.719 * H, 0.045 * W, 0.955 * W, st)
    row(0.824 * H, 0.045 * W, 0.955 * W, st)
    row(0.884 * H, 0.045 * W, 0.955 * W, st * 1.5)
    for xc in (0.164, 0.836, 0.016, 0.984):
        for y in np.arange(0.16 * H, 0.70 * H, 52 * s):
            rivets.append((xc * W, y))

    rr = 6.5 * s
    k = int(rr) + 2
    py, px = np.mgrid[-k:k + 1, -k:k + 1].astype(np.float32)
    dome = 5.5 * s * np.sqrt(np.clip(1 - (px ** 2 + py ** 2) / rr ** 2, 0, 1))
    rivet_mask = np.zeros((H, W), np.float32)
    rusty = np.zeros((H, W), np.float32)
    for (x, y) in rivets:
        xi, yi = int(x), int(y)
        if k <= xi < W - k and k <= yi < H - k:
            sl = (slice(yi - k, yi + k + 1), slice(xi - k, xi + k + 1))
            base = hgt[sl].mean()
            hgt[sl] = np.maximum(hgt[sl], base + dome)
            rivet_mask[sl] = np.maximum(rivet_mask[sl], (dome > 0).astype(np.float32))
            if rng.random() < 0.22 + 0.4 * (y / H > 0.7):
                rusty[sl] = np.maximum(rusty[sl], (dome > 0).astype(np.float32))

    # шестигранные болты на корпусах замка
    hexes = []
    for (x0, x1) in ((0.535, 0.632), (0.368, 0.465)):
        for (fx, fy) in ((0.12, 0.17), (0.88, 0.17), (0.12, 0.83), (0.88, 0.83)):
            hexes.append(((x0 + (x1 - x0) * fx) * W, (0.722 + 0.099 * fy) * H))
    himg = Image.new("L", (W, H), 0)
    hd = ImageDraw.Draw(himg)
    for (x, y) in hexes:
        r = 10 * s
        hd.regular_polygon((x, y, r), 6, rotation=rng.uniform(0, 60), fill=255)
    hexm = blur(np.asarray(himg, np.float32) / 255.0, 0.9 * s)
    hgt += hexm * 6 * s

    # --- надписи: только DRZ / BACK
    f_big = font("Oswald.ttf", 100, "Bold")
    bb = f_big.getbbox("D")
    f_big = font("Oswald.ttf", 100 * (0.30 * H) / (bb[3] - bb[1]) * 0.86, "Bold")
    m_drz = text_mask("DRZ", f_big, seam, 0.385 * H, tracking=0.035 * H, cap_ref="H")
    f_mid = font("Oswald.ttf", 0.088 * H, "SemiBold")
    m_otk = text_mask("REPLAY", f_mid, seam, 0.6 * H, tracking=0.055 * H, cap_ref="H")
    cols = np.where(m_otk.max(0) > 0.5)[0]
    lines = np.zeros((H, W), np.float32)
    ly0, ly1 = int(0.597 * H), int(0.597 * H + 5 * s)
    gap, ln = 0.03 * W, 0.125 * W
    lines[ly0:ly1, int(cols[0] - gap - ln):int(cols[0] - gap)] = 1
    lines[ly0:ly1, int(cols[-1] + gap):int(cols[-1] + gap + ln)] = 1
    m_otk = np.maximum(m_otk, lines)

    # --- маски износа
    conv = np.clip((hgt - blur(hgt, 5 * s)) / (2.5 * s), 0, 1)     # выпуклые кромки
    cav = np.clip((blur(hgt, 9 * s) - hgt) / (5 * s), 0, 1)        # впадины
    n_mid = fbm(rng, H, W, 40 * s, 5)
    n_fine = fbm(rng, H, W, 6 * s, 3)
    n_low = fbm(rng, H, W, 380 * s, 4)

    scr = Image.new("L", (W, H), 0)
    sd = ImageDraw.Draw(scr)
    for _ in range(int(900)):
        x, y = rng.uniform(0, W), rng.uniform(0, H)
        ang = rng.normal(0, 0.35) if rng.random() < 0.75 else rng.uniform(0, math.pi)
        ln = rng.uniform(8, 160) * s
        sd.line((x, y, x + math.cos(ang) * ln, y + math.sin(ang) * ln),
                fill=int(rng.uniform(60, 255)), width=1)
    scratches = blur(np.asarray(scr, np.float32) / 255.0, 0.45 * s)
    hgt -= scratches * 0.35 * s

    chips = smoothstep(0.62, 0.7, n_mid * 0.55 + conv * 0.65 + n_fine * 0.25)
    chips = np.maximum(chips, smoothstep(0.83, 0.86, n_fine) * smoothstep(0.4, 0.7, n_mid))
    chips = np.maximum(chips, scratches * 0.7)
    bottom = smoothstep(0.55, 1.0, v)

    # подтёки ржавчины под заклёпками и болтами
    streak_src = blur(rusty, 1.5 * s)
    streaks = np.zeros((H, W), np.float32)
    acc = streak_src.copy()
    for i in range(1, 9):
        sh = int(i * 11 * s)
        acc_s = np.zeros_like(acc)
        acc_s[sh:] = streak_src[:-sh] * (1 - i / 9)
        streaks = np.maximum(streaks, acc_s)
    streaks = blur(streaks, (1.2 * s, 0.6 * s)) * smoothstep(0.35, 0.75, value_noise(rng, H, W, 30 * s))
    streaks *= smoothstep(0.2, 0.7, fbm(rng, H, W, 3 * s, 2))

    rust = np.clip(chips * smoothstep(0.45, 0.9, n_low * 0.6 + bottom * 0.6) * 0.75
                   + streaks * 0.5 + cav * bottom * 0.3 * n_mid, 0, 1)

    # --- альбедо (sRGB)
    paint = np.array([0.255, 0.272, 0.262], np.float32)
    alb = np.ones((H, W, 3), np.float32) * paint
    alb *= (0.82 + 0.32 * n_low)[..., None] * (0.94 + 0.12 * n_mid)[..., None]
    sun = smoothstep(0.55, 0.9, fbm(rng, H, W, 200 * s, 3))
    alb = alb * (1 - 0.12 * sun[..., None]) + 0.12 * sun[..., None] * np.array([0.36, 0.37, 0.35])

    # полосы «зебра» на нижнем поясе — шевроном к шву
    hz = ((v > 0.872) & (v < 0.962)).astype(np.float32)
    hz = blur(hz, 1.0 * s)
    per = 0.072 * H
    sv = np.sin(2 * np.pi * (np.abs(xx - seam) + yy) / per)
    yel = smoothstep(-0.06, 0.06, sv)
    stripe = yel[..., None] * np.array([0.80, 0.58, 0.08]) + (1 - yel[..., None]) * np.array([0.055, 0.055, 0.05])
    scuff = smoothstep(0.55, 0.8, blur(rng.random((H, W)).astype(np.float32), (0.6 * s, 30 * s)) * 4 - 1.5)
    hz_paint = hz * (1 - smoothstep(0.6, 0.72, n_mid * 0.7 + n_fine * 0.3 + scuff * 0.4))
    alb = alb * (1 - hz_paint[..., None]) + stripe * hz_paint[..., None]

    def paint_layer(mask, color, wear_bias=0.0, spray=0.22):
        nonlocal alb
        spr = blur(mask, 2.4 * s) * spray
        worn = smoothstep(0.56 - wear_bias, 0.64 - wear_bias, n_mid * 0.6 + n_fine * 0.4)
        cov = np.clip(mask * (1 - worn) + spr * (1 - mask), 0, 1) * (1 - scratches * 0.6)
        cov *= 0.9 + 0.1 * n_fine
        alb = alb * (1 - cov[..., None]) + np.array(color, np.float32) * cov[..., None]
        return cov

    cov_drz = paint_layer(m_drz, (0.80, 0.78, 0.71), wear_bias=-0.07)
    cov_otk = paint_layer(m_otk, (0.80, 0.78, 0.71), wear_bias=-0.09)

    # сколы -> голая сталь, ржавчина, грязь
    steel = np.array([0.50, 0.505, 0.51]) * (0.8 + 0.3 * blur(rng.random((H, W)).astype(np.float32), (0.5, 25 * s))[..., None] * 1.6 - 0.4)
    chips_eff = chips * (1 - 0.55 * np.clip(m_drz + m_otk, 0, 1))
    alb = alb * (1 - chips_eff[..., None]) + steel * chips_eff[..., None]
    rust_col = np.array([0.42, 0.21, 0.09]) * (0.7 + 0.5 * n_fine[..., None])
    alb = alb * (1 - rust[..., None] * 0.85) + rust_col * rust[..., None] * 0.85
    grime = np.clip(cav * 0.9 + bottom * 0.35 * n_low + streaks * 0.3, 0, 1)
    alb *= (1 - 0.45 * grime[..., None]) * np.array([1.0, 0.97, 0.92]) ** grime[..., None]

    seal = smoothstep(3.2 * s, 1.5 * s, dseam)
    alb = alb * (1 - seal[..., None]) + 0.02 * seal[..., None]

    metal = np.clip(chips_eff, 0, 1)
    gloss = 0.07 + 0.08 * (cov_drz + cov_otk) + 0.08 * hz_paint - 0.05 * grime
    gloss = np.clip(gloss, 0.02, 0.3) * (1 - metal) + (0.45 + 0.25 * n_fine) * metal

    ao = np.clip(1 - 1.0 * np.clip(blur(hgt, 12 * s) - hgt, 0, None) / (10 * s), 0.35, 1)
    ao *= np.clip(1 - 0.7 * np.clip(blur(hgt, 3 * s) - hgt, 0, None) / (3 * s), 0.3, 1)

    doors = []
    for side, sl in (("L", slice(0, W // 2)), ("R", slice(W // 2, W))):
        h_ = hgt[:, sl]
        gy, gx = np.gradient(h_)
        n = np.stack([-gx, -gy, np.ones_like(gx)], -1)
        n /= np.linalg.norm(n, axis=-1, keepdims=True)
        doors.append(dict(
            side=side, x0=sl.start, w=sl.stop - sl.start,
            alb=srgb_to_lin(alb[:, sl]).astype(np.float32),
            n=n.astype(np.float32), ao=ao[:, sl].astype(np.float32),
            metal=metal[:, sl].astype(np.float32), gloss=gloss[:, sl].astype(np.float32),
            h=h_.astype(np.float32)))
    return doors


# ---------------------------------------------------------------- animation
def smoother(x):
    x = np.clip(x, 0, 1)
    return x * x * x * (x * (x * 6 - 15) + 10)


def door_progress(t):
    """0..1 — насколько разъехались створки; тяжёлый срыв, разгон, торможение."""
    if t <= T_OPEN:
        return 0.0
    u = (t - T_OPEN) / (T_OPEN_END - T_OPEN)
    jolt = 0.025 * smoother(u / 0.10)
    return float(min(1.0, jolt + (1 - 0.025) * smoother((u - 0.12) / 0.88) ** 1.15))


def lamp_level(t):
    if t < T_LIGHTS:
        return 0.0
    seq = [(0.00, 0.9), (0.05, 0.1), (0.09, 0.0), (0.16, 1.0), (0.19, 0.25), (0.23, 0.7),
           (0.26, 0.1), (0.31, 1.0), (0.40, 0.85), (0.47, 1.0)]
    dt = t - T_LIGHTS
    lvl = 1.0
    for (a, val) in seq:
        if dt >= a:
            lvl = val
    return lvl


def shake(t):
    rng = np.random.default_rng(int(t * 1000) + 3)
    amp = 0.0
    for ti, a in [(T_LIGHTS, 1.0)] + [(tb + BOLT_TRAVEL, 3.0) for tb in T_BOLTS] + \
            [(T_HISS, 2.0), (T_OPEN, 7.0), (T_OPEN_END - 0.05, 4.0)]:
        if t >= ti:
            amp += a * math.exp(-(t - ti) * 9)
    if T_OPEN < t < T_OPEN_END:
        amp += 1.4
    amp *= S
    return rng.normal(0, amp), rng.normal(0, amp * 0.6)


def beacon(t):
    if t < T_BEACON:
        return 0.0, 0.0
    spin = min(1.0, (t - T_BEACON) / 0.4)
    phi = 2 * math.pi * 1.25 * (t - T_BEACON) ** 1.0
    fade = 1 - smoother((t - T_OPEN - 0.3) / 0.8)
    return phi, spin * fade


def shade(d, offx, offy, t):
    h, w = d["h"].shape
    X = (np.arange(w, dtype=np.float32) + d["x0"] + offx)[None, :]
    Y = (np.arange(h, dtype=np.float32) + offy)[:, None]
    nx, ny, nz = d["n"][..., 0], d["n"][..., 1], d["n"][..., 2]
    alb, ao, metal, gloss = d["alb"], d["ao"], d["metal"], d["gloss"]
    Z = d["h"]

    lamp = lamp_level(t)
    phi, bea = beacon(t)
    lights = []
    if lamp > 0:
        for lx in (0.27, 0.73):
            lights.append(((lx * W, -0.32 * H, 0.70 * H), np.array([1.0, 0.95, 0.88]) * 1.9 * lamp, None))
    if bea > 0:
        sx = math.sin(phi)
        wash = (W / 2 + sx * 0.42 * W, 0.30 * W)
        lights.append(((W / 2 + sx * 0.12 * W, -0.05 * H, 0.10 * H),
                       np.array([1.0, 0.42, 0.06]) * 3.2 * bea * abs(sx) ** 1.3, wash))

    diff = np.zeros((h, w, 3), np.float32)
    spec = np.zeros((h, w, 3), np.float32)
    amb = 0.035 + 0.05 * lamp
    for (pos, col, wash) in lights:
        lx, ly, lz = pos[0] - X, pos[1] - Y, pos[2] - Z
        d2 = lx * lx + ly * ly + lz * lz
        inv = 1.0 / np.sqrt(d2)
        lx, ly, lz = lx * inv, ly * inv, lz * inv
        ndl = np.clip(nx * lx + ny * ly + nz * lz, 0, None)
        att = 1.0 / (1.0 + d2 / (1.15 * H) ** 2)
        if wash is not None:
            att = att * (0.12 + np.exp(-((X - wash[0]) / wash[1]) ** 2)) * np.exp(-Y / (0.75 * H))
        hx, hy, hz = lx, ly, lz + 1.0
        hn = 1.0 / np.sqrt(hx * hx + hy * hy + hz * hz)
        ndh = np.clip((nx * hx + ny * hy + nz * hz) * hn, 0, 1)
        p4 = ndh ** 4
        p12 = p4 * p4 * p4
        p48 = (p12 * p12) ** 2
        sp = (p12 * (1 - metal) * 0.6 + p48 * metal * 2.2) * gloss * (ndl > 0)
        a = (ndl * att)[..., None]
        diff += a * col
        spec += (sp * att)[..., None] * col
    tint = metal[..., None] * alb * 1.6 + (1 - metal[..., None]) * 1.0
    out = alb * (diff + amb * np.array([0.55, 0.65, 0.85])) * ao[..., None] + spec * tint * (0.5 + 0.5 * ao[..., None])
    return out


# ---------------------------------------------------------------- particles
def build_fx(seed=11):
    rng = np.random.default_rng(seed)
    s = S
    sprites = []
    for i in range(6):
        n = 128
        yy, xx = np.mgrid[-1:1:n * 1j, -1:1:n * 1j]
        r2 = xx ** 2 + yy ** 2
        base = np.clip(1 - r2, 0, 1) ** 2
        tex = fbm(rng, n, n, 26, 4)
        sprites.append((base * (0.35 + 0.65 * tex) ** 1.5).astype(np.float32))

    steam = []

    def emit(n, t0, t1, ox, oy, vx, vy, spread, size0, size1, life, op):
        for _ in range(n):
            ts = rng.uniform(t0, t1)
            ang = rng.normal(0, spread)
            sp = rng.uniform(0.6, 1.2)
            cvx, cvy = vx * math.cos(ang) - vy * math.sin(ang), vx * math.sin(ang) + vy * math.cos(ang)
            steam.append(dict(t0=ts, x=ox() if callable(ox) else ox, y=oy() if callable(oy) else oy,
                              vx=cvx * sp, vy=cvy * sp, s0=size0 * rng.uniform(0.7, 1.3),
                              s1=size1 * rng.uniform(0.7, 1.3), life=life * rng.uniform(0.7, 1.2),
                              op=op * rng.uniform(0.6, 1.0), spr=int(rng.integers(0, len(sprites))),
                              rot=rng.random() < 0.5))

    cx = W / 2
    # струи при сбросе давления: из шва сверху/снизу и из клапана корпуса
    for sgn in (-1, 1):
        emit(26, T_HISS, T_HISS + 0.35, cx, 0.125 * H, sgn * 1500 * s, -500 * s, 0.25, 30 * s, 260 * s, 1.5, 0.30)
        emit(22, T_HISS + 0.02, T_HISS + 0.30, cx, 0.86 * H, sgn * 1300 * s, 250 * s, 0.25, 30 * s, 240 * s, 1.4, 0.26)
    emit(30, T_HISS + 0.05, T_HISS + 0.45, 0.632 * W, 0.745 * H, 1700 * s, -150 * s, 0.18, 22 * s, 230 * s, 1.3, 0.33)
    # туман, вырывающийся из щели при открытии
    emit(110, T_OPEN, T_OPEN + 0.6, cx, lambda: rng.uniform(0.08, 0.96) * H, 0, -60 * s, math.pi,
         70 * s, 420 * s, 1.9, 0.08)

    sparks = []
    for sgn in (-1, 1):
        for _ in range(170):
            ts = rng.uniform(T_OPEN + 0.02, T_OPEN + 0.75) if rng.random() < 0.8 else rng.uniform(T_OPEN, T_OPEN + 0.12)
            ang = math.radians(rng.uniform(15, 80))
            sp = rng.uniform(350, 1250) * s
            sparks.append(dict(t0=ts, side=sgn, vx=sgn * math.cos(ang) * sp * rng.uniform(0.4, 1.1),
                               vy=-math.sin(ang) * sp, life=rng.uniform(0.18, 0.6), heat=rng.uniform(0.7, 1.3)))
    dust = [dict(t0=T_OPEN + rng.uniform(0, 0.25), x=cx + rng.normal(0, 25 * s), y=rng.uniform(0.0, 0.06) * H,
                 vx=rng.normal(0, 40 * s), vy=rng.uniform(0, 120) * s, r=rng.uniform(0.7, 1.6) * s,
                 op=rng.uniform(0.15, 0.4)) for _ in range(90)]
    motes = [dict(x=rng.uniform(0.3, 0.7) * W, y=rng.uniform(0.05, 0.95) * H, vx=rng.normal(0, 18) * s,
                  vy=rng.normal(-8, 10) * s, r=rng.uniform(0.6, 1.3) * s, op=rng.uniform(0.2, 0.6)) for _ in range(90)]
    return dict(sprites=sprites, steam=steam, sparks=sparks, dust=dust, motes=motes, cache={})


def sprite_at(i, size, rot):
    key = (i, int(size // 4), rot)
    c = FX["cache"]
    if key not in c:
        sz = max(4, int(size // 4) * 4)
        a = np.asarray(Image.fromarray(FX["sprites"][i]).resize((sz, sz), Image.BILINEAR))
        c[key] = a.T.copy() if rot else a
    return c[key]


def render_steam(t, edgeL, edgeR):
    acc = np.zeros((H, W), np.float32)
    for p in FX["steam"]:
        age = t - p["t0"]
        if age < 0 or age > p["life"]:
            continue
        k = 3.2
        f = (1 - math.exp(-k * age)) / k
        x = p["x"] + p["vx"] * f
        y = p["y"] + p["vy"] * f - 90 * S * age * age
        if p["x"] == W / 2 and p["vx"] == 0:     # туман из щели тянется вслед за кромками
            pass
        lt = age / p["life"]
        size = p["s0"] + (p["s1"] - p["s0"]) * (1 - (1 - lt) ** 2)
        op = p["op"] * smoothstep(0, 0.08, lt) * (1 - lt) ** 1.6
        spr = sprite_at(p["spr"], size, p["rot"])
        sz = spr.shape[0]
        x0, y0 = int(x - sz / 2), int(y - sz / 2)
        xa, ya, xb, yb = max(0, x0), max(0, y0), min(W, x0 + sz), min(H, y0 + sz)
        if xa >= xb or ya >= yb:
            continue
        a = spr[ya - y0:yb - y0, xa - x0:xb - x0] * op
        sl = acc[ya:yb, xa:xb]
        acc[ya:yb, xa:xb] = sl + a * (1 - sl)
    return acc


def render_sparks(t, edgeL, edgeR):
    img = Image.new("L", (W, H), 0)
    d = ImageDraw.Draw(img)
    g = 2600 * S
    any_ = False
    for p in FX["sparks"]:
        age = t - p["t0"]
        if age < 0 or age > p["life"]:
            continue
        any_ = True
        ox = (edgeL if p["side"] < 0 else edgeR)
        oy = 0.962 * H
        x = ox + p["vx"] * age
        y = oy + p["vy"] * age + 0.5 * g * age * age
        vx, vy = p["vx"], p["vy"] + g * age
        dt = 1 / 45
        lt = age / p["life"]
        val = int(255 * min(1, p["heat"]) * (1 - lt) ** 0.7)
        d.line((x, y, x - vx * dt, y - vy * dt), fill=val, width=max(1, int(2 * S)))
    if not any_:
        return None
    return np.asarray(img, np.float32) / 255.0


def render_points(t, items, mode):
    img = Image.new("L", (W, H), 0)
    d = ImageDraw.Draw(img)
    for p in items:
        if mode == "dust":
            age = t - p["t0"]
            if age < 0 or age > 1.6:
                continue
            x = p["x"] + p["vx"] * age
            y = p["y"] + p["vy"] * age + 0.5 * 900 * S * age * age
            op = p["op"] * (1 - age / 1.6)
        else:
            x = p["x"] + p["vx"] * t
            y = p["y"] + p["vy"] * t + math.sin(t * 2 + p["x"]) * 6 * S
            op = p["op"]
        r = p["r"]
        d.ellipse((x - r, y - r, x + r, y + r), fill=int(255 * op))
    return blur(np.asarray(img, np.float32) / 255.0, 0.6 * S)


def draw_bolts(rgb, t, dx, dy):
    """Три засова, проходящих через шов; уезжают в корпус справа."""
    s = S
    xl0, xr = 0.465 * W, 0.535 * W
    for i, tb in enumerate(T_BOLTS):
        pr = smoother((t - tb) / BOLT_TRAVEL) ** 0.6 if t > tb else 0.0
        xl = xl0 + (xr - xl0) * pr
        if xr - xl < 1:
            continue
        yc = (0.742 + 0.028 * i) * H + dy
        r = 10 * s
        y0, y1 = int(yc - r), int(yc + r) + 1
        x0, x1 = int(xl + dx), int(xr + dx)
        ys = (np.arange(y0, y1, dtype=np.float32) - yc) / r
        ys = np.clip(ys, -1, 1)
        nzc = np.sqrt(1 - ys ** 2)
        lum = 0.05 + 0.75 * np.clip(-ys * 0.55 + nzc * 0.7, 0, None) ** 1.5 + 1.6 * np.exp(-((ys + 0.5) / 0.13) ** 2)
        lum *= lamp_level(t) * 0.9 + 0.1
        col = np.array([0.42, 0.43, 0.45], np.float32)[None, :] * lum[:, None]
        cover = np.clip(1 - np.abs(ys) ** 8, 0, 1)
        seg = rgb[y0:y1, x0:x1]
        seg[:] = seg * (1 - cover[:, None, None]) + col[:, None, :] * cover[:, None, None]
        sy0, sy1 = int(yc + r), int(yc + r + 8 * s)
        rgb[sy0:sy1, x0:x1] *= np.linspace(0.35, 1, sy1 - sy0, dtype=np.float32)[:, None, None]


def lamp_glow(rgb, alpha, t, dx, dy):
    """Индикатор на корпусе: красный, после сброса давления — зелёный."""
    s = S
    x, y = 0.6 * W + dx, 0.738 * H + dy
    on_green = t >= T_HISS
    col = np.array([0.15, 1.0, 0.35]) if on_green else np.array([1.0, 0.08, 0.04])
    blink = 1.0 if on_green or int(t * 6) % 2 == 0 else 0.25
    if t < T_LIGHTS:
        blink = 0.6
    R = int(60 * s)
    y0, y1 = max(0, int(y) - R), min(H, int(y) + R)
    x0, x1 = max(0, int(x) - R), min(W, int(x) + R)
    if x0 >= x1 or y0 >= y1:
        return
    yy, xx = np.mgrid[y0:y1, x0:x1].astype(np.float32)
    d = np.sqrt((xx - x) ** 2 + (yy - y) ** 2)
    core = smoothstep(7 * s, 5 * s, d) * 4.0
    halo = np.exp(-d / (9 * s)) * 0.9
    g = (core + halo) * blink
    rgb[y0:y1, x0:x1] += g[..., None] * col * alpha[y0:y1, x0:x1, None]


def render_frame(fi, fps):
    t = fi / fps
    s = S
    dx, dy = shake(t)
    p = door_progress(t)
    maxoff = W / 2 + 140 * s
    pv = door_progress(t + 0.5 / fps) - door_progress(t - 0.5 / fps)
    vel = pv * maxoff                                  # пикселей за кадр
    rgb = np.zeros((H, W, 3), np.float32)
    alpha = np.zeros((H, W), np.float32)
    edges = []
    for d in DOORS:
        sgn = -1 if d["side"] == "L" else 1
        offx = sgn * p * maxoff + dx
        col = shade(d, offx, dy, t)
        x0 = d["x0"] + int(round(offx))
        y0 = int(round(dy))
        xa, xb = max(0, x0), min(W, x0 + d["w"])
        ya, yb = max(0, y0), min(H, y0 + H)
        lay = np.zeros((H, W, 3), np.float32)
        la = np.zeros((H, W), np.float32)
        if xa < xb:
            lay[ya:yb, xa:xb] = col[ya - y0:yb - y0, xa - x0:xb - x0]
            la[ya:yb, xa:xb] = 1.0
            if y0 > 0:                                 # тряска — не оставляем щель сверху
                lay[:y0, xa:xb] = lay[y0:y0 + 1, xa:xb]
                la[:y0, xa:xb] = 1
            if yb < H:
                lay[yb:, xa:xb] = lay[yb - 1:yb, xa:xb]
                la[yb:, xa:xb] = 1
        if vel > 1.0:
            r = int(vel * 0.55)
            lay = ndimage.uniform_filter1d(lay, 2 * r + 1, axis=1, mode="constant")
            la = ndimage.uniform_filter1d(la, 2 * r + 1, axis=1, mode="constant")
        edge = (x0 + d["w"]) if d["side"] == "L" else x0
        edges.append(edge)
        # подсветка внутренней кромки светом из проёма
        if p > 0:
            X = np.arange(W, dtype=np.float32)
            dist = (edge - X) if d["side"] == "L" else (X - edge)
            rim = np.exp(-np.clip(dist, 0, None) / (3 * s)) * (dist >= 0) * min(1, p * 30) * (1 - p) ** 0.8
            lay += (rim * 0.45)[None, :, None] * np.array([1.0, 0.9, 0.78]) * la[..., None]
        rgb = rgb * (1 - la[..., None]) + lay
        alpha = alpha * (1 - la) + la
    if t < T_OPEN + 0.05:
        draw_bolts(rgb, t, dx, dy)
    if t < T_OPEN_END:
        lamp_glow(rgb, alpha, t, dx + (0 if p == 0 else p * maxoff), dy)

    edgeL, edgeR = edges
    X = np.arange(W, dtype=np.float32)
    if p > 0:
        # тень створок на то, что за ними + световая дымка в проёме
        sh = 0.6 * np.exp(-np.clip(X - edgeL, 0, None) / (34 * s)) * (X >= edgeL) + \
            0.6 * np.exp(-np.clip(edgeR - X, 0, None) / (34 * s)) * (X <= edgeR)
        sh = np.clip(sh, 0, 0.75)[None, :]
        gap = max(1.0, edgeR - edgeL)
        haze = (np.exp(-np.abs(X - edgeL) / (60 * s)) + np.exp(-np.abs(X - edgeR) / (60 * s)))
        haze = haze * ((X > edgeL) & (X < edgeR)) * 0.07 * (1 - p) ** 1.5
        hz = haze[None, :] * np.ones((H, 1), np.float32)
        a_new = np.broadcast_to(sh, (H, W)) * (1 - alpha)
        alpha = alpha + a_new
        rgb += hz[..., None] * np.array([1.0, 0.9, 0.75]) * 0.9
        alpha = alpha + hz * (1 - alpha)

    # маячок: блик сверху по центру
    phi, bea = beacon(t)
    if bea > 0:
        face = max(0.0, math.cos(phi)) ** 6 * bea
        if face > 0.002:
            yy = np.arange(H, dtype=np.float32)[:, None]
            r2 = (X[None, :] - W / 2 - dx) ** 2 + (yy + 0.03 * H) ** 2
            fl = face * (1.8 * np.exp(-r2 / (2 * (0.07 * H) ** 2)) + 0.5 * np.exp(-np.sqrt(r2) / (0.28 * H)))
            rgb += fl[..., None] * np.array([1.0, 0.45, 0.08]) * alpha[..., None]

    # пар
    st = render_steam(t, edgeL, edgeR)
    if st.max() > 0:
        lamp = lamp_level(t)
        scol = np.array([0.55, 0.57, 0.60]) * (0.25 + 0.75 * lamp) + np.array([0.25, 0.10, 0.02]) * beacon(t)[1]
        rgb = rgb * (1 - st[..., None]) + st[..., None] * scol
        alpha = alpha + st * (1 - alpha)

    # искры, пыль, пылинки в свете
    sp = render_sparks(t, edgeL, edgeR)
    if sp is not None:
        glow = blur(sp, 3 * s) * 2.5
        e = sp * 5 + glow
        rgb += e[..., None] * np.array([1.0, 0.55, 0.18])
        alpha = np.maximum(alpha, np.clip(e, 0, 1))
    if t >= T_OPEN:
        du = render_points(t, FX["dust"], "dust")
        rgb = rgb * (1 - du[..., None] * 0.8) + du[..., None] * 0.8 * np.array([0.45, 0.42, 0.38])
        alpha = alpha + du * 0.8 * (1 - alpha)
        mo = render_points(t, FX["motes"], "motes")
        gapmask = ((X > edgeL) & (X < edgeR)).astype(np.float32)[None, :] * (1 - p) ** 1.2
        mo = mo * gapmask * 0.35
        rgb += mo[..., None] * np.array([1.0, 0.95, 0.85]) * 1.5
        alpha = alpha + mo * (1 - alpha)

    # bloom
    bright = np.clip(rgb - 0.85, 0, None)
    if bright.max() > 0:
        bl = blur(bright, 5 * s) * 0.5 + blur(bright, 22 * s) * 0.55
        rgb += bl
        alpha = np.clip(alpha + np.clip(bl.max(-1), 0, 1) * (1 - alpha), 0, 1)

    # проявка из темноты + виньетка + плёнка
    fade = smoothstep(0.0, 0.25, t)
    a = np.clip(alpha, 1e-4, 1)
    straight = rgb / a[..., None] * fade
    yy = (np.arange(H, dtype=np.float32)[:, None] / H - 0.5) * 2
    xn = (X[None, :] / W - 0.5) * 2
    vig = 1 - 0.42 * np.clip((xn ** 2 * 0.8 + yy ** 2) / 1.6, 0, 1) ** 1.3
    straight *= vig[..., None]
    disp = lin_to_srgb(aces(straight * 1.25))
    disp = disp ** np.array([1.02, 1.0, 0.97])               # чуть тёплые света, холодные тени
    grain = np.random.default_rng(fi).normal(0, 0.016, (H, W, 1)).astype(np.float32)
    disp = np.clip(disp + grain * (alpha[..., None] > 0.02), 0, 1)
    if fi == 0:
        alpha = np.ones_like(alpha)
        disp = disp * 0
    a8 = (np.clip(alpha, 0, 1) * 255 + 0.5).astype(np.uint8)
    c8 = (disp * 255 + 0.5).astype(np.uint8)
    c8[a8 == 0] = 0
    return c8.tobytes(), a8.tobytes()


# ---------------------------------------------------------------- audio
SR = 48000


def load(name, speed=1.0):
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", os.path.join(SOUNDS, name), "-f", "f32le",
                          "-ac", "2", "-ar", str(SR), "-"], capture_output=True, check=True).stdout
    a = np.frombuffer(raw, np.float32).reshape(-1, 2).astype(np.float64)
    if speed != 1.0:
        n = int(len(a) / speed)
        src = np.arange(n) * speed
        a = np.stack([np.interp(src, np.arange(len(a)), a[:, c]) for c in range(2)], -1)
    return a


def db(x):
    return 10 ** (x / 20)


def build_sfx(path):
    total = int((DURATION + 2.6) * SR)
    mix = np.zeros((total, 2))
    rng = np.random.default_rng(5)

    def place(sig, t, gain=0.0, fin=0.0, fout=0.0, start=0.0, length=None, pan=0.0):
        sig = sig[int(start * SR):]
        if length is not None:
            sig = sig[:int(length * SR)]
        sig = sig.copy() * db(gain)
        n = len(sig)
        if fin > 0:
            k = min(n, int(fin * SR))
            sig[:k] *= np.linspace(0, 1, k)[:, None]
        if fout > 0:
            k = min(n, int(fout * SR))
            sig[n - k:] *= np.linspace(1, 0, k)[:, None] ** 2
        if pan:
            sig[:, 0] *= min(1, 1 - pan)
            sig[:, 1] *= min(1, 1 + pan)
        i0 = int(t * SR)
        if i0 < 0:
            sig = sig[-i0:]
            i0 = 0
        i1 = min(total, i0 + len(sig))
        mix[i0:i1] += sig[:i1 - i0]

    def thump(f0, dur, decay=7.0):
        tt = np.arange(int(dur * SR)) / SR
        ph = 2 * np.pi * np.cumsum(f0 * (1 + 1.2 * np.exp(-tt * 28))) / SR
        env = (1 - np.exp(-tt * 400)) * np.exp(-tt * decay)
        x = np.sin(ph) * env
        return np.stack([x, x], -1)

    def lowpass(x, fc, order=2):
        b, a = signal.butter(order, fc / (SR / 2), "low")
        return signal.lfilter(b, a, x, axis=0)

    def noise(dur):
        return rng.standard_normal((int(dur * SR), 2))

    # фон помещения
    place(lowpass(load("room_tone.wav"), 900), 0.0, -17, fin=0.5, fout=1.2, length=DURATION + 0.6)
    # рубильник + лампы
    place(load("lightclunk1.wav", 0.72), T_LIGHTS - 0.06, -3)
    place(load("clank1.wav", 0.55), T_LIGHTS - 0.08, -9)
    place(thump(52, 0.6), T_LIGHTS, -9)
    tt = np.arange(int((DURATION - T_LIGHTS) * SR)) / SR
    hum = sum(np.sin(2 * np.pi * 100 * k * tt + k) / k ** 1.3 for k in range(1, 9))
    flick = np.array([lamp_level(T_LIGHTS + x) for x in tt[::480]])
    flick = np.repeat(flick, 480)[:len(tt)]
    hum = hum * flick * (1 + 0.15 * rng.standard_normal(len(tt)))
    hum = np.stack([hum, hum * 0.9], -1)
    place(lowpass(hum, 3000), T_LIGHTS, -34, fout=0.8)
    # маячок
    place(load("rotating.wav", 0.7), T_BEACON - 0.05, -16)
    mt = np.arange(int((T_OPEN + 1.0 - T_BEACON) * SR)) / SR
    motor = signal.sawtooth(2 * np.pi * 38 * mt) * (0.7 + 0.3 * np.sin(2 * np.pi * 1.25 * mt))
    place(lowpass(np.stack([motor, motor], -1), 400), T_BEACON, -36, fin=0.3, fout=0.6)
    # засовы: пик удара совпадает с концом хода
    for i, tb in enumerate(T_BOLTS):
        hit = tb + BOLT_TRAVEL
        name, pk, spd = [("mechanical1.wav", 0.34, 0.82), ("mechanical2.wav", 0.44, 0.78), ("clank1.wav", 0.17, 0.62)][i]
        place(load(name, spd), hit - pk / spd, -2, pan=(i - 1) * 0.15)
        place(load("clank1.wav", 0.5 + 0.06 * i), hit - 0.17 / (0.5 + 0.06 * i), -12)
        place(thump(58 - 4 * i, 0.5, 9), hit, -8)
    # сброс давления
    place(load("steam_release.wav"), T_HISS - 0.05, -1, length=1.45, fout=0.5)
    place(lowpass(noise(1.6), 7000) * 0.25, T_HISS, -14, fin=0.01, fout=1.2)
    place(load("steam_long.wav"), T_OPEN + 0.1, -10, start=0.3, length=2.0, fin=0.2, fout=1.2)
    # створки
    place(load("iron_door.ogg", 0.55), T_OPEN - 0.36 / 0.55 + 0.02, -2)
    place(thump(42, 1.4, 3.5), T_OPEN, -3)
    place(load("door_start.wav"), T_OPEN - 0.18, 0, fout=0.45, length=1.30)
    place(load("door_finish.wav"), T_OPEN_END - 0.90, -1, fin=0.25)
    rd = T_OPEN_END - T_OPEN + 0.3
    rt = np.arange(int(rd * SR)) / SR
    vel_env = np.array([door_progress(T_OPEN + x + 0.02) - door_progress(T_OPEN + x) for x in rt[::480]])
    vel_env = np.repeat(vel_env / (vel_env.max() + 1e-9), 480)[:len(rt)]
    rumble = lowpass(noise(rd), 140, 4) * vel_env[:, None] * 3.0
    grind = lowpass(signal.lfilter([1], [1, -0.97], noise(rd), axis=0), 2500) * vel_env[:, None] * 0.04
    place(rumble, T_OPEN, -4)
    place(grind, T_OPEN, -16)
    place(load("clank1.wav", 0.45), T_OPEN_END - 0.05 - 0.17 / 0.45, -5)
    place(thump(38, 1.2, 4), T_OPEN_END - 0.05, -5)

    # реверберация ангара
    ir_len = int(2.4 * SR)
    it = np.arange(ir_len) / SR
    ir = rng.standard_normal((ir_len, 2)) * np.exp(-it / 0.42)[:, None]
    ir = lowpass(ir, 4200)
    for d_, g in ((0.017, 0.5), (0.031, 0.35), (0.047, 0.3), (0.071, 0.22)):
        ir[int(d_ * SR)] += g
    ir /= np.sqrt((ir ** 2).sum(0))
    wet = np.stack([signal.fftconvolve(mix[:, c], ir[:, c])[:total] for c in range(2)], -1)
    out = mix + wet * db(-6)
    b, a = signal.butter(2, 28 / (SR / 2), "high")
    out = signal.lfilter(b, a, out, axis=0)
    out = np.tanh(out / np.abs(out).max() * 1.6) / math.tanh(1.6) * db(-1)
    # хвост к концу затухает
    fade_n = int(1.0 * SR)
    out[-fade_n:] *= np.linspace(1, 0, fade_n)[:, None] ** 2
    pcm = (out * 32767).astype(np.int16)
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "s16le", "-ar", str(SR), "-ac", "2", "-i", "-", path],
                   input=pcm.tobytes(), check=True)


# ---------------------------------------------------------------- main
def init_worker():
    pass


def _frame(args):
    return render_frame(*args)


def main():
    global W, H, S, DOORS, FX
    ap = argparse.ArgumentParser()
    ap.add_argument("--size", default="1920x1080")
    ap.add_argument("--fps", type=int, default=30)
    ap.add_argument("--out", default=os.path.join(HERE, "..", "src", "assets", "intro"))
    ap.add_argument("--still", help="через запятую: секунды для PNG-превью (поверх --bg)")
    ap.add_argument("--bg", help="картинка-фон для превью")
    ap.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) - 1))
    ap.add_argument("--no-sfx", action="store_true")
    a = ap.parse_args()
    W, H = map(int, a.size.lower().split("x"))
    S = H / 1080.0
    os.makedirs(a.out, exist_ok=True)

    print("строю ворота…", flush=True)
    DOORS = build_gate()
    FX = build_fx()

    if a.still:
        bg = None
        if a.bg:
            bg = np.asarray(Image.open(a.bg).convert("RGB").resize((W, H)), np.float32) / 255
        for ts in a.still.split(","):
            fi = int(round(float(ts) * a.fps))
            c, al = render_frame(fi, a.fps)
            c = np.frombuffer(c, np.uint8).reshape(H, W, 3) / 255.0
            al = np.frombuffer(al, np.uint8).reshape(H, W, 1) / 255.0
            base = bg if bg is not None else np.full((H, W, 3), 0.5)
            img = c * al + base * (1 - al)
            p = os.path.join(a.out, f"still_{float(ts):.2f}.png")
            Image.fromarray((img * 255).astype(np.uint8)).save(p)
            print(p)
        return

    if not a.no_sfx:
        print("свожу звук…", flush=True)
        build_sfx(os.path.join(a.out, "intro_sfx.wav"))

    n = int(round(DURATION * a.fps))
    cpath = os.path.join(a.out, "gate_color.mp4")
    apath = os.path.join(a.out, "gate_alpha.mkv")
    common = ["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-s", f"{W}x{H}", "-r", str(a.fps)]
    pc = subprocess.Popen(common + ["-pix_fmt", "rgb24", "-i", "-", "-c:v", "libx264", "-preset", "slow",
                                    "-crf", "17", "-pix_fmt", "yuv444p", cpath], stdin=subprocess.PIPE)
    pa = subprocess.Popen(common + ["-pix_fmt", "gray", "-i", "-", "-c:v", "ffv1", apath], stdin=subprocess.PIPE)
    print(f"рендер {n} кадров {W}x{H} в {a.jobs} потоков…", flush=True)
    with Pool(a.jobs) as pool:
        for i, (c, al) in enumerate(pool.imap(_frame, [(fi, a.fps) for fi in range(n)], chunksize=1)):
            pc.stdin.write(c)
            pa.stdin.write(al)
            if i % 10 == 0:
                print(f"  кадр {i}/{n}", flush=True)
    pc.stdin.close(); pa.stdin.close()
    pc.wait(); pa.wait()
    meta = dict(width=W, height=H, fps=a.fps, duration=n / a.fps, replay_start=T_OPEN, version=1)
    with open(os.path.join(a.out, "intro.json"), "w") as f:
        json.dump(meta, f, indent=2)
    print("готово:", a.out)


if __name__ == "__main__":
    main()
