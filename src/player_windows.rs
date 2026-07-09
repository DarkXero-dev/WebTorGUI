use anyhow::{anyhow, Result};
use std::io::{BufRead, BufReader, Write};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SetParent, SetWindowPos,
    ShowWindow, CW_USEDEFAULT, HWND_TOP, SET_WINDOW_POS_FLAGS, SHOW_WINDOW_CMD, SWP_NOZORDER,
    SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WNDCLASSEXW, WS_CHILD, WS_VISIBLE,
};

/// A video window embedded via a Win32 child window: mpv is spawned with
/// `--wid=<hwnd>` targeting a real window we create as a child of our own
/// app window, so it renders directly on screen (not through egui's texture
/// pipeline) - the Windows counterpart to `player.rs`'s X11 reparenting.
///
/// mpv's own fullscreen (its OSC button or `f` key) can't work correctly
/// while this stays a child window: Windows clips a child's rendering to
/// its parent's client area regardless of anything mpv does internally. When
/// mpv reports (via its JSON IPC named pipe) that its `fullscreen` property
/// changed, we reparent this window to the desktop and size it to the
/// monitor ourselves, and reparent it back when fullscreen is toggled off -
/// the same trick `player.rs` performs against the X11 root window.
pub struct EmbeddedPlayer {
    hwnd: HWND,
    child: std::process::Child,
    last_geom: (i32, i32, u32, u32),
    parent_hwnd: HWND,
    is_native_fullscreen: bool,
    fullscreen_rx: Option<std::sync::mpsc::Receiver<bool>>,
    hidden: bool,
    pipe_path: String,
}

// HWND wraps a raw pointer, so it isn't Send/Sync by default - safe here
// because we never touch it from more than one thread at a time (the
// IPC-reading thread only ever sends parsed bool events over the channel,
// never touches the HWND itself).
unsafe impl Send for EmbeddedPlayer {}

const WINDOW_CLASS_NAME: PCWSTR = windows::core::w!("WebtorAppEmbeddedVideo");

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn register_window_class() {
    use std::sync::Once;
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            lpszClassName: WINDOW_CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
}

impl EmbeddedPlayer {
    /// `parent_handle` is the owning window's raw HWND value, widened to
    /// `isize` so the caller in `ui/app.rs` can hold one field type across
    /// platforms (an X11 XID is 32-bit; `player.rs` widens it the same way).
    pub fn spawn(parent_handle: isize, x: i32, y: i32, w: u32, h: u32, source: &str) -> Result<Self> {
        register_window_class();
        let parent_hwnd = HWND(parent_handle as *mut _);

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                WINDOW_CLASS_NAME,
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE,
                x,
                y,
                w as i32,
                h as i32,
                Some(parent_hwnd),
                None,
                None,
                None,
            )
        }
        .map_err(|e| anyhow!("CreateWindowExW failed: {e}"))?;

        let pipe_path = format!(r"\\.\pipe\webtorapp-mpv-{}", uuid::Uuid::new_v4());
        let mpv_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("mpv.exe")))
            .filter(|p| p.exists())
            .unwrap_or_else(|| std::path::PathBuf::from("mpv.exe"));

        let child = std::process::Command::new(mpv_path)
            .args([
                "--no-terminal",
                "--really-quiet",
                "--force-window=yes",
                "--keep-open=yes",
                // Crop-to-fill: scales the video to cover the whole embedded
                // window with no letterbox bars, while keeping the source's
                // aspect ratio (unlike --keepaspect=no, which stretches and
                // distorts the picture).
                "--panscan=1.0",
                // mpv's built-in on-screen controller defaults to a scale
                // tuned for a full monitor - inside our small embedded
                // window its buttons/seekbar are nearly illegible, so scale
                // both the windowed and fullscreen (native-fullscreen) OSC up.
                "--script-opts=osc-scalewindowed=1.6,osc-scalefullscreen=1.4",
                &format!("--wid={}", hwnd.0 as isize),
                &format!("--input-ipc-server={pipe_path}"),
                source,
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("failed to launch mpv: {e}"))?;

        // Named pipe client handles on Windows don't support the same
        // non-blocking read semantics the Linux Unix-socket version relies
        // on, so a background thread does blocking reads instead, parsing
        // each JSON line and forwarding fullscreen-toggle events through a
        // channel - the same "background reader + mpsc + non-blocking
        // drain" shape this codebase already uses for `torrent_add_rx`.
        let (tx, fullscreen_rx) = std::sync::mpsc::channel();
        let pipe_path_for_thread = pipe_path.clone();
        std::thread::spawn(move || {
            let mut stream = None;
            for _ in 0..50 {
                if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(&pipe_path_for_thread) {
                    stream = Some(f);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let Some(mut stream) = stream else { return };
            let _ = writeln!(stream, r#"{{"command": ["observe_property", 1, "fullscreen"]}}"#);
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
                if json.get("event").and_then(|e| e.as_str()) == Some("property-change")
                    && json.get("name").and_then(|n| n.as_str()) == Some("fullscreen")
                {
                    if let Some(v) = json.get("data").and_then(|d| d.as_bool()) {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            hwnd,
            child,
            last_geom: (x, y, w, h),
            parent_hwnd,
            is_native_fullscreen: false,
            fullscreen_rx: Some(fullscreen_rx),
            hidden: false,
            pipe_path,
        })
    }

    /// Non-blocking check for mpv-reported fullscreen state changes (its OSC
    /// fullscreen button or the `f` key). Returns `Some(true/false)` the
    /// frame that state changes, `None` otherwise.
    pub fn poll_fullscreen_toggle(&mut self) -> Option<bool> {
        self.fullscreen_rx.as_ref()?.try_recv().ok()
    }

    pub fn is_native_fullscreen(&self) -> bool {
        self.is_native_fullscreen
    }

    /// Reparent the embedded window to the desktop and size it to cover the
    /// whole monitor, so mpv's fullscreen actually fills the screen instead
    /// of being clipped to our app's small embedded video area.
    pub fn enter_native_fullscreen(&mut self) {
        if self.is_native_fullscreen {
            return;
        }
        unsafe {
            let _ = SetParent(self.hwnd, None);
            let monitor: HMONITOR = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let rect: RECT = if GetMonitorInfoW(monitor, &mut info).as_bool() {
                info.rcMonitor
            } else {
                RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
            };
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SET_WINDOW_POS_FLAGS(0),
            );
        }
        self.is_native_fullscreen = true;
    }

    /// Reparent the embedded window back into our app and restore it to the
    /// given (app-relative) geometry.
    pub fn exit_native_fullscreen(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.is_native_fullscreen {
            return;
        }
        unsafe {
            let _ = SetParent(self.hwnd, Some(self.parent_hwnd));
            let _ = SetWindowPos(self.hwnd, None, x, y, w as i32, h as i32, SWP_NOZORDER);
        }
        self.is_native_fullscreen = false;
        self.last_geom = (x, y, w, h);
    }

    /// Move/resize the embedded window if its target geometry changed.
    /// No-op while native-fullscreened, since that geometry is monitor-sized
    /// and managed by enter/exit_native_fullscreen instead.
    pub fn reposition(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if self.is_native_fullscreen {
            return;
        }
        let geom = (x, y, w, h);
        if geom == self.last_geom || w == 0 || h == 0 {
            return;
        }
        self.last_geom = geom;
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, x, y, w as i32, h as i32, SWP_NOZORDER);
        }
    }

    /// Hides (or shows) the embedded window. mpv renders as a real Win32
    /// child window painted directly by the OS compositor, not through
    /// egui's own draw pipeline, so it always paints over anything egui
    /// draws in that screen region - no z-order egui offers can put a popup
    /// above it. This is the only way to let an egui popup appear on top of
    /// the video: hide the video underneath it, then show it again once the
    /// popup closes.
    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden == self.hidden {
            return;
        }
        self.hidden = hidden;
        let cmd: SHOW_WINDOW_CMD = if hidden { SW_HIDE } else { SW_SHOW };
        unsafe {
            let _ = ShowWindow(self.hwnd, cmd);
        }
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for EmbeddedPlayer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        // Unlike a Unix domain socket, a Windows named pipe leaves no
        // filesystem entry behind once both ends close - nothing to clean
        // up on disk the way `player.rs` removes its socket file.
        let _ = &self.pipe_path;
    }
}

/// Extract our own app window's HWND, if the current backend exposes one
/// (raw-window-handle's Win32 variant), widened to `isize` to match the
/// Linux implementation's (32-bit) X11 XID.
pub fn own_window_handle(handle: raw_window_handle::RawWindowHandle) -> Option<isize> {
    use raw_window_handle::RawWindowHandle;
    match handle {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}
