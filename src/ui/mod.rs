//! DrzReplay settings window (GTK 4 + libadwaita): Record, Clips and Settings tabs.
mod clips;
mod media;

use crate::autostart;
use crate::config::{self as C, Config};
use crate::daemon::self_exe;
use crate::hotkey::pretty;
use crate::system as sys;
use adw::prelude::*;
use gtk::glib::{self, translate::IntoGlib};
use gtk::{gdk, gio};
use libadwaita as adw;
use gtk4 as gtk;
use serde_json::{json, Value};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant, SystemTime};

const FPS: &[(i64, &str)] = &[(24, "24"), (30, "30"), (60, "60")];
const QUALITY: &[(&str, &str)] = &[("low", "Low"), ("balanced", "Medium"), ("high", "High"), ("ultra", "Maximum")];
const RESOLUTION: &[(i64, &str)] = &[(0, "Original"), (1080, "1080p"), (720, "720p")];
const FORMAT: &[(&str, &str)] = &[("mp4", "MP4"), ("mkv", "MKV")];

const CSS: &str = "
window.csd, window.csd > decoration, window.solid-csd { border-radius: 0; box-shadow: none; }
.thumb { border-radius: 6px; background: alpha(currentColor, 0.08); }
.rec-on { color: #e01b24; }
.save-hint { margin-top: 6px; }
button.rec-btn { padding: 0 22px; min-height: 36px; }
button.save { min-height: 40px; padding: 0 34px; font-weight: bold; }
";

// ---------------------------------------------------------------- helpers
pub fn human_size(mut n: f64) -> String {
    for unit in ["B", "KB", "MB", "GB"] {
        if n < 1024.0 || unit == "GB" {
            return if unit == "B" || unit == "KB" { format!("{n:.0} {unit}") } else { format!("{n:.1} {unit}") };
        }
        n /= 1024.0;
    }
    String::new()
}

pub fn human_time(sec: f64) -> String {
    let s = sec.round() as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn tilde(path: &Path) -> String {
    let home = C::home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn human_date(ts: SystemTime) -> String {
    use chrono::{DateTime, Local};
    let d: DateTime<Local> = ts.into();
    let today = Local::now().date_naive();
    if d.date_naive() == today {
        format!("Today, {}", d.format("%H:%M"))
    } else if d.date_naive() == today.pred_opt().unwrap_or(today) {
        format!("Yesterday, {}", d.format("%H:%M"))
    } else {
        format!("{} {}, {}", d.format("%b"), d.format("%-d"), d.format("%H:%M"))
    }
}

fn spawn_detached(args: &[&str]) {
    let mut c = Command::new(args[0]);
    c.args(&args[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    sys::detach(&mut c);
    if let Ok(mut ch) = c.spawn() {
        std::thread::spawn(move || {
            let _ = ch.wait();
        });
    }
}

fn run_self(args: &[&str]) {
    let exe = self_exe().to_string_lossy().into_owned();
    let mut v = vec![exe.as_str()];
    v.extend_from_slice(args);
    spawn_detached(&v);
}

fn button(label: Option<&str>, icon: Option<&str>, tip: Option<&str>, flat: bool) -> gtk::Button {
    let b = gtk::Button::builder().valign(gtk::Align::Center).build();
    if let Some(t) = tip {
        b.set_tooltip_text(Some(t));
    }
    if let Some(i) = icon {
        b.set_icon_name(i);
        if flat {
            b.add_css_class("flat");
        }
    } else if let Some(l) = label {
        b.set_label(l);
    }
    b
}

// ---------------------------------------------------------------- the window
enum Row {
    Switch(adw::SwitchRow),
    Combo(adw::ComboRow, Vec<Value>),
    Spin(adw::SpinRow),
    Scale(gtk::Scale),
}

struct Ui {
    rec_row: adw::ActionRow,
    rec_dot: gtk::Image,
    rec_switch: gtk::Switch,
    buf_row: adw::ActionRow,
    buf_bar: gtk::ProgressBar,
    save_btn: gtk::Button,
    save_hint: gtk::Label,
    rec_btn: gtk::Button,
    rec_btn_content: adw::ButtonContent,
    rec_hint: gtk::Label,
    out_row: adw::ComboRow,
    out_vol: adw::ActionRow,
    mic_row: adw::ComboRow,
    mic_vol: adw::ActionRow,
    clips_group: adw::PreferencesGroup,
    clips_list: gtk::ListBox,
    clips_more: gtk::Label,
    clips_empty: adw::StatusPage,
    clips_stack: gtk::Stack,
    dir_row: adw::ActionRow,
    hk_row: adw::ActionRow,
    hk_label: gtk::Label,
    rk_row: adw::ActionRow,
    rk_label: gtk::Label,
    auto_row: adw::SwitchRow,
    ver_row: adw::ActionRow,
    ver_btn: gtk::Button,
    update_banner: adw::Banner,
}

pub struct Win {
    pub win: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    ui: OnceCell<Ui>,
    cfg: RefCell<Config>,
    first_run: bool,
    conf_mtime: Cell<SystemTime>,
    flush_id: RefCell<Option<glib::SourceId>>,
    dirty: RefCell<HashSet<String>>,
    busy: Cell<bool>,
    rows: RefCell<HashMap<String, Row>>,
    enc_info: sys::Encoders,
    thumbs: media::ThumbLoader,
    clip_rows: RefCell<HashMap<(PathBuf, u128, u64), adw::ActionRow>>,
    clips_sig: RefCell<Option<String>>,
    dir_sig: RefCell<Option<(PathBuf, Option<SystemTime>)>>,
    last_refresh: Cell<Option<Instant>>,
    latest: RefCell<Option<String>>,
}

fn conf_mtime() -> SystemTime {
    std::fs::metadata(C::conf_path()).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

impl Win {
    fn new(app: &adw::Application) -> Rc<Win> {
        let win = adw::ApplicationWindow::builder().application(app).title(C::APP_NAME).default_width(640).default_height(800).build();
        let cfg = Config::load();
        let first_run = !Config::exists();
        let w = Rc::new(Win {
            win,
            toasts: adw::ToastOverlay::new(),
            ui: OnceCell::new(),
            cfg: RefCell::new(cfg),
            first_run,
            conf_mtime: Cell::new(conf_mtime()),
            flush_id: RefCell::new(None),
            dirty: RefCell::new(HashSet::new()),
            busy: Cell::new(false),
            rows: RefCell::new(HashMap::new()),
            enc_info: sys::detect_encoders(false),
            thumbs: media::ThumbLoader::new(),
            clip_rows: RefCell::new(HashMap::new()),
            clips_sig: RefCell::new(None),
            dir_sig: RefCell::new(None),
            last_refresh: Cell::new(None),
            latest: RefCell::new(None),
        });
        w.build();
        w
    }

    fn ui(&self) -> &Ui {
        self.ui.get().expect("window is built")
    }

    fn build(self: &Rc<Self>) {
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(&gdk::Display::default().unwrap(), &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

        // square window corners: with no compositor (most tiling / light desktops) the rounded ones
        // show as black wedges. libadwaita's own style for that, plus an explicit rule in case
        // GTK resets the class
        self.win.add_css_class("solid-csd");

        let stack = adw::ViewStack::new();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build()));
        let menu = gio::Menu::new();
        menu.append(Some("Recorder log"), Some("app.open-log"));
        menu.append(Some("About DrzReplay"), Some("app.about"));
        header.pack_end(&gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Menu").build());

        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        let update_banner = adw::Banner::builder().button_label("Update").build();
        {
            let w = Rc::downgrade(self);
            update_banner.connect_button_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    w.run_update();
                }
            });
        }
        tv.add_top_bar(&update_banner);

        let rec = self.page_record();
        let clips = self.page_clips();
        let settings = self.page_settings();
        stack.add_titled_with_icon(&rec.0, Some("record"), "Record", "media-record-symbolic");
        stack.add_titled_with_icon(&clips.0, Some("clips"), "Clips", "folder-videos-symbolic");
        stack.add_titled_with_icon(&settings.0, Some("settings"), "Settings", "emblem-system-symbolic");
        self.toasts.set_child(Some(&stack));
        tv.set_content(Some(&self.toasts));
        self.win.set_content(Some(&tv));

        let (rec, clips, set) = (rec.1, clips.1, settings.1);
        let ui = Ui {
            rec_row: rec.0,
            rec_dot: rec.1,
            rec_switch: rec.2,
            buf_row: rec.3,
            buf_bar: rec.4,
            save_btn: rec.5,
            save_hint: rec.6,
            rec_btn: rec.7,
            rec_btn_content: rec.8,
            rec_hint: rec.9,
            out_row: rec.10,
            out_vol: rec.11,
            mic_row: rec.12,
            mic_vol: rec.13,
            clips_group: clips.0,
            clips_list: clips.1,
            clips_more: clips.2,
            clips_empty: clips.3,
            clips_stack: clips.4,
            dir_row: set.0,
            hk_row: set.1,
            hk_label: set.2,
            rk_row: set.3,
            rk_label: set.4,
            auto_row: set.5,
            ver_row: set.6,
            ver_btn: set.7,
            update_banner,
        };
        let _ = self.ui.set(ui);

        self.update_sensitivity();
        self.poll();
        {
            let w = Rc::downgrade(self);
            glib::timeout_add_local(Duration::from_millis(1000), move || match w.upgrade() {
                Some(w) => {
                    w.poll();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        }
        self.check_updates(false);
        if self.first_run {
            let w = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(w) = w.upgrade() {
                    w.first_run_setup();
                }
            });
        }
        let w = Rc::downgrade(self);
        self.win.connect_close_request(move |_| {
            // a change made right before closing must not be lost to the save delay
            if let Some(w) = w.upgrade() {
                if w.flush_id.borrow().is_some() {
                    w.flush();
                }
            }
            glib::Propagation::Proceed
        });
    }

    // ------------------------------------------------------------------ settings plumbing
    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn toast_button(&self, text: &str, label: &str, cb: impl Fn() + 'static) {
        let t = adw::Toast::builder().title(text).button_label(label).build();
        t.connect_button_clicked(move |_| cb());
        self.toasts.add_toast(t);
    }

    fn cfg(&self) -> std::cell::Ref<'_, Config> {
        self.cfg.borrow()
    }

    fn set(self: &Rc<Self>, key: &str, value: Value) {
        if self.busy.get() || self.cfg().get(key) == &value {
            return;
        }
        self.cfg.borrow_mut().set(key, value);
        self.dirty.borrow_mut().insert(key.to_string());
        if let Some(id) = self.flush_id.borrow_mut().take() {
            id.remove();
        }
        let w = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(w) = w.upgrade() {
                *w.flush_id.borrow_mut() = None;
                w.flush_now();
            }
        });
        *self.flush_id.borrow_mut() = Some(id);
        self.update_sensitivity();
    }

    /// write now (cancels the pending delayed write)
    fn flush(&self) {
        if let Some(id) = self.flush_id.borrow_mut().take() {
            id.remove();
        }
        self.flush_now();
    }

    fn flush_now(&self) {
        // write only what was changed here, the tray may have changed other keys meanwhile
        let mut cfg = Config::load();
        for k in self.dirty.borrow_mut().drain() {
            let v = self.cfg().get(&k).clone();
            cfg.set(&k, v);
        }
        let _ = cfg.save();
        *self.cfg.borrow_mut() = cfg;
        self.conf_mtime.set(conf_mtime());
        if let Some(pid) = C::daemon_pid() {
            unsafe { libc::kill(pid, libc::SIGHUP) };
        }
    }

    /// The config changed outside this window (tray menu, CLI): show the new values.
    fn reload_external(self: &Rc<Self>) {
        *self.cfg.borrow_mut() = Config::load();
        self.busy.set(true);
        for (key, row) in self.rows.borrow().iter() {
            let v = self.cfg().get(key).clone();
            match row {
                Row::Switch(r) => r.set_active(v.as_bool().unwrap_or(false)),
                Row::Combo(r, values) => {
                    if let Some(i) = values.iter().position(|x| *x == v) {
                        r.set_selected(i as u32);
                    }
                }
                Row::Spin(r) => r.set_value(v.as_f64().unwrap_or(0.0)),
                Row::Scale(s) => s.set_value(v.as_f64().unwrap_or(0.0)),
            }
        }
        self.busy.set(false);
        self.update_sensitivity();
    }

    fn switch(self: &Rc<Self>, title: &str, key: &str, subtitle: Option<&str>) -> adw::SwitchRow {
        let row = adw::SwitchRow::builder().title(title).active(self.cfg().b(key)).build();
        if let Some(s) = subtitle {
            row.set_subtitle(s);
        }
        let (w, k) = (Rc::downgrade(self), key.to_string());
        row.connect_active_notify(move |r| {
            if let Some(w) = w.upgrade() {
                w.set(&k, json!(r.is_active()));
            }
        });
        self.rows.borrow_mut().insert(key.to_string(), Row::Switch(row.clone()));
        row
    }

    fn combo(self: &Rc<Self>, title: &str, key: &str, options: Vec<(Value, String)>, subtitle: Option<&str>) -> adw::ComboRow {
        let (mut values, mut labels): (Vec<Value>, Vec<String>) = options.into_iter().unzip();
        let cur = self.cfg().get(key).clone();
        if !values.contains(&cur) {
            labels.push(cur.as_str().map(String::from).unwrap_or_else(|| cur.to_string()));
            values.push(cur.clone());
        }
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(&refs)).build();
        if let Some(s) = subtitle {
            row.set_subtitle(s);
        }
        row.set_selected(values.iter().position(|v| *v == cur).unwrap_or(0) as u32);
        let (w, k, vals) = (Rc::downgrade(self), key.to_string(), values.clone());
        row.connect_selected_notify(move |r| {
            if let (Some(w), Some(v)) = (w.upgrade(), vals.get(r.selected() as usize)) {
                w.set(&k, v.clone());
            }
        });
        self.rows.borrow_mut().insert(key.to_string(), Row::Combo(row.clone(), values));
        row
    }

    fn spin(self: &Rc<Self>, title: &str, subtitle: &str, key: &str, lo: f64, hi: f64, step: f64) -> adw::SpinRow {
        let adj = gtk::Adjustment::new(self.cfg().f(key), lo, hi, step, step * 4.0, 0.0);
        let row = adw::SpinRow::builder().title(title).subtitle(subtitle).adjustment(&adj).digits(0).build();
        let (w, k) = (Rc::downgrade(self), key.to_string());
        row.connect_value_notify(move |r| {
            if let Some(w) = w.upgrade() {
                w.set(&k, json!(r.value() as i64));
            }
        });
        self.rows.borrow_mut().insert(key.to_string(), Row::Spin(row.clone()));
        row
    }

    fn scale(self: &Rc<Self>, title: &str, key: &str, lo: f64, hi: f64) -> adw::ActionRow {
        let row = adw::ActionRow::builder().title(title).build();
        let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, lo, hi, 5.0);
        s.set_value(self.cfg().f(key));
        s.set_size_request(200, -1);
        s.set_draw_value(true);
        s.set_value_pos(gtk::PositionType::Left);
        s.set_format_value_func(|_, v| format!("{v:.0}%"));
        s.add_mark(100.0, gtk::PositionType::Bottom, None);
        let (w, k) = (Rc::downgrade(self), key.to_string());
        s.connect_value_changed(move |s| {
            if let Some(w) = w.upgrade() {
                w.set(&k, json!(s.value() as i64));
            }
        });
        row.add_suffix(&s);
        self.rows.borrow_mut().insert(key.to_string(), Row::Scale(s));
        row
    }

    // ------------------------------------------------------------------ Record tab
    #[allow(clippy::type_complexity)]
    fn page_record(
        self: &Rc<Self>,
    ) -> (
        adw::PreferencesPage,
        (adw::ActionRow, gtk::Image, gtk::Switch, adw::ActionRow, gtk::ProgressBar, gtk::Button, gtk::Label, gtk::Button, adw::ButtonContent, gtk::Label, adw::ComboRow, adw::ActionRow, adw::ComboRow, adw::ActionRow),
    ) {
        let page = adw::PreferencesPage::new();

        let g = adw::PreferencesGroup::new();
        let rec_row = adw::ActionRow::builder().title("Replays").build();
        let rec_dot = gtk::Image::from_icon_name("media-record-symbolic");
        rec_row.add_prefix(&rec_dot);
        let rec_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        {
            let w = Rc::downgrade(self);
            rec_switch.connect_active_notify(move |sw| {
                if let Some(w) = w.upgrade() {
                    w.on_rec_switch(sw);
                }
            });
        }
        rec_row.add_suffix(&rec_switch);
        rec_row.set_activatable_widget(Some(&rec_switch));
        g.add(&rec_row);

        let buf_row = adw::ActionRow::builder().title("Buffer").build();
        buf_row.add_prefix(&gtk::Image::from_icon_name("document-open-recent-symbolic"));
        let buf_bar = gtk::ProgressBar::builder().valign(gtk::Align::Center).build();
        buf_bar.set_size_request(160, -1);
        buf_row.add_suffix(&buf_bar);
        g.add(&buf_row);
        page.add(&g);

        let g = adw::PreferencesGroup::new();
        let bx = gtk::Box::builder().orientation(gtk::Orientation::Vertical).halign(gtk::Align::Center).margin_top(6).build();
        let save_btn = gtk::Button::builder().label("Save replay").css_classes(["suggested-action", "pill", "save"]).build();
        {
            let w = Rc::downgrade(self);
            save_btn.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    run_self(&["save"]);
                    w.toast("Saving…");
                }
            });
        }
        bx.append(&save_btn);
        let save_hint = gtk::Label::builder().css_classes(["dim-label", "save-hint"]).build();
        bx.append(&save_hint);
        // a full video from now until stopped (same encoder, no extra load)
        let rec_btn = gtk::Button::builder().css_classes(["pill", "rec-btn"]).halign(gtk::Align::Center).margin_top(18).build();
        let rec_btn_content = adw::ButtonContent::builder().icon_name("media-record-symbolic").label("Start recording").build();
        rec_btn.set_child(Some(&rec_btn_content));
        // works with replays off too: then the screen is captured only for this video
        {
            let w = Rc::downgrade(self);
            rec_btn.connect_clicked(move |_| {
                if w.upgrade().is_some() {
                    run_self(&["record"]);
                }
            });
        }
        bx.append(&rec_btn);
        let rec_hint = gtk::Label::builder().css_classes(["dim-label", "save-hint"]).build();
        bx.append(&rec_hint);
        g.add(&bx);
        page.add(&g);

        let g = adw::PreferencesGroup::builder().title("Video").build();
        g.add(&self.spin("Length", "Seconds kept in memory", "seconds", 10.0, 600.0, 5.0));
        if sys::is_wayland() {
            // the desktop's screen-sharing dialog picks the screen; it is remembered until asked again
            let row = adw::ActionRow::builder().title("Screen").subtitle("Picked in the screen-sharing dialog").build();
            let b = button(Some("Choose again"), None, None, true);
            let w = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    crate::wayland::forget_screen();
                    w.start_daemon("restart");
                    w.toast("Choose the screen in the dialog");
                }
            });
            row.add_suffix(&b);
            g.add(&row);
        } else {
            let mut screens: Vec<(Value, String)> = vec![(json!("all"), "All screens".into())];
            screens.extend(sys::monitors().into_iter().map(|m| (json!(m.name), format!("{} ({}×{})", m.name, m.w, m.h))));
            if screens.len() > 2 {
                g.add(&self.combo("Screen", "monitor", screens, None));
            }
        }
        g.add(&self.combo("Frame rate", "fps", FPS.iter().map(|(v, l)| (json!(v), l.to_string())).collect(), None));
        g.add(&self.combo("Quality", "rec_quality", QUALITY.iter().map(|(v, l)| (json!(v), l.to_string())).collect(), None));
        let mut encs: Vec<(Value, String)> = vec![(json!("auto"), "Auto".into())];
        encs.extend(self.enc_info.available.iter().map(|e| (json!(e), sys::encoder_label(e))));
        g.add(&self.combo("Encoder", "encoder", encs, None));
        g.add(&self.switch("Show cursor", "draw_mouse", None));
        page.add(&g);

        let g = adw::PreferencesGroup::builder().title("Sound").build();
        g.add(&self.switch("System sound", "system_audio", Some("What you hear")));
        let mut outs: Vec<(Value, String)> = vec![(json!("@DEFAULT_MONITOR@"), "Default".into())];
        outs.extend(sys::audio_outputs().into_iter().map(|(n, l)| (json!(n), l)));
        let out_row = self.combo("Output", "system_device", outs, None);
        g.add(&out_row);
        let out_vol = self.scale("Volume", "system_volume", 0.0, 200.0);
        g.add(&out_vol);
        g.add(&self.switch("Microphone", "mic", None));
        let mut ins: Vec<(Value, String)> = vec![(json!("@DEFAULT_SOURCE@"), "Default".into())];
        ins.extend(sys::audio_inputs().into_iter().map(|(n, l)| (json!(n), l)));
        let mic_row = self.combo("Input", "mic_device", ins, None);
        g.add(&mic_row);
        let mic_vol = self.scale("Volume", "mic_volume", 0.0, 200.0);
        g.add(&mic_vol);
        page.add(&g);
        (page, (rec_row, rec_dot, rec_switch, buf_row, buf_bar, save_btn, save_hint, rec_btn, rec_btn_content, rec_hint, out_row, out_vol, mic_row, mic_vol))
    }

    // ------------------------------------------------------------------ Settings tab
    #[allow(clippy::type_complexity)]
    fn page_settings(
        self: &Rc<Self>,
    ) -> (adw::PreferencesPage, (adw::ActionRow, adw::ActionRow, gtk::Label, adw::ActionRow, gtk::Label, adw::SwitchRow, adw::ActionRow, gtk::Button)) {
        let page = adw::PreferencesPage::new();

        let g = adw::PreferencesGroup::builder().title("Saving").build();
        let dir_row = adw::ActionRow::builder().title("Folder").subtitle(tilde(&self.cfg().output_dir())).subtitle_lines(1).build();
        let open = button(None, Some("folder-open-symbolic"), Some("Open"), true);
        {
            let w = Rc::downgrade(self);
            open.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    w.open_dir();
                }
            });
        }
        dir_row.add_suffix(&open);
        let change = button(Some("Change"), None, None, true);
        {
            let w = Rc::downgrade(self);
            change.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    w.pick_folder();
                }
            });
        }
        dir_row.add_suffix(&change);
        g.add(&dir_row);
        let prefix = adw::EntryRow::builder().title("File name").text(self.cfg().s("file_prefix")).show_apply_button(true).build();
        {
            let w = Rc::downgrade(self);
            prefix.connect_apply(move |r| {
                if let Some(w) = w.upgrade() {
                    let t = r.text().trim().replace('/', "-");
                    w.set("file_prefix", json!(if t.is_empty() { C::APP_NAME.to_string() } else { t }));
                }
            });
        }
        g.add(&prefix);
        g.add(&self.combo("Format", "container", FORMAT.iter().map(|(v, l)| (json!(v), l.to_string())).collect(), None));
        g.add(&self.combo("Resolution", "export_height", RESOLUTION.iter().map(|(v, l)| (json!(v), l.to_string())).collect(), None));
        g.add(&self.combo("Quality", "export_quality", QUALITY.iter().map(|(v, l)| (json!(v), l.to_string())).collect(), Some("When the clip is resized or has the intro")));
        let intro = self.switch("Intro", "intro", Some("Replay number and date in the corner for the first seconds"));
        let preview = button(None, Some("media-playback-start-symbolic"), Some("Preview"), true);
        {
            let w = Rc::downgrade(self);
            preview.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    w.flush();
                    run_self(&["preview-intro"]);
                    w.toast("Rendering the preview…");
                }
            });
        }
        intro.add_suffix(&preview);
        g.add(&intro);
        page.add(&g);

        let g = adw::PreferencesGroup::builder().title("Cleanup").description("Oldest clips are deleted when a limit is reached").build();
        g.add(&self.spin("Keep clips", "0 = keep all", "keep_max_clips", 0.0, 9999.0, 1.0));
        g.add(&self.spin("Max space, GB", "0 = no limit", "keep_max_gb", 0.0, 2000.0, 1.0));
        page.add(&g);

        let g = adw::PreferencesGroup::builder().title("Controls").build();
        let (hk_row, hk_label) = self.hotkey_row("Replay hotkey", "hotkey", None);
        g.add(&hk_row);
        let (rk_row, rk_label) = self.hotkey_row("Recording hotkey", "record_hotkey", Some("Starts and stops recording a full video"));
        g.add(&rk_row);
        g.add(&self.switch("Clear the buffer after a replay", "replay_since_last", Some("The next replay only has what happened after the last one")));
        g.add(&self.switch("Click sound", "click_sound", Some("When a replay is saved or a recording starts and stops")));
        g.add(&self.switch("Notifications", "notifications", None));
        page.add(&g);

        let g = adw::PreferencesGroup::builder().title("Startup").build();
        let auto_row = adw::SwitchRow::builder().title("Start with computer").active(autostart::enabled()).build();
        auto_row.connect_active_notify(|r| autostart::set_enabled(r.is_active()));
        g.add(&auto_row);
        g.add(&self.switch("Tray icon", "tray", None));
        page.add(&g);

        let g = adw::PreferencesGroup::builder().title("Updates").build();
        let ver_row = adw::ActionRow::builder().title(format!("{} {}", C::APP_NAME, C::VERSION)).build();
        let ver_btn = button(Some("Check"), None, None, true);
        {
            let w = Rc::downgrade(self);
            ver_btn.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    w.on_ver_btn();
                }
            });
        }
        ver_row.add_suffix(&ver_btn);
        g.add(&ver_row);
        page.add(&g);

        let g = adw::PreferencesGroup::new();
        g.add(&gtk::Label::builder().label("privet mmpd").css_classes(["dim-label", "caption"]).margin_top(12).build());
        page.add(&g);
        (page, (dir_row, hk_row, hk_label, rk_row, rk_label, auto_row, ver_row, ver_btn))
    }

    fn update_sensitivity(&self) {
        let Some(ui) = self.ui.get() else { return };
        let cfg = self.cfg();
        for w in [ui.out_row.upcast_ref::<gtk::Widget>(), ui.out_vol.upcast_ref()] {
            w.set_sensitive(cfg.b("system_audio"));
        }
        for w in [ui.mic_row.upcast_ref::<gtk::Widget>(), ui.mic_vol.upcast_ref()] {
            w.set_sensitive(cfg.b("mic"));
        }
        ui.save_hint.set_label(&format!("or press {}", pretty(&cfg.s("hotkey"))));
        let rk = cfg.s("record_hotkey");
        ui.rec_hint.set_label(&if rk.is_empty() { String::new() } else { format!("or press {}", pretty(&rk)) });
        ui.rec_hint.set_visible(!rk.is_empty());
    }

    // ------------------------------------------------------------------ status
    fn poll(self: &Rc<Self>) {
        if self.flush_id.borrow().is_none() && conf_mtime() != self.conf_mtime.get() {
            self.conf_mtime.set(conf_mtime());
            self.reload_external();
        }
        let ui = self.ui();
        let pid = C::daemon_pid();
        let st = if pid.is_some() { C::read_obj(&C::statusfile()) } else { Default::default() };
        let state = if pid.is_some() { st.get("state").and_then(|s| s.as_str()).map(String::from) } else { None };
        let replays_off = st.get("replays_off").and_then(|v| v.as_bool()).unwrap_or(false);
        let sv = |k: &str| st.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let fv = |k: &str| st.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        self.busy.set(true);
        ui.rec_switch.set_active(matches!(state.as_deref(), Some("recording") | Some("starting")) && !replays_off);
        self.busy.set(false);
        let recording = state.as_deref() == Some("recording") && !replays_off;
        if recording {
            ui.rec_dot.add_css_class("rec-on");
        } else {
            ui.rec_dot.remove_css_class("rec-on");
        }

        let subtitle = if pid.is_none() {
            "Off".to_string()
        } else if recording {
            let g: Vec<i64> = st.get("geometry").and_then(|g| g.as_array()).map(|a| a.iter().filter_map(|v| v.as_i64()).collect()).unwrap_or_default();
            let (gw, gh) = if g.len() >= 2 { (g[0], g[1]) } else { (0, 0) };
            let enc = sys::encoder_label(&sv("encoder"));
            let enc = if sv("encoder").is_empty() { String::new() } else { enc };
            let extra = if st.get("audio_off").and_then(|v| v.as_bool()).unwrap_or(false) { " · no sound" } else { "" };
            format!("{gw}×{gh} · {} fps · {enc}{extra}", fv("fps") as i64)
        } else if state.as_deref() == Some("paused") || replays_off {
            "Off · turn on to save replays".into()
        } else if state.as_deref() == Some("error") {
            if sv("error").is_empty() { "Error".into() } else { sv("error") }
        } else if sv("error").is_empty() {
            "Starting…".into()
        } else {
            sv("error")
        };
        ui.rec_row.set_subtitle(&subtitle);

        let secs = if fv("seconds") > 0.0 { fv("seconds") } else { self.cfg().f("seconds") };
        let buffered = if recording { fv("buffered") } else { 0.0 };
        ui.buf_bar.set_fraction(if secs > 0.0 { (buffered / secs).min(1.0) } else { 0.0 });
        ui.buf_row.set_subtitle(&if recording { format!("{buffered:.0} of {secs:.0} s · {:.0} MB in memory", fv("buffer_mb")) } else { "Empty".into() });
        ui.save_btn.set_sensitive(recording && buffered >= 1.0);

        let cfg = self.cfg().clone();
        for (prefix, key, row, sub) in [("hotkey", "hotkey", &ui.hk_row, ""), ("record_hotkey", "record_hotkey", &ui.rk_row, "Starts and stops recording a full video")] {
            if pid.is_some() && st.get(prefix).and_then(|v| v.as_str()) == Some(cfg.s(key).as_str()) {
                if st.get(&format!("{prefix}_ok")).and_then(|v| v.as_bool()).unwrap_or(false) {
                    row.set_subtitle(sub);
                } else {
                    let e = sv(&format!("{prefix}_error"));
                    if !matches!(e.as_str(), "" | "starting" | "no hotkey") {
                        row.set_subtitle(&format!("Not working: {e}"));
                    }
                }
            }
        }

        let since = if pid.is_some() { st.get("file_recording").and_then(|v| v.as_f64()) } else { None };
        if let Some(since) = since {
            let s = (C::now() - since) as i64;
            ui.rec_btn_content.set_label(&format!("Stop recording · {}:{:02}", s / 60, s % 60));
            ui.rec_btn_content.set_icon_name("media-playback-stop-symbolic");
            ui.rec_btn.add_css_class("destructive-action");
        } else {
            ui.rec_btn_content.set_label("Start recording");
            ui.rec_btn_content.set_icon_name("media-record-symbolic");
            ui.rec_btn.remove_css_class("destructive-action");
        }
        ui.rec_btn.set_sensitive(since.is_some() || matches!(state.as_deref(), Some("recording") | Some("paused")) || pid.is_none());
        self.load_clips(false);
    }

    fn on_rec_switch(self: &Rc<Self>, sw: &gtk::Switch) {
        if self.busy.get() {
            return;
        }
        let pid = C::daemon_pid();
        let st = if pid.is_some() { C::read_obj(&C::statusfile()) } else { Default::default() };
        if sw.is_active() {
            if pid.is_none() {
                self.start_daemon("start");
            } else if st.get("state").and_then(|s| s.as_str()) == Some("error") {
                self.start_daemon("restart");
            } else if st.get("replays_off").and_then(|v| v.as_bool()).unwrap_or(false) {
                crate::request(pid.unwrap(), "resume");
            }
        } else if let Some(pid) = pid {
            // paused, not quit: the hotkeys stay, so Start recording works with replays off too
            crate::request(pid, "pause");
        }
    }

    fn start_daemon(&self, cmd: &str) {
        self.flush();
        run_self(&[cmd]);
    }

    fn open_dir(&self) {
        let d = self.cfg().output_dir();
        let _ = std::fs::create_dir_all(&d);
        let _ = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(&d).uri(), gio::AppLaunchContext::NONE);
    }

    fn pick_folder(self: &Rc<Self>) {
        let dlg = gtk::FileDialog::builder().title("Folder").build();
        dlg.set_initial_folder(Some(&gio::File::for_path(self.cfg().output_dir())));
        let w = Rc::downgrade(self);
        dlg.select_folder(Some(&self.win), gio::Cancellable::NONE, move |res| {
            let (Ok(f), Some(w)) = (res, w.upgrade()) else { return };
            if let Some(p) = f.path() {
                w.set("output_dir", json!(p.to_string_lossy()));
                w.ui().dir_row.set_subtitle(&tilde(&p));
                w.load_clips(true);
            }
        });
    }

    fn hotkey_row(self: &Rc<Self>, title: &str, key: &str, subtitle: Option<&str>) -> (adw::ActionRow, gtk::Label) {
        let row = adw::ActionRow::builder().title(title).build();
        if let Some(s) = subtitle {
            row.set_subtitle(s);
        }
        let cur = self.cfg().s(key);
        let label = gtk::Label::builder().label(if cur.is_empty() { "None".to_string() } else { pretty(&cur) }).valign(gtk::Align::Center).build();
        label.add_css_class("dim-label");
        row.add_suffix(&label);
        let b = button(Some("Change"), None, None, true);
        let (w, k, l, r, sub) = (Rc::downgrade(self), key.to_string(), label.clone(), row.clone(), subtitle.map(String::from));
        b.connect_clicked(move |_| {
            if let Some(w) = w.upgrade() {
                w.change_hotkey(&k, &l, &r, sub.as_deref());
            }
        });
        row.add_suffix(&b);
        (row, label)
    }

    fn change_hotkey(self: &Rc<Self>, key: &str, label: &gtk::Label, row: &adw::ActionRow, subtitle: Option<&str>) {
        let dlg = adw::AlertDialog::new(Some("Press a new hotkey"), Some("Esc to cancel"));
        dlg.add_response("cancel", "Cancel");
        let ctl = gtk::EventControllerKey::new();
        ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (w, key, label, row, subtitle, dlg2) = (Rc::downgrade(self), key.to_string(), label.clone(), row.clone(), subtitle.map(String::from), dlg.clone());
        ctl.connect_key_pressed(move |_, keyval, code, state| {
            let Some(w) = w.upgrade() else { return glib::Propagation::Proceed };
            let mods = state & gtk::accelerator_get_default_mod_mask();
            // read the key in the first (Latin) layout, so Alt+Я is saved as Alt+Z
            let mut keyval = keyval;
            if let Some((base, ..)) = WidgetExt::display(&w.win).translate_key(code, gdk::ModifierType::empty(), 0) {
                if base.into_glib() != 0 {
                    keyval = base;
                }
            }
            let name = keyval.name().map(|n| n.to_string()).unwrap_or_default();
            const MODIFIER_KEYS: &[&str] = &[
                "Shift_L", "Shift_R", "Control_L", "Control_R", "Alt_L", "Alt_R", "Super_L", "Super_R", "Meta_L", "Meta_R", "ISO_Level3_Shift", "Caps_Lock",
                "Num_Lock", "Hyper_L", "Hyper_R",
            ];
            const SOLO_KEYS: &[&str] = &["Pause", "Scroll_Lock", "Print", "Insert"];
            if MODIFIER_KEYS.contains(&name.as_str()) {
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::Escape && mods.is_empty() {
                dlg2.close();
                return glib::Propagation::Stop;
            }
            let is_f = name.starts_with('F') && name.len() > 1 && name[1..].chars().all(|c| c.is_ascii_digit());
            if mods.is_empty() && !SOLO_KEYS.contains(&name.as_str()) && !is_f {
                dlg2.set_body("Use it with Ctrl, Alt, Shift or Super");
                return glib::Propagation::Stop;
            }
            let accel = gtk::accelerator_name(keyval.to_lower(), mods).to_string();
            let other = if key == "hotkey" { "record_hotkey" } else { "hotkey" };
            if accel == w.cfg().s(other) {
                dlg2.set_body("That key is already used for the other action");
                return glib::Propagation::Stop;
            }
            w.set(&key, json!(accel));
            label.set_label(&pretty(&accel));
            row.set_subtitle(subtitle.as_deref().unwrap_or(""));
            dlg2.close();
            glib::Propagation::Stop
        });
        dlg.add_controller(ctl);
        dlg.present(Some(&self.win));
    }

    // ------------------------------------------------------------------ updates
    fn check_updates(self: &Rc<Self>, manual: bool) {
        let ui = self.ui();
        if !manual {
            let cached = C::read_obj(&C::update_cache());
            if C::now() - cached.get("time").and_then(|t| t.as_f64()).unwrap_or(0.0) < 6.0 * 3600.0 {
                self.show_update(cached.get("latest").and_then(|l| l.as_str()).map(String::from), false);
                return;
            }
        }
        ui.ver_btn.set_sensitive(false);
        ui.ver_row.set_subtitle("Checking…");
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        std::thread::spawn(move || {
            let latest = sys::latest_version();
            if let Some(l) = &latest {
                C::update_json(&C::update_cache(), json!({"time": C::now(), "latest": l}));
            }
            let _ = tx.send(latest);
        });
        let w = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
            Ok(latest) => {
                if let Some(w) = w.upgrade() {
                    w.show_update(latest, true);
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => glib::ControlFlow::Break,
        });
    }

    fn show_update(&self, latest: Option<String>, fresh: bool) {
        let ui = self.ui();
        ui.ver_btn.set_sensitive(true);
        *self.latest.borrow_mut() = latest.clone();
        match latest {
            Some(l) if sys::is_newer(&l) => {
                ui.ver_row.set_subtitle(&format!("Version {l} is available"));
                ui.ver_btn.set_label("Update");
                ui.update_banner.set_title(&format!("{} {l} is available", C::APP_NAME));
                ui.update_banner.set_button_label(Some("Update"));
                ui.update_banner.set_revealed(true);
            }
            Some(_) => ui.ver_row.set_subtitle("Up to date"),
            None => ui.ver_row.set_subtitle(if fresh { "Couldn't check (offline?)" } else { "" }),
        }
    }

    fn on_ver_btn(self: &Rc<Self>) {
        let newer = self.latest.borrow().as_deref().is_some_and(sys::is_newer);
        if newer {
            self.run_update();
        } else {
            self.check_updates(true);
        }
    }

    fn run_update(self: &Rc<Self>) {
        let ui = self.ui();
        ui.update_banner.set_button_label(None);
        ui.update_banner.set_title("Updating…");
        ui.ver_btn.set_sensitive(false);
        let (tx, rx) = std::sync::mpsc::channel::<(Option<bool>, String)>();
        std::thread::spawn(move || {
            let _ = tx.send(sys::self_update());
        });
        let w = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(250), move || match rx.try_recv() {
            Ok((ok, msg)) => {
                if let Some(w) = w.upgrade() {
                    w.update_finished(ok, &msg);
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => glib::ControlFlow::Break,
        });
    }

    fn update_finished(&self, ok: Option<bool>, msg: &str) {
        let ui = self.ui();
        ui.ver_btn.set_sensitive(true);
        match ok {
            Some(true) => {
                ui.update_banner.set_title("Updated. Reopen this window to see what's new");
                ui.ver_row.set_subtitle(msg);
            }
            None => {
                // installed before the helper existed: the old way, once
                ui.update_banner.set_revealed(false);
                if sys::run_in_terminal(&C::install_cmd()) {
                    self.toast("Follow the terminal, then reopen this window");
                } else {
                    self.toast(&format!("No terminal found. Run: {}", C::install_cmd()));
                }
            }
            Some(false) => {
                ui.update_banner.set_title(&format!("Update failed: {msg}"));
                ui.update_banner.set_button_label(Some("Try again"));
            }
        }
    }

    fn first_run_setup(self: &Rc<Self>) {
        let _ = self.cfg().save();
        self.conf_mtime.set(conf_mtime());
        autostart::set_enabled(true);
        self.ui().auto_row.set_active(true);
        if C::daemon_pid().is_none() {
            self.start_daemon("start");
        }
        self.toast(&format!("Replays are on. Press {} to save one.", pretty(&self.cfg().s("hotkey"))));
    }
}

// ---------------------------------------------------------------- the application
fn on_about(app: &adw::Application) {
    let about = adw::AboutDialog::builder()
        .application_name(C::APP_NAME)
        .application_icon("drzreplay")
        .version(C::VERSION)
        .developer_name("DRZ")
        .license_type(gtk::License::MitX11)
        .website(format!("https://github.com/{}", C::REPO))
        .issue_url(format!("https://github.com/{}/issues", C::REPO))
        .build();
    about.add_legal_section("Sound", None, gtk::License::Custom, Some("Mechanical Sounds by Brian MacIntosh, CC0"));
    about.add_legal_section("Fonts", None, gtk::License::Custom, Some("Oswald and Inter, SIL Open Font License 1.1"));
    about.present(app.active_window().as_ref());
}

fn float_on_i3() {
    // i3 would tile the settings window across the whole screen; float it like a dialog
    spawn_detached(&["i3-msg", &format!("[title=\"^{}$\"] floating enable, move position center", C::APP_NAME)]);
}

pub fn main() -> i32 {
    glib::set_prgname(Some("drzreplay")); // window class, otherwise it is the file name
    glib::set_application_name(C::APP_NAME);
    let mut flags = gio::ApplicationFlags::empty();
    if std::env::var_os("DRZREPLAY_NON_UNIQUE").is_some() {
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = adw::Application::builder().application_id(C::APP_ID).flags(flags).build();
    let window: Rc<RefCell<Option<Rc<Win>>>> = Rc::new(RefCell::new(None));

    app.connect_startup(|app| {
        if let Some(d) = gdk::Display::default() {
            gtk::IconTheme::for_display(&d).add_search_path(C::assets_dir().join("icons"));
        }
        gtk::Window::set_default_icon_name("drzreplay");
        let about = gio::SimpleAction::new("about", None);
        let a = app.clone();
        about.connect_activate(move |_, _| on_about(&a));
        app.add_action(&about);
        let log = gio::SimpleAction::new("open-log", None);
        log.connect_activate(|_, _| {
            let p = C::ffmpeg_log();
            if p.exists() {
                let _ = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(&p).uri(), gio::AppLaunchContext::NONE);
            }
        });
        app.add_action(&log);
        app.set_accels_for_action("window.close", &["<Control>w", "<Control>q"]);
    });
    {
        let window = window.clone();
        app.connect_activate(move |app| {
            if window.borrow().is_none() {
                let w = Win::new(app);
                if autostart::is_i3() {
                    w.win.connect_map(|_| {
                        glib::timeout_add_local_once(Duration::from_millis(150), float_on_i3);
                    });
                }
                *window.borrow_mut() = Some(w);
            }
            if let Some(w) = window.borrow().as_ref() {
                w.win.present();
            }
        });
    }
    let code = app.run_with_args::<&str>(&[]);
    i32::from(code.value())
}

#[allow(dead_code)]
fn _weak_unused(_: Weak<Win>) {}
