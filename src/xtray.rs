//! Tray icon for desktops without StatusNotifier (LXDE, older Xfce, i3bar-less setups…): the classic
//! XEmbed system tray. Left or middle click saves a replay, right click opens the menu.
use crate::tray::{Act, View};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{channel, Receiver, Sender};
use x11rb::connection::Connection;
use x11rb::protocol::render::{self, ConnectionExt as _, PictOp, Pictformat};
use x11rb::protocol::xproto::{self, ConnectionExt as _, *};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

const ICON: &[u8] = include_bytes!("../assets/icons/drzreplay-64.png");
const ICON_REC: &[u8] = include_bytes!("../assets/icons/drzreplay-rec-64.png");
const INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Medium.ttf");

pub struct XTray {
    tx: Sender<View>,
}

impl XTray {
    pub fn update(&self, v: View) {
        let _ = self.tx.send(v);
    }
}

/// Dock an icon in the system tray; Err when there is no tray on this desktop.
pub fn spawn(acts: Sender<Act>, view: View) -> Result<XTray, String> {
    let (conn, screen) = RustConnection::connect(None).map_err(|e| e.to_string())?;
    let sel = intern(&conn, &format!("_NET_SYSTEM_TRAY_S{screen}"))?;
    let owner = conn.get_selection_owner(sel).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.owner;
    if owner == x11rb::NONE {
        return Err("no system tray on this desktop".into());
    }
    let (tx, rx) = channel::<View>();
    let (ready_tx, ready_rx) = channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let mut t = match Tray::new(conn, screen, owner, acts, view) {
            Ok(t) => {
                let _ = ready_tx.send(Ok(()));
                t
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        t.run(rx);
    });
    ready_rx.recv().map_err(|e| e.to_string())??;
    Ok(XTray { tx })
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, String> {
    Ok(conn.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
}

// ---------------------------------------------------------------- the menu
enum Item {
    Entry { label: String, enabled: bool, act: Act, check: Option<bool> },
    Separator,
}

fn menu_items(v: &View) -> Vec<Item> {
    let e = |label: &str, enabled: bool, act: Act| Item::Entry { label: label.to_string(), enabled, act, check: None };
    let mut items = vec![];
    if let Some(l) = &v.update_label {
        items.push(e(l, true, Act::Update));
    }
    items.push(e(&v.save_label, v.save_enabled, Act::Save));
    items.push(e(&v.record_label, v.record_enabled, Act::ToggleRecord));
    items.push(Item::Separator);
    items.push(e(&v.pause_label, true, Act::TogglePause));
    items.push(Item::Entry { label: "Intro".into(), enabled: true, act: Act::ToggleIntro(!v.intro), check: Some(v.intro) });
    items.push(Item::Separator);
    items.push(e("Open folder", true, Act::OpenFolder));
    items.push(e("Settings", true, Act::OpenSettings));
    items.push(Item::Separator);
    items.push(e("Restart", true, Act::Restart));
    items.push(e("Quit", true, Act::Quit));
    items
}

const BG: [u8; 3] = [0x2b, 0x2b, 0x2f];
const BORDER: [u8; 3] = [0x3f, 0x3f, 0x45];
const HOVER: [u8; 3] = [0x3d, 0x3d, 0x44];
const TEXT: [u8; 3] = [0xe8, 0xe8, 0xeb];
const DIM: [u8; 3] = [0x78, 0x78, 0x7e];
const ROW: i32 = 25;
const SEP: i32 = 7;
const WIDTH: i32 = 170;
const FONT: f32 = 13.0;
const PAD: i32 = 4; // top and bottom
const TEXT_X: i32 = 28;

struct Canvas {
    w: i32,
    h: i32,
    px: Vec<u8>, // B G R 0 per pixel, the layout of a 24-bit ZPixmap
}

impl Canvas {
    fn new(w: i32, h: i32, c: [u8; 3]) -> Canvas {
        let mut px = vec![0u8; (w * h * 4) as usize];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[c[2], c[1], c[0], 0]);
        }
        Canvas { w, h, px }
    }
    fn blend(&mut self, x: i32, y: i32, c: [u8; 3], a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        for k in 0..3 {
            let d = self.px[i + 2 - k] as f32;
            self.px[i + 2 - k] = (c[k] as f32 * a + d * (1.0 - a)).round() as u8;
        }
    }
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
        for y in y0.max(0)..y1.min(self.h) {
            for x in x0.max(0)..x1.min(self.w) {
                self.blend(x, y, c, 1.0);
            }
        }
    }
    /// a filled rectangle with rounded corners (anti-aliased)
    fn round_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, r: f32, c: [u8; 3]) {
        let (cx, cy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
        let (hx, hy) = ((x1 - x0) as f32 / 2.0, (y1 - y0) as f32 / 2.0);
        for y in y0.max(0)..y1.min(self.h) {
            for x in x0.max(0)..x1.min(self.w) {
                let (qx, qy) = ((x as f32 + 0.5 - cx).abs() - (hx - r), (y as f32 + 0.5 - cy).abs() - (hy - r));
                let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                self.blend(x, y, c, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }
    fn text(&mut self, font: &FontRef, x: f32, y_mid: f32, size: f32, s: &str, c: [u8; 3]) {
        let scale = PxScale::from(size * font.height_unscaled() / font.units_per_em().unwrap_or(1000.0));
        let sf = font.as_scaled(scale);
        let base = y_mid - (sf.ascent() + sf.descent()) / 2.0;
        let mut cx = x;
        let mut prev = None;
        for ch in s.chars() {
            let id = font.glyph_id(ch);
            if let Some(p) = prev {
                cx += sf.kern(p, id);
            }
            if let Some(o) = font.outline_glyph(id.with_scale_and_position(scale, ab_glyph::point(cx, base + sf.ascent()))) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| self.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, c, cov));
            }
            cx += sf.h_advance(id);
            prev = Some(id);
        }
    }
}

fn text_width(font: &FontRef, size: f32, s: &str) -> f32 {
    let scale = PxScale::from(size * font.height_unscaled() / font.units_per_em().unwrap_or(1000.0));
    let sf = font.as_scaled(scale);
    s.chars().map(|c| sf.h_advance(font.glyph_id(c))).sum()
}

struct Menu {
    win: Window,
    gc: Gcontext,
    x: i32,
    y: i32,
    items: Vec<Item>,
    rows: Vec<(i32, i32)>, // top, height of every item
    hover: Option<usize>,
    h: i32,
    w: i32,
}

// ---------------------------------------------------------------- the icon
struct Tray {
    conn: RustConnection,
    screen: usize,
    acts: Sender<Act>,
    view: View,
    win: Window,
    size: (u16, u16),
    gc: Gcontext,
    src_fmt: Pictformat,
    dst_fmt: Pictformat,
    icons: [image::RgbaImage; 2],
    menu: Option<Menu>,
    font: FontRef<'static>,
    screen_size: (i32, i32),
}

impl Tray {
    fn new(conn: RustConnection, screen: usize, owner: Window, acts: Sender<Act>, view: View) -> Result<Tray, String> {
        let e = |e: &dyn std::fmt::Display| e.to_string();
        let (root, root_visual, root_depth, sw, sh) = {
            let s = &conn.setup().roots[screen];
            (s.root, s.root_visual, s.root_depth, s.width_in_pixels as i32, s.height_in_pixels as i32)
        };
        // the tray may ask for icons with a 32-bit (alpha) visual: a window of another depth can't be
        // moved into it. Without the hint, an ordinary window that shows the panel through itself.
        let vis_atom = intern(&conn, "_NET_SYSTEM_TRAY_VISUAL")?;
        let wanted: Option<u32> = conn
            .get_property(false, owner, vis_atom, AtomEnum::VISUALID, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()));
        let alpha = wanted.filter(|v| *v != root_visual).and_then(|v| {
            conn.setup().roots[screen].allowed_depths.iter().find(|d| d.depth == 32 && d.visuals.iter().any(|x| x.visual_id == v)).map(|_| v)
        });
        let win = conn.generate_id().map_err(|x| e(&x))?;
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY;
        let icon_visual = if let Some(v) = alpha {
            let cmap = conn.generate_id().map_err(|x| e(&x))?;
            conn.create_colormap(ColormapAlloc::NONE, cmap, root, v).map_err(|x| e(&x))?;
            conn.create_window(32, win, root, 0, 0, 24, 24, 0, WindowClass::INPUT_OUTPUT, v, &CreateWindowAux::new().colormap(cmap).border_pixel(0).background_pixel(0).event_mask(events))
                .map_err(|x| e(&x))?;
            v
        } else {
            conn.create_window(
                root_depth,
                win,
                root,
                0,
                0,
                24,
                24,
                0,
                WindowClass::INPUT_OUTPUT,
                root_visual,
                &CreateWindowAux::new().background_pixmap(BackPixmap::PARENT_RELATIVE).event_mask(events),
            )
            .map_err(|x| e(&x))?;
            root_visual
        };
        let name = b"DrzReplay";
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, name).map_err(|x| e(&x))?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"drzreplay\0drzreplay\0").map_err(|x| e(&x))?;
        let xembed_info = intern(&conn, "_XEMBED_INFO")?;
        conn.change_property32(PropMode::REPLACE, win, xembed_info, xembed_info, &[0, 1]).map_err(|x| e(&x))?;
        let opcode = intern(&conn, "_NET_SYSTEM_TRAY_OPCODE")?;
        let msg = ClientMessageEvent::new(32, owner, opcode, [CURRENT_TIME, 0, win, 0, 0]); // 0 = request dock
        conn.send_event(false, owner, EventMask::NO_EVENT, msg).map_err(|x| e(&x))?;
        let gc = conn.generate_id().map_err(|x| e(&x))?;
        conn.create_gc(gc, win, &CreateGCAux::new()).map_err(|x| e(&x))?;

        // picture formats for XRender: the window's own, and 32-bit ARGB for the icon
        let formats = conn.render_query_pict_formats().map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;
        let dst_fmt = formats
            .screens
            .get(screen)
            .and_then(|s| s.depths.iter().flat_map(|d| d.visuals.iter()).find(|v| v.visual == icon_visual))
            .map(|v| v.format)
            .ok_or("no picture format")?;
        let src_fmt = formats
            .formats
            .iter()
            .find(|f| f.depth == 32 && f.type_ == render::PictType::DIRECT && f.direct.alpha_mask == 255 && f.direct.alpha_shift == 24 && f.direct.red_shift == 16)
            .map(|f| f.id)
            .ok_or("no ARGB picture format")?;
        let load = |b: &[u8]| image::load_from_memory(b).map(|i| i.to_rgba8()).map_err(|x| x.to_string());
        let t = Tray {
            conn,
            screen,
            acts,
            view,
            win,
            size: (24, 24),
            gc,
            src_fmt,
            dst_fmt,
            icons: [load(ICON)?, load(ICON_REC)?],
            menu: None,
            font: FontRef::try_from_slice(INTER).map_err(|x| x.to_string())?,
            screen_size: (sw, sh),
        };
        if std::env::var_os("DRZREPLAY_DEBUG").is_some() {
            eprintln!("xtray: window {win:#x} visual {icon_visual:#x} manager {owner:#x} wanted {wanted:?}");
        }
        // not mapped here: the tray maps it once embedded (_XEMBED_INFO), mapping it first would let the
        // window manager adopt it as an ordinary window
        t.conn.flush().map_err(|x| e(&x))?;
        Ok(t)
    }

    fn draw_icon(&self) {
        let (w, h) = (self.size.0.max(1), self.size.1.max(1));
        let side = w.min(h);
        let icon = &self.icons[self.view.recording as usize];
        let scaled = image::imageops::resize(icon, side as u32, side as u32, image::imageops::FilterType::Triangle);
        // premultiplied BGRA, what XRender expects
        let mut data = Vec::with_capacity(side as usize * side as usize * 4);
        for p in scaled.pixels() {
            let a = p[3] as u32;
            data.extend_from_slice(&[(p[2] as u32 * a / 255) as u8, (p[1] as u32 * a / 255) as u8, (p[0] as u32 * a / 255) as u8, p[3]]);
        }
        let c = &self.conn;
        let (Ok(pm), Ok(src), Ok(dst), Ok(gc)) = (c.generate_id(), c.generate_id(), c.generate_id(), c.generate_id()) else { return };
        let root = c.setup().roots[self.screen].root;
        let _ = c.create_pixmap(32, pm, root, side, side);
        let _ = c.create_gc(gc, pm, &CreateGCAux::new());
        let _ = c.put_image(ImageFormat::Z_PIXMAP, pm, gc, side, side, 0, 0, 0, 32, &data);
        let _ = c.render_create_picture(src, pm, self.src_fmt, &Default::default());
        let _ = c.render_create_picture(dst, self.win, self.dst_fmt, &Default::default());
        let _ = c.clear_area(false, self.win, 0, 0, 0, 0); // the panel's background shows through
        let (ox, oy) = (((w - side) / 2) as i16, ((h - side) / 2) as i16);
        let _ = c.render_composite(PictOp::OVER, src, x11rb::NONE, dst, 0, 0, 0, 0, ox, oy, side, side);
        let _ = c.render_free_picture(src);
        let _ = c.render_free_picture(dst);
        let _ = c.free_gc(gc);
        let _ = c.free_pixmap(pm);
        let _ = c.flush();
    }

    // ------------------------------------------------------------ menu
    fn open_menu(&mut self, px: i32, py: i32) {
        self.close_menu();
        let items = menu_items(&self.view);
        let mut rows = vec![];
        let mut y = PAD;
        for it in &items {
            let h = if matches!(it, Item::Separator) { SEP } else { ROW };
            rows.push((y, h));
            y += h;
        }
        let h = y + PAD;
        let (sw, sh) = self.screen_size;
        let widest = items
            .iter()
            .filter_map(|i| if let Item::Entry { label, .. } = i { Some(text_width(&self.font, FONT, label)) } else { None })
            .fold(0.0f32, f32::max);
        let wd = (widest.ceil() as i32 + TEXT_X + 16).max(WIDTH);
        let x = (px - wd / 2).clamp(0, (sw - wd).max(0));
        let yy = if py + h > sh { (py - h).max(0) } else { py };
        let c = &self.conn;
        let s = &c.setup().roots[self.screen];
        let (Ok(win), Ok(gc)) = (c.generate_id(), c.generate_id()) else { return };
        let ok = c
            .create_window(
                s.root_depth,
                win,
                s.root,
                x as i16,
                yy as i16,
                wd as u16,
                h as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                s.root_visual,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS | EventMask::LEAVE_WINDOW),
            )
            .is_ok();
        if !ok {
            return;
        }
        let _ = c.create_gc(gc, win, &CreateGCAux::new());
        let _ = c.map_window(win);
        let _ = c.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let _ = c.grab_pointer(true, win, EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION, GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, x11rb::NONE, CURRENT_TIME);
        let _ = c.grab_keyboard(true, win, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC);
        let _ = c.flush();
        self.menu = Some(Menu { win, gc, x, y: yy, items, rows, hover: None, h, w: wd });
        self.draw_menu();
    }

    fn close_menu(&mut self) {
        if let Some(m) = self.menu.take() {
            let c = &self.conn;
            let _ = c.ungrab_pointer(CURRENT_TIME);
            let _ = c.ungrab_keyboard(CURRENT_TIME);
            let _ = c.free_gc(m.gc);
            let _ = c.destroy_window(m.win);
            let _ = c.flush();
        }
    }

    fn draw_menu(&self) {
        let Some(m) = &self.menu else { return };
        let mut cv = Canvas::new(m.w, m.h, BORDER);
        cv.rect(1, 1, m.w - 1, m.h - 1, BG);
        for (i, (it, (top, h))) in m.items.iter().zip(&m.rows).enumerate() {
            match it {
                Item::Separator => cv.rect(10, top + h / 2, m.w - 10, top + h / 2 + 1, BORDER),
                Item::Entry { label, enabled, check, .. } => {
                    if m.hover == Some(i) && *enabled {
                        cv.round_rect(4, *top, m.w - 4, top + h, 4.0, HOVER);
                    }
                    let col = if *enabled { TEXT } else { DIM };
                    if *check == Some(true) {
                        // a tick, drawn by hand (the font has no such glyph)
                        let (cx, cy) = (11, top + h / 2);
                        for k in 0..3 {
                            cv.rect(cx + k, cy + k - 1, cx + k + 2, cy + k + 1, col);
                        }
                        for k in 0..5 {
                            cv.rect(cx + 3 + k, cy + 2 - k, cx + 5 + k, cy + 4 - k, col);
                        }
                    }
                    cv.text(&self.font, TEXT_X as f32, (top + h / 2) as f32, FONT, label, col);
                }
            }
        }
        let c = &self.conn;
        let _ = c.put_image(ImageFormat::Z_PIXMAP, m.win, m.gc, m.w as u16, m.h as u16, 0, 0, 0, c.setup().roots[self.screen].root_depth, &cv.px);
        let _ = c.flush();
    }

    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        let m = self.menu.as_ref()?;
        if x < 0 || x >= m.w {
            return None;
        }
        m.rows.iter().position(|(t, h)| y >= *t && y < t + h).filter(|i| matches!(m.items[*i], Item::Entry { enabled: true, .. }))
    }

    fn handle(&mut self, ev: Event) -> bool {
        if std::env::var_os("DRZREPLAY_DEBUG").is_some() {
            match &ev {
                Event::ReparentNotify(e) => eprintln!("reparent window={:#x} parent={:#x} at {},{}", e.window, e.parent, e.x, e.y),
                Event::ConfigureNotify(e) => eprintln!("configure {:#x} {}x{} at {},{}", e.window, e.width, e.height, e.x, e.y),
                Event::ClientMessage(e) => {
                    let name = self.conn.get_atom_name(e.type_).ok().and_then(|c| c.reply().ok()).map(|r| String::from_utf8_lossy(&r.name).into_owned());
                    eprintln!("client message {:?} {:?}", name, e.data.as_data32());
                }
                Event::MapNotify(e) => eprintln!("map {:#x}", e.window),
                Event::UnmapNotify(e) => eprintln!("unmap {:#x}", e.window),
                other => eprintln!("event {:?}", std::mem::discriminant(other)),
            }
        }
        match ev {
            Event::Expose(e) if e.window == self.win => self.draw_icon(),
            Event::Expose(e) if self.menu.as_ref().is_some_and(|m| m.win == e.window) => self.draw_menu(),
            Event::ConfigureNotify(e) if e.window == self.win => {
                self.size = (e.width, e.height);
                self.draw_icon();
            }
            Event::ButtonPress(e) if e.event == self.win => match e.detail {
                1 | 2 => {
                    let _ = self.acts.send(Act::Save);
                }
                3 => self.open_menu(e.root_x as i32, e.root_y as i32),
                _ => {}
            },
            Event::MotionNotify(e) if self.menu.is_some() => {
                let h = self.item_at(e.event_x as i32, e.event_y as i32);
                if self.menu.as_ref().unwrap().hover != h {
                    self.menu.as_mut().unwrap().hover = h;
                    self.draw_menu();
                }
            }
            Event::ButtonPress(e) if self.menu.is_some() => {
                let (x, y) = (e.event_x as i32, e.event_y as i32);
                if let Some(i) = self.item_at(x, y) {
                    if let Item::Entry { act, .. } = &self.menu.as_ref().unwrap().items[i] {
                        let _ = self.acts.send(*act);
                    }
                    self.close_menu();
                } else if x < 0 || y < 0 || x >= self.menu.as_ref().unwrap().w || y >= self.menu.as_ref().unwrap().h {
                    self.close_menu(); // a click outside
                }
            }
            Event::KeyPress(_) if self.menu.is_some() => self.close_menu(),
            Event::DestroyNotify(e) if e.window == self.win => return false,
            _ => {}
        }
        true
    }

    fn run(&mut self, rx: Receiver<View>) {
        let fd = self.conn.stream().as_raw_fd();
        loop {
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, 200) };
            while let Ok(Some(ev)) = self.conn.poll_for_event() {
                if !self.handle(ev) {
                    return;
                }
            }
            let mut latest = None;
            loop {
                match rx.try_recv() {
                    Ok(v) => latest = Some(v),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => {
                        self.close_menu();
                        let _ = self.conn.destroy_window(self.win);
                        let _ = self.conn.flush();
                        return; // the daemon let go of us
                    }
                }
            }
            if let Some(v) = latest {
                let icon_changed = v.recording != self.view.recording;
                self.view = v;
                if icon_changed {
                    self.draw_icon();
                }
                if self.menu.is_some() {
                    self.draw_menu();
                }
            }
        }
    }
}

#[allow(dead_code)]
fn _unused(_: xproto::Window) {}
