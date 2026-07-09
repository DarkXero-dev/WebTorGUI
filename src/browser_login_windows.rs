use anyhow::{anyhow, Result};
use std::sync::mpsc::Sender;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::EventLoopBuilderExtWindows;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

pub type CookieResult = Result<Vec<(String, String)>, String>;

/// Opens a real, separate browser window (WebView2 via wry) pointed at
/// webtor.io/login. Our own reqwest client can't get past webtor.io's
/// Cloudflare bot challenge no matter what headers it sends - a real browser
/// engine passes it normally. Once the user finishes the email-code login in
/// this window, the resulting SuperTokens session cookies are read directly
/// out of it and sent back over `result_tx`.
///
/// The Windows twin of `browser_login.rs`. Same public shape, same cookie
/// contract; only the windowing differs (WebView2 needs no GTK, and tao's
/// Windows window is a real HWND that satisfies wry's generic
/// `HasWindowHandle` build path).
pub fn open_login_window(result_tx: Sender<CookieResult>) {
    std::thread::spawn(move || {
        let result = run();
        let _ = result_tx.send(result.map_err(|e| e.to_string()));
    });
}

fn run() -> Result<Vec<(String, String)>> {
    // The webview runs on a dedicated thread so it doesn't block egui's own
    // event loop on the main thread - tao normally refuses that for platform
    // compatibility, so it must be opted into explicitly. Unlike Cocoa (see
    // src/lib.rs), Win32 has no main-thread-only windowing rule; tao's own
    // docs note only that the window dies with its thread, which is exactly
    // what we want once the cookie is captured.
    let mut event_loop: EventLoop<()> = EventLoopBuilder::new().with_any_thread(true).build();
    let window = WindowBuilder::new()
        .with_title("Sign in to webtor.io")
        .with_inner_size(tao::dpi::LogicalSize::new(480.0, 720.0))
        .build(&event_loop)
        .map_err(|e| anyhow!("could not open browser window: {e}"))?;

    let webview = WebViewBuilder::new()
        .with_url("https://webtor.io/login")
        .build(&window)
        .map_err(|e| anyhow!("could not create embedded browser: {e}"))?;

    let mut found: Option<Vec<(String, String)>> = None;
    event_loop.run_return(|event, _target, control_flow| {
        *control_flow = ControlFlow::Poll;

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

    drop(webview);
    drop(window);

    found.ok_or_else(|| anyhow!("window closed before signing in"))
}
