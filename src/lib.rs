pub mod auth;
// webtor.io's Cloudflare challenge can't be passed by a plain HTTP client,
// so login happens in a real embedded browser whose cookies we then read.
// That needs platform-specific windowing - webkit2gtk on Linux, WebView2 on
// Windows - so this mirrors the `player` split below: two files, one module
// name, one shared `open_login_window` API, no cfg gates for callers.
//
// macOS is still unsupported: the webview runs its own event loop on a
// background thread, which is a hard incompatibility with Cocoa (macOS
// requires all windowing on the true main thread). Porting there needs a
// real multi-viewport redesign, not something to guess at untested.
#[cfg(all(target_os = "linux", feature = "embedded-login"))]
#[path = "browser_login.rs"]
pub mod browser_login;
#[cfg(all(target_os = "windows", feature = "embedded-login"))]
#[path = "browser_login_windows.rs"]
pub mod browser_login;
// Windows 7 build only (see the `win7` feature in Cargo.toml): WebView2 has
// had no security updates since Jan 2023 on Win7, and the Evergreen
// bootstrapper refuses to install there at all anymore, so there's no
// embedded browser to speak of. Login instead opens the user's own system
// browser and asks them to paste the session cookie back in. See
// src/manual_login.rs and the login UI branch in src/ui/app.rs.
#[cfg(all(target_os = "windows", feature = "win7"))]
pub mod manual_login;
pub mod db;
pub mod downloads;
// Embedded video playback (mpv reparented into our own window) needs
// platform-specific window-handling code - X11 reparenting on Linux, Win32
// child windows on Windows. Both files expose the same `EmbeddedPlayer`
// shape, so nothing outside this module needs a cfg gate of its own.
#[cfg(target_os = "linux")]
#[path = "player.rs"]
pub mod player;
#[cfg(target_os = "windows")]
#[path = "player_windows.rs"]
pub mod player;
pub mod settings;
#[cfg(target_os = "linux")]
pub mod single_instance;
pub mod torrent;
pub mod torrent_engine;
// The system tray, like `player` above: two files, one module name, one
// shared `spawn`/`show_window` API. Linux speaks StatusNotifierItem over
// D-Bus (ksni); Windows uses the Win32 notification area (tray-icon).
// Windows gets it only with the `tray` feature (off for win7 - see Cargo.toml).
#[cfg(target_os = "linux")]
#[path = "tray.rs"]
pub mod tray;
#[cfg(all(target_os = "windows", feature = "tray"))]
#[path = "tray_windows.rs"]
pub mod tray;
pub mod ui;
pub mod webtor_auth;

use std::sync::{Arc, Mutex};

pub fn run() {
    // Single-instance enforcement and the tray icon are both Linux-specific
    // (StatusNotifierItem over D-Bus, and a Unix domain socket) - on other
    // platforms this build simply doesn't have those two conveniences yet.
    #[cfg(target_os = "linux")]
    let Some(instance_listener) = single_instance::acquire() else {
        return;
    };

    // Embedded video playback reparents an mpv window into our own X11
    // window, which requires our app to actually be an X11 client rather
    // than a native Wayland surface.
    #[cfg(target_os = "linux")]
    std::env::remove_var("WAYLAND_DISPLAY");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let _guard = rt.enter();

    let app_settings = Arc::new(Mutex::new(settings::load_settings()));
    let db_conn = db::open().expect("open database");

    let (dl_tx, dl_rx) = std::sync::mpsc::channel::<downloads::engine::DownloadEvent>();

    downloads::scheduler::start(Arc::clone(&app_settings), dl_tx.clone());

    let torrent_output_dir = {
        let settings = app_settings.lock().unwrap();
        std::path::PathBuf::from(&settings.download_dir).join("torrents")
    };
    std::fs::create_dir_all(&torrent_output_dir).expect("create torrents output dir");
    let torrent_engine = Arc::new(
        rt.block_on(torrent_engine::TorrentEngine::new(torrent_output_dir))
            .expect("start torrent engine"),
    );

    // Without this, the running window has no icon of its own to report to
    // the window manager - on Linux, a dock/taskbar showing the live window
    // (rather than looking up a `.desktop` file, which dev builds don't even
    // have installed) falls back to a blank/generic icon.
    let icon = {
        let bytes = include_bytes!("../icons/icon.png");
        let img = image::load_from_memory(bytes).expect("decode embedded app icon").into_rgba8();
        let (width, height) = img.dimensions();
        egui::IconData { rgba: img.into_raw(), width, height }
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 750.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Webtor Desktop")
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        "Webtor Desktop",
        native_options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);

            // Both the tray icon and a second launch need to act on the
            // window directly (show/focus/exit) even while it's hidden and
            // its own event loop is otherwise idle - so they get a context
            // clone here rather than going through app-side polling.
            //
            // On Windows this must happen here, on the main thread: the tray
            // hooks into eframe's own message loop (see tray_windows.rs).
            #[cfg(any(target_os = "linux", all(target_os = "windows", feature = "tray")))]
            tray::spawn(cc.egui_ctx.clone());

            #[cfg(target_os = "linux")]
            {
                let raise_ctx = cc.egui_ctx.clone();
                std::thread::spawn(move || {
                    for stream in instance_listener.incoming().flatten() {
                        drop(stream);
                        tray::show_window(&raise_ctx);
                    }
                });
            }

            Ok(Box::new(ui::app::WebtorApp::new(
                cc,
                app_settings,
                db_conn,
                dl_tx,
                dl_rx,
                torrent_engine,
            )))
        }),
    )
    .expect("eframe run");
}
