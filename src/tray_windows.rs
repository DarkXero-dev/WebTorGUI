use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

/// Un-hides and focuses the window. `egui::Context` is `Send + Sync` and
/// safe to call from any thread - this is what makes it work even though
/// the window may be hidden and its own event loop otherwise idle.
pub fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Windows twin of `tray.rs`. Same public API, same behaviour: a tray icon
/// with Show/Quit, left-click to restore.
///
/// Unlike the login webview, this does NOT stand up a second event loop -
/// that is what crashed the app before (see src/browser_login_windows.rs).
/// `tray-icon` creates a hidden Win32 message window on *this* thread and
/// relies on whatever loop already pumps it, which is eframe's winit loop.
/// So this must be called from the main thread, during eframe setup, and
/// never from a worker.
///
/// Tray/menu clicks arrive on a global channel rather than through egui, so
/// they're consumed on background threads. That matters: while the window is
/// hidden, egui's `update()` never runs, so polling per-frame would make the
/// tray dead exactly when it's the only way back in.
pub fn spawn(ctx: egui::Context) {
    let icon = match load_icon() {
        Ok(icon) => icon,
        Err(_) => return,
    };

    let show = MenuItem::new("Show Webtor Desktop", true, None);
    // A real, unconditional exit - not routed through the window's
    // close-intercept (which deliberately treats a close as "minimize to
    // tray"), and not relying on the egui event loop noticing anything,
    // since the whole point of this menu item is to work while hidden.
    let quit = MenuItem::new("Quit", true, None);
    let show_id = show.id().clone();
    let quit_id = quit.id().clone();

    let menu = Menu::new();
    if menu.append(&show).is_err()
        || menu.append(&PredefinedMenuItem::separator()).is_err()
        || menu.append(&quit).is_err()
    {
        return;
    }

    let tray = TrayIconBuilder::new()
        .with_tooltip("Webtor Desktop")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        // Left-click restores the window (matching the Linux tray's
        // `activate`); the menu stays on right-click.
        .with_menu_on_left_click(false)
        .build();
    let Ok(tray) = tray else {
        // No tray (e.g. Explorer's notification area unavailable). Closing to
        // tray would then hide the window with no way back, so leave the
        // close-intercept to fall through to a plain quit.
        return;
    };
    // Leaking the handle keeps the tray icon alive for the process's
    // lifetime - dropping it would tear the icon down.
    std::mem::forget(tray);

    let menu_ctx = ctx.clone();
    std::thread::spawn(move || {
        let rx = MenuEvent::receiver();
        while let Ok(event) = rx.recv() {
            if event.id == show_id {
                show_window(&menu_ctx);
            } else if event.id == quit_id {
                std::process::exit(0);
            }
        }
    });

    std::thread::spawn(move || {
        let rx = TrayIconEvent::receiver();
        while let Ok(event) = rx.recv() {
            // Act on button *up*, or a single click fires twice (down + up).
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(&ctx);
            }
        }
    });
}

fn load_icon() -> anyhow::Result<Icon> {
    let img = image::load_from_memory(include_bytes!("../icons/icon.png"))?.into_rgba8();
    let (width, height) = img.dimensions();
    Ok(Icon::from_rgba(img.into_raw(), width, height)?)
}
