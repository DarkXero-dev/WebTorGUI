use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, StackMode, WindowClass};
use x11rb::rust_connection::RustConnection;

/// A video window embedded via X11 window reparenting: mpv is spawned with
/// `--wid=<window>` targeting a real X11 window we create as a child of our
/// own app window, so it renders directly on screen (not through egui's
/// texture pipeline). Requires an X11 or XWayland session.
///
/// mpv's own fullscreen (its OSC button or `f` key) cannot work correctly
/// while this window stays a child of our app: window managers only manage
/// direct children of the root window, so an EWMH fullscreen request on a
/// nested child is simply ignored, and even if mpv resized itself, X11 clips
/// child window rendering to the parent's bounds regardless. When mpv
/// reports (via its JSON IPC socket) that its `fullscreen` property changed,
/// we reparent the window to the root window and size it to the monitor
/// ourselves, and reparent it back when fullscreen is toggled off.
pub struct EmbeddedPlayer {
    conn: RustConnection,
    win_id: u32,
    child: std::process::Child,
    last_geom: (i32, i32, u32, u32),
    parent_xid: u32,
    screen_root: u32,
    screen_w: u16,
    screen_h: u16,
    is_native_fullscreen: bool,
    ipc: Option<UnixStream>,
    ipc_buf: Vec<u8>,
    ipc_path: String,
    hidden: bool,
    /// Populated from mpv's own IPC property-change events (`media-title`,
    /// `duration`, `width`, `height`) - so Now Playing can show the file's
    /// actual metadata instead of just its raw path/URL. `media-title` is
    /// mpv's own best title (embedded tag if the file has one, otherwise
    /// its filename) - not something we derive ourselves.
    media_title: Option<String>,
    duration_secs: Option<f64>,
    width: Option<i64>,
    height: Option<i64>,
}

impl EmbeddedPlayer {
    /// `parent_handle` is the owning window's raw X11 XID, widened to
    /// `isize` (XIDs are 32-bit) to match what the caller in `ui/app.rs` holds.
    pub fn spawn(parent_handle: isize, x: i32, y: i32, w: u32, h: u32, source: &str) -> Result<Self> {
        let parent_xid = parent_handle as u32;
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| anyhow!("no X11 connection: {e}"))?;
        let win_id = conn.generate_id()?;
        let screen = &conn.setup().roots[screen_num];
        let screen_root = screen.root;
        let screen_w = screen.width_in_pixels;
        let screen_h = screen.height_in_pixels;

        // Our own app window (an eframe/glow GL surface) very likely uses a
        // non-default visual/depth (e.g. a 32-bit ARGB visual for
        // compositing), so the child must inherit BOTH depth and visual from
        // the actual parent (0 = COPY_FROM_PARENT for the visual field) -
        // hardcoding the root screen's default visual here causes an X11
        // BadMatch error on CreateWindow when it doesn't match.
        //
        // override_redirect matters once this window is reparented to the
        // root for native fullscreen: it tells the window manager to never
        // manage/decorate/intercept this window, so it behaves as a plain
        // borderless overlay at exactly the geometry we set, instead of the
        // WM trying to treat a suddenly-root-level window as a new toplevel.
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win_id,
            parent_xid,
            x as i16,
            y as i16,
            w as u16,
            h as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().background_pixel(0).border_pixel(0).override_redirect(1u32),
        )?
        .check()
        .map_err(|e| anyhow!("create_window failed: {e:?}"))?;
        conn.map_window(win_id)?.check().map_err(|e| anyhow!("map_window failed: {e:?}"))?;
        conn.flush()?;

        let ipc_path = format!("/tmp/webtorapp-mpv-{}.sock", uuid::Uuid::new_v4());

        let child = std::process::Command::new("mpv")
            .args([
                "--no-terminal",
                "--really-quiet",
                "--force-window=yes",
                "--keep-open=yes",
                // mpv's built-in on-screen controller defaults to a scale
                // tuned for a full monitor - inside our small embedded
                // window its buttons/seekbar are nearly illegible, so scale
                // both the windowed and fullscreen (native-fullscreen) OSC up.
                "--script-opts=osc-scalewindowed=1.6,osc-scalefullscreen=1.4",
                &format!("--wid={win_id}"),
                &format!("--input-ipc-server={ipc_path}"),
                source,
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("failed to launch mpv: {e}"))?;

        // mpv needs a moment to create the IPC socket after spawning; retry
        // briefly rather than failing playback over a missing fullscreen hook.
        let mut ipc = None;
        for _ in 0..50 {
            if let Ok(stream) = UnixStream::connect(&ipc_path) {
                ipc = Some(stream);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if let Some(stream) = &ipc {
            let _ = stream.set_nonblocking(true);
        }
        let mut player = Self {
            conn,
            win_id,
            child,
            last_geom: (x, y, w, h),
            parent_xid,
            screen_root,
            screen_w,
            screen_h,
            is_native_fullscreen: false,
            ipc,
            ipc_buf: Vec::new(),
            ipc_path,
            hidden: false,
            media_title: None,
            duration_secs: None,
            width: None,
            height: None,
        };
        player.send_ipc(r#"{"command": ["observe_property", 1, "fullscreen"]}"#);
        player.send_ipc(r#"{"command": ["observe_property", 2, "media-title"]}"#);
        player.send_ipc(r#"{"command": ["observe_property", 3, "duration"]}"#);
        player.send_ipc(r#"{"command": ["observe_property", 4, "width"]}"#);
        player.send_ipc(r#"{"command": ["observe_property", 5, "height"]}"#);
        Ok(player)
    }

    fn send_ipc(&mut self, line: &str) {
        if let Some(stream) = self.ipc.as_mut() {
            let _ = writeln!(stream, "{line}");
        }
    }

    /// Non-blocking check for mpv-reported fullscreen state changes (its OSC
    /// fullscreen button or the `f` key). Returns `Some(true/false)` the
    /// frame that state changes, `None` otherwise. Also drains and applies
    /// every other observed property-change on the same socket
    /// (`media-title`/`duration`/`width`/`height`, see `media_title` et al)
    /// - the IPC socket only has one reader, so this is the only place
    /// anything is allowed to read from it.
    pub fn poll_fullscreen_toggle(&mut self) -> Option<bool> {
        let stream = self.ipc.as_mut()?;
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => None,
            Ok(n) => {
                self.ipc_buf.extend_from_slice(&buf[..n]);
                let mut fullscreen_result = None;
                while let Some(pos) = self.ipc_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.ipc_buf.drain(..=pos).collect();
                    let Ok(text) = std::str::from_utf8(&line) else { continue };
                    let Ok(json) = serde_json::from_str::<serde_json::Value>(text.trim()) else { continue };
                    if json.get("event").and_then(|e| e.as_str()) != Some("property-change") {
                        continue;
                    }
                    let Some(name) = json.get("name").and_then(|n| n.as_str()) else { continue };
                    let data = json.get("data");
                    match name {
                        "fullscreen" => {
                            if let Some(v) = data.and_then(|d| d.as_bool()) {
                                fullscreen_result = Some(v);
                            }
                        }
                        "media-title" => self.media_title = data.and_then(|d| d.as_str()).map(str::to_string),
                        "duration" => self.duration_secs = data.and_then(|d| d.as_f64()),
                        "width" => self.width = data.and_then(|d| d.as_i64()),
                        "height" => self.height = data.and_then(|d| d.as_i64()),
                        _ => {}
                    }
                }
                fullscreen_result
            }
        }
    }

    pub fn is_native_fullscreen(&self) -> bool {
        self.is_native_fullscreen
    }

    /// mpv's own best title for what's playing - an embedded tag (e.g. an
    /// MKV's title metadata) if the file has one, otherwise just its
    /// filename. `None` until mpv has actually reported it (a moment after
    /// playback starts).
    pub fn media_title(&self) -> Option<&str> {
        self.media_title.as_deref()
    }

    /// Video resolution as `(width, height)`, once mpv has reported both.
    pub fn resolution(&self) -> Option<(i64, i64)> {
        Some((self.width?, self.height?))
    }

    /// Total duration, formatted as `h:mm:ss` (or `m:ss` under an hour).
    pub fn duration_label(&self) -> Option<String> {
        let total = self.duration_secs?.round() as i64;
        let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
        Some(if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") })
    }

    /// Reparent the embedded window to the root window and size it to cover
    /// the whole monitor, so mpv's fullscreen actually fills the screen
    /// instead of being clipped to our app's small embedded video area.
    pub fn enter_native_fullscreen(&mut self) {
        if self.is_native_fullscreen {
            return;
        }
        let _ = self.conn.reparent_window(self.win_id, self.screen_root, 0, 0);
        let _ = self.conn.configure_window(
            self.win_id,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(self.screen_w as u32)
                .height(self.screen_h as u32)
                .stack_mode(StackMode::ABOVE),
        );
        let _ = self.conn.flush();
        self.is_native_fullscreen = true;
    }

    /// Reparent the embedded window back into our app and restore it to the
    /// given (app-relative) geometry.
    pub fn exit_native_fullscreen(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.is_native_fullscreen {
            return;
        }
        let _ = self.conn.reparent_window(self.win_id, self.parent_xid, x as i16, y as i16);
        let _ = self.conn.configure_window(self.win_id, &ConfigureWindowAux::new().width(w).height(h));
        let _ = self.conn.flush();
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
        let _ = self
            .conn
            .configure_window(self.win_id, &ConfigureWindowAux::new().x(x).y(y).width(w).height(h));
        let _ = self.conn.flush();
    }

    /// Unmaps (or remaps) the embedded window. mpv renders as a real X11
    /// child window painted directly by the X server, not through egui's own
    /// draw pipeline, so it always paints over anything egui draws in that
    /// screen region - no z-order egui offers can put a popup above it. This
    /// is the only way to let an egui popup appear on top of the video: hide
    /// the video underneath it, then show it again once the popup closes.
    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden == self.hidden {
            return;
        }
        self.hidden = hidden;
        let _ = if hidden { self.conn.unmap_window(self.win_id) } else { self.conn.map_window(self.win_id) };
        let _ = self.conn.flush();
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for EmbeddedPlayer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.conn.destroy_window(self.win_id);
        let _ = self.conn.flush();
        let _ = std::fs::remove_file(&self.ipc_path);
    }
}

/// Extract the X11 window ID of our own app window, if the current backend
/// exposes one (X11/XWayland via Xlib or Xcb raw-window-handle variants).
pub fn own_window_handle(handle: raw_window_handle::RawWindowHandle) -> Option<isize> {
    use raw_window_handle::RawWindowHandle;
    match handle {
        RawWindowHandle::Xlib(h) => Some(h.window as isize),
        RawWindowHandle::Xcb(h) => Some(h.window.get() as isize),
        _ => None,
    }
}
