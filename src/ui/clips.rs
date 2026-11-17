//! The Clips tab: the list of saved clips and what you can do with each of them.
use super::{button, human_date, human_size, human_time, media, Win};
use crate::config as C;
use crate::hotkey::pretty;
use crate::trim;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

const MAX_CLIPS_SHOWN: usize = 60;

/// Clipboard / drag-and-drop content for a file: drop it into Discord, a browser, a file manager.
fn file_provider(path: &Path) -> gdk::ContentProvider {
    let list = gdk::FileList::from_array(&[gio::File::for_path(path)]);
    gdk::ContentProvider::for_value(&list.to_value())
}

#[derive(Clone)]
struct Clip {
    mtime: SystemTime,
    size: u64,
    path: PathBuf,
}

fn mtime_ns(t: SystemTime) -> u128 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

impl Win {
    #[allow(clippy::type_complexity)]
    pub(super) fn page_clips(self: &Rc<Self>) -> (gtk::Stack, (adw::PreferencesGroup, gtk::ListBox, gtk::Label, adw::StatusPage, gtk::Stack)) {
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder().title("Saved clips").build();
        let hs = gtk::Box::builder().spacing(6).build();
        let refresh = button(None, Some("view-refresh-symbolic"), Some("Refresh"), true);
        {
            let w = Rc::downgrade(self);
            refresh.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    w.refresh_clips();
                }
            });
        }
        hs.append(&refresh);
        let folder = button(None, Some("folder-open-symbolic"), Some("Open folder"), true);
        {
            let w = Rc::downgrade(self);
            folder.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    w.open_dir();
                }
            });
        }
        hs.append(&folder);
        group.set_header_suffix(Some(&hs));
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        group.add(&list);
        let more = gtk::Label::builder().css_classes(["dim-label"]).margin_top(10).visible(false).build();
        group.add(&more);
        page.add(&group);

        let empty = adw::StatusPage::builder().icon_name("folder-videos-symbolic").title("No clips yet").build();
        let stack = gtk::Stack::new();
        stack.add_named(&page, Some("list"));
        stack.add_named(&empty, Some("empty"));
        (stack.clone(), (group, list, more, empty, stack))
    }

    fn clip_files(&self) -> Vec<Clip> {
        let d = self.cfg().output_dir();
        let mut out: Vec<Clip> = std::fs::read_dir(&d)
            .map(|r| {
                r.flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        let name = p.file_name()?.to_string_lossy().to_lowercase();
                        if name.starts_with('.') || !(name.ends_with(".mp4") || name.ends_with(".mkv")) {
                            return None;
                        }
                        let m = e.metadata().ok()?;
                        Some(Clip { mtime: m.modified().ok()?, size: m.len(), path: p })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(b.path.cmp(&a.path)));
        out
    }

    pub(super) fn refresh_clips(self: &Rc<Self>) {
        // the button may be hammered: one rescan per half second is plenty
        let now = Instant::now();
        if self.last_refresh.get().is_some_and(|t| now.duration_since(t) < Duration::from_millis(500)) {
            return;
        }
        self.last_refresh.set(Some(now));
        self.load_clips(true);
    }

    pub(super) fn load_clips(self: &Rc<Self>, force: bool) {
        let ui = self.ui();
        // polled every second: a stat of the folder is enough to know nothing was added or removed
        let d = self.cfg().output_dir();
        let dir_sig = (d.clone(), std::fs::metadata(&d).and_then(|m| m.modified()).ok());
        if self.dir_sig.borrow().as_ref() == Some(&dir_sig) && !force {
            return;
        }
        *self.dir_sig.borrow_mut() = Some(dir_sig);
        let clips = self.clip_files();
        let shown: Vec<Clip> = clips.iter().take(MAX_CLIPS_SHOWN).cloned().collect();
        let sig = format!("{}|{}|{}", d.display(), clips.len(), shown.iter().map(|c| format!("{}:{}:{}", c.path.display(), mtime_ns(c.mtime), c.size)).collect::<Vec<_>>().join(";"));
        if self.clips_sig.borrow().as_deref() == Some(sig.as_str()) && !force {
            return;
        }
        *self.clips_sig.borrow_mut() = Some(sig);
        let total: u64 = clips.iter().map(|c| c.size).sum();
        ui.clips_group.set_description(if clips.is_empty() {
            None
        } else {
            Some(format!("{} clip{} · {}", clips.len(), if clips.len() == 1 { "" } else { "s" }, human_size(total as f64)))
        }
        .as_deref());
        ui.clips_empty.set_description(Some(&format!("Press {} to save the last {} seconds", pretty(&self.cfg().s("hotkey")), self.cfg().i("seconds"))));
        ui.clips_stack.set_visible_child_name(if clips.is_empty() { "empty" } else { "list" });
        // rows are built once per clip and reused; only new clips get a new row (and a thumbnail)
        while let Some(child) = ui.clips_list.first_child() {
            ui.clips_list.remove(&child);
        }
        let mut rows = std::collections::HashMap::new();
        for c in &shown {
            let key = (c.path.clone(), mtime_ns(c.mtime), c.size);
            let row = self.clip_rows.borrow().get(&key).cloned().unwrap_or_else(|| self.clip_row(c));
            ui.clips_list.append(&row);
            rows.insert(key, row);
        }
        *self.clip_rows.borrow_mut() = rows;
        let hidden = clips.len().saturating_sub(MAX_CLIPS_SHOWN);
        ui.clips_more.set_visible(hidden > 0);
        if hidden > 0 {
            ui.clips_more.set_label(&format!("{hidden} older clip{} in the folder", if hidden == 1 { "" } else { "s" }));
        }
    }

    fn clip_row(self: &Rc<Self>, c: &Clip) -> adw::ActionRow {
        let path = c.path.clone();
        // clips the user renamed show their name, the others the date they were saved
        let date = if trim::is_auto_name(&path) { None } else { Some(human_date(c.mtime)) };
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let title = if date.is_some() { stem } else { human_date(c.mtime) };
        let join = |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(" · ");
        let size = human_size(c.size as f64);
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&title))
            .activatable(true)
            .subtitle(join(vec![date.clone(), Some(size.clone())]))
            .tooltip_text(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .build();
        let pic = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).css_classes(["thumb"]).valign(gtk::Align::Center).margin_top(6).margin_bottom(6).can_shrink(true).build();
        pic.set_size_request(96, 54);
        pic.set_overflow(gtk::Overflow::Hidden);
        row.add_prefix(&pic);

        let duration = Rc::new(Cell::new(0.0f64));
        {
            let (pic, row, duration, date, size) = (pic.clone(), row.clone(), duration.clone(), date, size);
            self.thumbs.request(&path, move |jpg, dur| {
                if let Some(j) = jpg {
                    pic.set_filename(Some(&j));
                }
                if dur > 0.0 {
                    duration.set(dur);
                    row.set_subtitle(&join(vec![date.clone(), Some(human_time(dur)), Some(size.clone())]));
                }
            });
        }
        {
            let w = Rc::downgrade(self);
            let p = path.clone();
            row.connect_activated(move |_| {
                if let Some(w) = w.upgrade() {
                    w.open_uri(&p);
                }
            });
        }
        let add = |icon: &str, tip: &str, f: Box<dyn Fn(&Rc<Win>) + 'static>| {
            let b = button(None, Some(icon), Some(tip), true);
            let w = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(w) = w.upgrade() {
                    f(&w);
                }
            });
            row.add_suffix(&b);
        };
        {
            let (p, d) = (path.clone(), duration.clone());
            add("edit-cut-symbolic", "Trim", Box::new(move |w| w.trim_clip(&p, d.get())));
        }
        {
            let p = path.clone();
            add("document-edit-symbolic", "Rename", Box::new(move |w| w.rename_clip(&p)));
        }
        {
            let p = path.clone();
            add("edit-copy-symbolic", "Copy (paste into Discord, Telegram…)", Box::new(move |w| w.copy_clip(&p)));
        }
        {
            let p = path.clone();
            add("folder-open-symbolic", "Show in folder", Box::new(move |_| crate::system::show_in_folder(&p)));
        }
        {
            let p = path.clone();
            add("user-trash-symbolic", "Move to trash", Box::new(move |w| w.trash_clip(&p)));
        }
        let drag = gtk::DragSource::builder().actions(gdk::DragAction::COPY).build();
        {
            let p = path.clone();
            drag.connect_prepare(move |_, _, _| Some(file_provider(&p)));
        }
        {
            let pic = pic.clone();
            drag.connect_drag_begin(move |src, _| {
                let paintable = gtk::WidgetPaintable::new(Some(&pic));
                src.set_icon(Some(&paintable), 0, 0);
            });
        }
        row.add_controller(drag);
        row
    }

    /// Pick a start and an end while watching the clip, save that piece as a new file.
    fn trim_clip(self: &Rc<Self>, path: &Path, dur: f64) {
        let dur = if dur > 0.0 { dur } else { media::duration(path) };
        if dur <= 0.0 {
            self.toast("Can't read this clip");
            return;
        }
        let media_file = gtk::MediaFile::for_filename(path);
        let video = gtk::Video::builder().media_stream(&media_file).autoplay(false).loop_(false).vexpand(true).build();
        video.set_size_request(480, 270);

        let spin = |title: &str, value: f64| {
            let adj = gtk::Adjustment::new(value, 0.0, dur, 0.1, 1.0, 0.0);
            let row = adw::SpinRow::builder().title(title).adjustment(&adj).digits(1).build();
            let here = button(None, Some("find-location-symbolic"), Some("Use the current position"), true);
            let (m, r) = (media_file.clone(), row.clone());
            here.connect_clicked(move |_| r.set_value(m.timestamp() as f64 / 1e6));
            row.add_suffix(&here);
            row
        };
        let start = spin("Start, s", 0.0);
        let end = spin("End, s", (dur * 10.0).round() / 10.0);
        let delete = adw::SwitchRow::builder().title("Delete the original").build();
        let group = adw::PreferencesGroup::new();
        group.add(&start);
        group.add(&end);
        group.add(&delete);
        let length = gtk::Label::builder().css_classes(["dim-label"]).halign(gtk::Align::Start).build();
        let save = gtk::Button::builder().label("Save trimmed copy").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();

        let changed = {
            let (start, end, save, length, m) = (start.clone(), end.clone(), save.clone(), length.clone(), media_file.clone());
            move |seek: Option<&adw::SpinRow>| {
                let (a, b) = (start.value(), end.value());
                let ok = b - a >= 0.5;
                save.set_sensitive(ok);
                length.set_label(&if ok { format!("New length {}", human_time(b - a)) } else { "The end must be after the start".to_string() });
                if let Some(row) = seek {
                    if m.is_prepared() {
                        m.seek((row.value() * 1e6) as i64); // show the frame being picked
                    }
                }
            }
        };
        let changed = Rc::new(changed);
        {
            let c = changed.clone();
            start.connect_value_notify(move |r| c(Some(r)));
            let c = changed.clone();
            end.connect_value_notify(move |r| c(Some(r)));
        }
        changed(None);

        let bx = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
        bx.append(&video);
        bx.append(&group);
        bx.append(&length);
        bx.append(&save);
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&adw::HeaderBar::new());
        tv.set_content(Some(&bx));
        let dlg = adw::Dialog::builder().title("Trim").content_width(560).child(&tv).build();

        {
            let (w, dlg, m, start, end, delete, path) = (Rc::downgrade(self), dlg.clone(), media_file.clone(), start.clone(), end.clone(), delete.clone(), path.to_path_buf());
            save.connect_clicked(move |_| {
                let Some(w) = w.upgrade() else { return };
                let (a, b, rm) = (start.value(), end.value(), delete.is_active());
                m.pause();
                dlg.close();
                w.toast("Trimming…");
                let (tx, rx) = std::sync::mpsc::channel();
                let p = path.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(trim::trim(&p, a, b, rm));
                });
                let w = Rc::downgrade(&w);
                glib::timeout_add_local(Duration::from_millis(200), move || match rx.try_recv() {
                    Ok(res) => {
                        if let Some(w) = w.upgrade() {
                            match res {
                                Ok(out) => {
                                    w.toast(&format!("Saved {}", out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                                    w.load_clips(true);
                                }
                                Err(e) => w.toast(&format!("Couldn't trim: {e}")),
                            }
                        }
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(_) => glib::ControlFlow::Break,
                });
            });
        }
        {
            let m = media_file.clone();
            dlg.connect_closed(move |_| m.pause());
        }
        dlg.present(Some(&self.win));
    }

    fn rename_clip(self: &Rc<Self>, path: &Path) {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let entry = gtk::Entry::builder().text(stem).activates_default(true).build();
        let dlg = adw::AlertDialog::builder().heading("Rename clip").extra_child(&entry).close_response("cancel").default_response("rename").build();
        dlg.add_response("cancel", "Cancel");
        dlg.add_response("rename", "Rename");
        dlg.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        let (w, path, e) = (Rc::downgrade(self), path.to_path_buf(), entry.clone());
        dlg.connect_response(None, move |_, r| {
            if r != "rename" {
                return;
            }
            let Some(w) = w.upgrade() else { return };
            match trim::rename(&path, e.text().as_str()) {
                Err(msg) => w.toast(&format!("Couldn't rename: {msg}")),
                Ok(new) if new != path => {
                    w.toast("Renamed");
                    w.load_clips(true);
                }
                Ok(_) => {}
            }
        });
        dlg.present(Some(&self.win));
        entry.grab_focus();
        entry.select_region(0, -1);
    }

    fn copy_clip(&self, path: &Path) {
        let _ = self.win.clipboard().set_content(Some(&file_provider(path)));
        self.toast("Copied — paste it into a chat");
    }

    fn trash_clip(self: &Rc<Self>, path: &Path) {
        match gio::File::for_path(path).trash(gio::Cancellable::NONE) {
            Ok(()) => self.toast("Moved to trash"),
            Err(_) => {
                // no trash on this drive (USB stick, tmpfs…): offer to delete for good
                let (w, p) = (Rc::downgrade(self), path.to_path_buf());
                self.toast_button("This drive has no trash", "Delete", move || {
                    let _ = std::fs::remove_file(&p);
                    if let Some(w) = w.upgrade() {
                        w.load_clips(true);
                    }
                });
            }
        }
        self.load_clips(true);
    }

    pub(super) fn open_uri(&self, path: &Path) {
        let _ = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(path).uri(), gio::AppLaunchContext::NONE);
    }
}

#[allow(dead_code)]
fn _unused() -> PathBuf {
    C::home()
}
