//! Tray icon (StatusNotifierItem): what AppIndicator did before, without GTK 3.
use crate::config as C;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Category, Handle, Status, ToolTip, TrayMethods};
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Act {
    Save,
    ToggleRecord,
    TogglePause,
    ToggleIntro(bool),
    OpenFolder,
    OpenSettings,
    Update,
    Restart,
    Quit,
}

/// What the menu shows: the daemon pushes a new copy whenever something changes.
#[derive(Clone, Default, Debug)]
pub struct View {
    pub save_label: String,
    pub save_enabled: bool,
    pub record_label: String,
    pub record_enabled: bool,
    pub pause_label: String,
    pub intro: bool,
    pub update_label: Option<String>,
    pub recording: bool, // the red dot: the screen is being captured
    pub tooltip: String,
}

pub struct Tray {
    tx: Sender<Act>,
    pub view: View,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "drzreplay".into()
    }
    fn category(&self) -> Category {
        Category::ApplicationStatus
    }
    fn title(&self) -> String {
        C::APP_NAME.into()
    }
    fn status(&self) -> Status {
        Status::Active
    }
    fn icon_theme_path(&self) -> String {
        C::assets_dir().join("icons").to_string_lossy().into_owned()
    }
    fn icon_name(&self) -> String {
        if self.view.recording { "drzreplay-rec" } else { "drzreplay" }.into()
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: self.view.tooltip.clone(), ..Default::default() }
    }
    // a click on the icon saves, like the old middle click (secondary activation)
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Act::Save);
    }
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Act::Save);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let v = &self.view;
        let item = |label: &str, enabled: bool, act: Act| -> MenuItem<Self> {
            StandardItem {
                label: label.to_string(),
                enabled,
                activate: Box::new(move |t: &mut Self| {
                    let _ = t.tx.send(act);
                }),
                ..Default::default()
            }
            .into()
        };
        let mut items: Vec<MenuItem<Self>> = vec![];
        if let Some(l) = &v.update_label {
            items.push(item(l, true, Act::Update));
        }
        items.push(item(&v.save_label, v.save_enabled, Act::Save));
        items.push(item(&v.record_label, v.record_enabled, Act::ToggleRecord));
        items.push(MenuItem::Separator);
        items.push(item(&v.pause_label, true, Act::TogglePause));
        items.push(
            CheckmarkItem {
                label: "Intro".into(),
                checked: v.intro,
                activate: Box::new(|t: &mut Self| {
                    let on = !t.view.intro;
                    t.view.intro = on;
                    let _ = t.tx.send(Act::ToggleIntro(on));
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(item("Open folder", true, Act::OpenFolder));
        items.push(item("Settings", true, Act::OpenSettings));
        items.push(MenuItem::Separator);
        // starts the recorder over (a running full recording is saved first): when something got stuck
        items.push(item("Restart", true, Act::Restart));
        items.push(item("Quit", true, Act::Quit));
        items
    }
}

pub struct TrayHandle {
    rt: tokio::runtime::Handle,
    handle: Handle<Tray>,
}

impl TrayHandle {
    pub fn update(&self, view: View) {
        let h = self.handle.clone();
        self.rt.spawn(async move {
            h.update(move |t: &mut Tray| t.view = view).await;
        });
    }

    pub fn shutdown(&self) {
        let h = self.handle.clone();
        self.rt.spawn(async move {
            h.shutdown().await;
        });
    }
}

/// Show the tray icon; Err says why it can't (no StatusNotifier host on this desktop).
pub fn spawn(rt: &tokio::runtime::Runtime, tx: Sender<Act>, view: View) -> Result<TrayHandle, String> {
    let tray = Tray { tx, view };
    let handle = rt.block_on(async { tray.spawn().await }).map_err(|e| e.to_string())?;
    Ok(TrayHandle { rt: rt.handle().clone(), handle })
}

/// Whichever kind of tray this desktop has.
pub enum AnyTray {
    /// StatusNotifierItem (KDE, GNOME with the extension, Xfce, most panels today)
    Sni(TrayHandle),
    /// the classic XEmbed tray (LXDE, older panels)
    X(crate::xtray::XTray),
}

impl AnyTray {
    pub fn update(&self, view: View) {
        match self {
            AnyTray::Sni(t) => t.update(view),
            AnyTray::X(t) => t.update(view),
        }
    }
    pub fn shutdown(&self) {
        if let AnyTray::Sni(t) = self {
            t.shutdown();
        } // the XEmbed icon goes away when its handle is dropped
    }
}

pub fn spawn_any(rt: &tokio::runtime::Runtime, tx: Sender<Act>, view: View) -> Result<AnyTray, String> {
    match spawn(rt, tx.clone(), view.clone()) {
        Ok(h) => Ok(AnyTray::Sni(h)),
        Err(sni) => crate::xtray::spawn(tx, view).map(AnyTray::X).map_err(|x| format!("{sni}; {x}")),
    }
}
