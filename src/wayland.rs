//! Wayland: the screen comes from the ScreenCast portal (PipeWire), the hotkey from the GlobalShortcuts portal.
//!
//! The desktop asks once which screen to share; the answer is remembered (restore token), so later
//! starts are silent. GStreamer records the PipeWire stream and the sound with one clock and hands
//! an MPEG-TS stream to ffmpeg, which writes the same ring buffer as on X11.
use crate::config::{self as C, Config};
use crate::system as sys;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use futures_util::StreamExt;
use serde_json::json;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub enum WlEv {
    /// the user picked a screen
    CastReady,
    CastError(String),
    /// sharing was stopped from the desktop
    CastClosed,
    ShortcutPress(String),
    ShortcutState { ok: bool, err: String },
}

fn token_file() -> PathBuf {
    C::cache_dir().join("screencast.json")
}

fn err_text(e: &ashpd::Error) -> String {
    match e {
        ashpd::Error::Response(ResponseError::Cancelled) => "cancelled".into(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- screen
type Cast = Screencast<'static>;

struct Shared {
    /// (PipeWire node, size) of the shared screen
    stream: Option<(u32, (i32, i32))>,
    proxy: Option<Arc<Cast>>,
    session: Option<Arc<Session<'static, Cast>>>,
}

pub struct ScreenCast {
    shared: Arc<Mutex<Shared>>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl ScreenCast {
    /// Ask the desktop for a screen; `emit` hears when it is ready, failed or stopped.
    pub fn start(rt: &Handle, cursor: bool, emit: impl Fn(WlEv) + Send + Sync + 'static) -> Result<ScreenCast, String> {
        let shared = Arc::new(Mutex::new(Shared { stream: None, proxy: None, session: None }));
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let sh = shared.clone();
        rt.spawn(async move {
            let run = async {
                let proxy = Screencast::new().await.map_err(|_| "no screen-sharing portal (install xdg-desktop-portal for your desktop)".to_string())?;
                let session = proxy.create_session().await.map_err(|e| err_text(&e))?;
                let modes = proxy.available_cursor_modes().await.unwrap_or_default();
                // the cursor drawn into the picture when asked for and offered, otherwise hidden
                let mode = if cursor && modes.contains(CursorMode::Embedded) { CursorMode::Embedded } else { CursorMode::Hidden };
                let token = C::read_obj(&token_file()).get("token").and_then(|t| t.as_str()).map(String::from);
                proxy
                    .select_sources(&session, mode, SourceType::Monitor.into(), false, token.as_deref(), PersistMode::ExplicitlyRevoked)
                    .await
                    .map_err(|e| err_text(&e))?
                    .response()
                    .map_err(|e| err_text(&e))?;
                let started = proxy.start(&session, None).await.map_err(|e| err_text(&e))?.response().map_err(|e| err_text(&e))?;
                if let Some(t) = started.restore_token() {
                    let _ = C::write_json(&token_file(), &json!({"token": t}));
                }
                let s = started.streams().first().ok_or_else(|| "no screen was shared".to_string())?;
                let stream = (s.pipe_wire_node_id(), s.size().unwrap_or((0, 0)));
                let closed = session.receive_closed().await.map_err(|e| err_text(&e))?;
                let session = Arc::new(session);
                {
                    let mut g = sh.lock().unwrap();
                    g.stream = Some(stream);
                    g.proxy = Some(Arc::new(proxy));
                    g.session = Some(session.clone());
                }
                Ok::<_, String>((session, closed))
            };
            match run.await {
                Err(e) => emit(WlEv::CastError(e)),
                Ok((_session, closed)) => {
                    emit(WlEv::CastReady);
                    futures_util::pin_mut!(closed);
                    tokio::select! {
                        _ = closed.next() => emit(WlEv::CastClosed),
                        _ = stop_rx => {}
                    }
                }
            }
        });
        Ok(ScreenCast { shared, stop: Mutex::new(Some(stop_tx)) })
    }

    pub fn stream(&self) -> Option<(u32, (i32, i32))> {
        self.shared.lock().unwrap().stream
    }

    /// A new PipeWire connection (fd) for one recorder process.
    pub fn open_remote(&self, rt: &Handle) -> Result<OwnedFd, String> {
        let (proxy, session) = {
            let g = self.shared.lock().unwrap();
            (g.proxy.clone().ok_or("no session")?, g.session.clone().ok_or("no session")?)
        };
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), proxy.open_pipe_wire_remote(&session)).await })
            .map_err(|_| "the desktop didn't answer".to_string())?
            .map_err(|e| err_text(&e))
    }

    pub fn close(&self, rt: &Handle) {
        if let Some(s) = self.stop.lock().unwrap().take() {
            let _ = s.send(());
        }
        let session = self.shared.lock().unwrap().session.take();
        if let Some(s) = session {
            let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), s.close()).await });
        }
    }
}

/// Ask which screen to share again next time.
pub fn forget_screen() {
    let _ = std::fs::remove_file(token_file());
}

// ---------------------------------------------------------------- hotkeys
/// GTK accelerator "<Alt>z" → shortcut trigger "ALT+z" (XDG shortcuts spec)
pub fn portal_trigger(accel: &str) -> String {
    let mut mods: Vec<String> = vec![];
    let mut key = accel;
    while let Some(rest) = key.strip_prefix('<') {
        let Some(end) = rest.find('>') else { break };
        mods.push(
            match &rest[..end] {
                "Control" | "Primary" => "CTRL".to_string(),
                "Alt" => "ALT".into(),
                "Shift" => "SHIFT".into(),
                "Super" | "Meta" => "LOGO".into(),
                other => other.to_uppercase(),
            },
        );
        key = &rest[end + 1..];
    }
    mods.push(key.to_string());
    mods.join("+")
}

pub struct Shortcuts {
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl Shortcuts {
    /// Global hotkeys through the desktop (GNOME 48+, KDE Plasma 6): `emit` gets presses and the state.
    pub fn start(rt: &Handle, keys: Vec<(String, String)>, emit: impl Fn(WlEv) + Send + Sync + 'static) -> Result<Shortcuts, String> {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        rt.spawn(async move {
            let fail = |e: String| emit(WlEv::ShortcutState { ok: false, err: e });
            let Ok(gs) = GlobalShortcuts::new().await else {
                return fail("this desktop has no global shortcuts; bind “drzreplay save” in its keyboard settings".into());
            };
            let session = match gs.create_session().await {
                Ok(s) => s,
                Err(e) => return fail(err_text(&e)),
            };
            let Ok(mut activated) = gs.receive_activated().await else { return fail("can't listen for shortcuts".into()) };
            let descriptions = |n: &str| if n == "save" { "Save a replay" } else { "Start / stop recording" }.to_string();
            let list: Vec<NewShortcut> =
                keys.iter().map(|(n, a)| NewShortcut::new(n.clone(), descriptions(n)).preferred_trigger(portal_trigger(a).as_str())).collect();
            match gs.bind_shortcuts(&session, &list, None).await {
                Ok(r) => {
                    if let Err(e) = r.response() {
                        return fail(err_text(&e));
                    }
                }
                Err(e) => return fail(err_text(&e)),
            }
            emit(WlEv::ShortcutState { ok: true, err: String::new() });
            tokio::pin!(stop_rx);
            loop {
                tokio::select! {
                    a = activated.next() => match a {
                        Some(a) => emit(WlEv::ShortcutPress(a.shortcut_id().to_string())),
                        None => break,
                    },
                    _ = &mut stop_rx => break,
                }
            }
            let _ = session.close().await;
        });
        Ok(Shortcuts { stop: Mutex::new(Some(stop_tx)) })
    }

    pub fn close(&self, _rt: &Handle) {
        if let Some(s) = self.stop.lock().unwrap().take() {
            let _ = s.send(());
        }
    }
}

// ---------------------------------------------------------------- GStreamer
fn gst_encoder(enc: &str, fps: i64, q: i64) -> String {
    let k = fps; // a keyframe every second, like on X11
    match enc {
        "nvenc" => format!("videoconvert n-threads=2 ! video/x-raw,format=NV12 ! nvh264enc gop-size={k} bframes=0 rc-mode=vbr const-quality={q} ! video/x-h264,profile=high"),
        "vaapi" => format!("videoconvert n-threads=2 ! video/x-raw,format=NV12 ! vah264enc key-int-max={k} b-frames=0 rate-control=cqp qpi={q} qpp={q} ! video/x-h264,profile=high"),
        _ => format!("videoconvert n-threads=2 ! video/x-raw,format=I420 ! x264enc speed-preset=ultrafast tune=zerolatency pass=qual quantizer={q} key-int-max={k} threads=2 ! video/x-h264,profile=high"),
    }
}

fn gst_element(enc: &str) -> &'static str {
    match enc {
        "nvenc" => "nvh264enc",
        "vaapi" => "vah264enc",
        _ => "x264enc",
    }
}

/// gst-launch-1.0 pipeline: screen (+ sound) → H.264 / AAC → MPEG-TS on stdout
pub fn gst_cmd(cfg: &Config, enc: &str, node: u32, fd: i32) -> Vec<String> {
    let fps = cfg.i("fps");
    let q = C::rec_quality(&cfg.s("rec_quality"));
    // the compositor only sends a frame when something changes (and keepalive-time once a second);
    // videorate turns that into a steady frame rate. Late frames are dropped before it, never after.
    let v = format!(
        "pipewiresrc fd={fd} path={node} do-timestamp=true keepalive-time=1000 ! queue max-size-buffers=3 leaky=downstream ! videorate ! video/x-raw,framerate={fps}/1 ! {} ! h264parse config-interval=-1 ! queue max-size-time=3000000000 ! mux.",
        gst_encoder(enc, fps, q)
    );
    let mut audio: Vec<(String, i64)> = vec![];
    if cfg.b("system_audio") {
        let d = cfg.s("system_device");
        audio.push((if d.is_empty() { "@DEFAULT_MONITOR@".into() } else { d }, cfg.i("system_volume")));
    }
    if cfg.b("mic") {
        let d = cfg.s("mic_device");
        audio.push((if d.is_empty() { "@DEFAULT_SOURCE@".into() } else { d }, cfg.i("mic_volume")));
    }
    let mut a = String::new();
    if !audio.is_empty() {
        a.push_str(" audiomixer name=am ! audioconvert ! avenc_aac bitrate=160000 ! aacparse ! queue ! mux.");
        for (dev, vol) in &audio {
            a.push_str(&format!(
                " pulsesrc device={dev} do-timestamp=true provide-clock=false client-name=drzreplay ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! volume volume={:.2} ! queue ! am.",
                *vol as f64 / 100.0
            ));
        }
    }
    let pipeline = format!("mpegtsmux name=mux alignment=7 ! fdsink fd=1 sync=false {v}{a}");
    let mut cmd: Vec<String> = ["gst-launch-1.0", "-q", "-e"].map(String::from).to_vec();
    cmd.extend(pipeline.split_whitespace().map(String::from));
    cmd
}

fn gst_has(element: &str) -> bool {
    sys::run(&["gst-inspect-1.0", "--exists", element], Duration::from_secs(10)).is_some_and(|o| o.ok())
}

/// Encoders GStreamer can use here, best first.
pub fn gst_encoders() -> Vec<String> {
    let mut out = vec![];
    for enc in ["nvenc", "vaapi", "x264"] {
        if !gst_has(gst_element(enc)) {
            continue;
        }
        let test = format!(
            "videotestsrc num-buffers=10 ! video/x-raw,width=640,height=360,framerate=30/1 ! {} ! fakesink",
            gst_encoder(enc, 30, 23)
        );
        let mut cmd = vec!["gst-launch-1.0".to_string(), "-q".into()];
        cmd.extend(test.split_whitespace().map(String::from));
        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        if sys::run(&refs, Duration::from_secs(20)).is_some_and(|o| o.ok()) {
            out.push(enc.to_string());
        }
    }
    out
}

/// Packages needed for Wayland capture that aren't installed, for the error message.
pub fn missing_parts() -> Vec<String> {
    let mut need = vec![];
    if !gst_has("pipewiresrc") {
        need.push("gstreamer1.0-pipewire".to_string());
    }
    if !gst_has("pulsesrc") {
        need.push("gstreamer1.0-plugins-good".into());
    }
    if !gst_has("avenc_aac") {
        need.push("gstreamer1.0-libav".into());
    }
    need
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trigger() {
        assert_eq!(portal_trigger("<Alt>z"), "ALT+z");
        assert_eq!(portal_trigger("<Primary><Shift>r"), "CTRL+SHIFT+r");
    }
}
