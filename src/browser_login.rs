use anyhow::{anyhow, Result};
use std::sync::mpsc::Sender;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::unix::{EventLoopBuilderExtUnix, WindowExtUnix};
use tao::window::WindowBuilder;
use wry::{WebViewBuilder, WebViewBuilderExtUnix};

pub type CookieResult = Result<Vec<(String, String)>, String>;

/// Opens a real, separate browser window (webkit2gtk via wry) pointed at
/// webtor.io/login. Our own reqwest client can't get past webtor.io's
/// Cloudflare bot challenge no matter what headers it sends (verified: same
/// UA, same everything, curl passes and reqwest doesn't - it comes down to
/// TLS/HTTP client fingerprinting) - a real browser engine passes it
/// normally. Once the user finishes the email-code login in this window, the
/// resulting SuperTokens session cookies are read directly out of it and
/// sent back over `result_tx`.
pub fn open_login_window(result_tx: Sender<CookieResult>) {
    std::thread::spawn(move || {
        let result = run();
        let _ = result_tx.send(result.map_err(|e| e.to_string()));
    });
}

fn run() -> Result<Vec<(String, String)>> {
    gtk::init().map_err(|e| anyhow!("could not init gtk: {e}"))?;

    // The webview runs on a dedicated thread so it doesn't block egui's own
    // event loop on the main thread - tao normally refuses that for platform
    // compatibility, so it must be opted into explicitly.
    let mut event_loop: EventLoop<()> = EventLoopBuilder::new().with_any_thread(true).build();
    let window = WindowBuilder::new()
        .with_title("Sign in to webtor.io")
        .with_inner_size(tao::dpi::LogicalSize::new(480.0, 720.0))
        .build(&event_loop)
        .map_err(|e| anyhow!("could not open browser window: {e}"))?;

    // tao's Window on Linux is a GTK window, not a raw X11 surface, so wry's
    // generic HasWindowHandle-based build() (which only accepts Xlib/Xcb
    // handles) rejects it - it has to go through the GTK-specific path.
    // gtk_window() itself is a GtkBin and already holds tao's default vbox as
    // its one child, so the webview must attach to that vbox instead.
    let vbox = window.default_vbox().ok_or_else(|| anyhow!("window has no content box"))?;
    let webview = WebViewBuilder::new()
        .with_url("https://webtor.io/login")
        .build_gtk(vbox)
        .map_err(|e| anyhow!("could not create embedded browser: {e}"))?;
    window.set_visible(true);
    window.set_focus();

    let mut found: Option<Vec<(String, String)>> = None;
    event_loop.run_return(|event, _target, control_flow| {
        *control_flow = ControlFlow::Poll;
        gtk::main_iteration_do(false);

        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
            return;
        }

        if let Ok(cookies) = webview.cookies_for_url("https://webtor.io") {
            if cookies.iter().any(|c| c.name() == "sAccessToken") {
                found = Some(cookies.iter().map(|c| (c.name().to_string(), c.value().to_string())).collect());
                *control_flow = ControlFlow::Exit;
            }
        }
    });

    // Tear the widgets down explicitly, then keep pumping the GTK main loop
    // for a moment - dropping them at function-return has nowhere left to
    // process the resulting destroy/unmap, which is why the window used to
    // sit on screen frozen and unresponsive after a successful sign-in
    // instead of actually closing.
    drop(webview);
    drop(window);
    for _ in 0..50 {
        gtk::main_iteration_do(false);
    }

    found.ok_or_else(|| anyhow!("window closed before signing in"))
}
