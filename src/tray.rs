use ksni::menu::StandardItem;
use ksni::{MenuItem, Tray, TrayMethods};

struct AppTray {
    ctx: egui::Context,
}

impl Tray for AppTray {
    fn id(&self) -> String {
        "webtorapp".into()
    }

    fn title(&self) -> String {
        "Webtor Desktop".into()
    }

    fn icon_name(&self) -> String {
        "applications-multimedia".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        show_window(&self.ctx);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Show Webtor Desktop".into(),
                activate: Box::new(|tray: &mut Self| show_window(&tray.ctx)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                // A real, unconditional exit - not routed through the
                // window's close-intercept (which deliberately treats a
                // close as "minimize to tray"), and not relying on the
                // egui event loop noticing anything, since the whole point
                // of this menu item is to work even while it's hidden.
                activate: Box::new(|_tray: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Un-hides and focuses the window. `egui::Context` is `Send + Sync` and
/// safe to call from any thread - this is what makes it work even though
/// the window may be hidden and its own event loop otherwise idle.
pub fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Spawns the system tray icon (StatusNotifierItem via D-Bus - no GTK main
/// loop needed, unlike a libappindicator-based tray). Must run inside a
/// tokio context. Silently does nothing if the desktop has no working tray
/// implementation - closing to tray then just hides the window with no way
/// back except relaunching the app, which single-instance detection raises.
pub fn spawn(ctx: egui::Context) {
    tokio::spawn(async move {
        if let Ok(handle) = (AppTray { ctx }).spawn().await {
            // Leaking the handle keeps the tray alive for the process's
            // lifetime - dropping it would tear the tray icon down.
            std::mem::forget(handle);
        }
    });
}
