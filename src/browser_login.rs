use anyhow::{anyhow, Result};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::unix::{EventLoopBuilderExtUnix, WindowExtUnix};
use tao::window::WindowBuilder;
use wry::{WebViewBuilder, WebViewBuilderExtUnix};

/// Injected into the profile page once it's had a moment to render. Just
/// the whole rendered page's text - webtor.io's real markup can't be
/// inspected from here (Cloudflare blocks a plain fetch), and a prior
/// attempt at scoping this to just the tier card by walking up a fixed
/// number of DOM ancestors from an anchor text node proved unreliable: on
/// a real account it overshot into a giant wrapper holding the whole
/// settings page (Profile/Vault/Preferences/Stremio/Backends/Addons all in
/// one column), not a small card. `crate::webtor_auth` picks specific known
/// labels back out of this text instead of us trying to fence off "the
/// card" blind.
const SCRAPE_JS: &str = "document.body.innerText";

/// What a profile-page scrape found: the whole rendered page's text, or
/// `None` if the scrape never completed.
pub struct ProfileScrape {
    pub full_text: Option<String>,
}

/// `evaluate_script_with_callback` hands back the script's return value
/// JSON-encoded, so a plain string result arrives JSON-quoted.
fn parse_scrape_result(raw: &str) -> ProfileScrape {
    ProfileScrape { full_text: serde_json::from_str(raw).ok() }
}

/// What the login window captured: the session cookies (required), plus a
/// best-effort scrape of webtor.io/profile.
pub struct LoginCapture {
    pub cookies: Vec<(String, String)>,
    pub scrape: ProfileScrape,
}

pub type CookieResult = Result<LoginCapture, String>;

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

fn run() -> Result<LoginCapture> {
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
    // Once signed in, navigate the same window to webtor.io/profile and
    // scrape it (see `SCRAPE_JS`) before closing - best-effort only, a
    // failure/timeout here must never block finishing the login itself.
    let mut navigated_to_profile = false;
    let mut profile_nav_at: Option<Instant> = None;
    let mut scrape_requested = false;
    let scrape_result: Arc<Mutex<Option<ProfileScrape>>> = Arc::new(Mutex::new(None));
    event_loop.run_return(|event, _target, control_flow| {
        *control_flow = ControlFlow::Poll;
        gtk::main_iteration_do(false);

        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
            return;
        }

        if found.is_none() {
            // `cookies_for_url` only returns cookies whose Path is a prefix
            // of the queried URL's path (RFC 6265 cookie-path matching) - a
            // bare "https://webtor.io" query path is "/", so anything
            // narrowly path-scoped (SuperTokens' refresh-token cookie is
            // commonly scoped to its own refresh endpoint, not "/") never
            // came back. `cookies()` has no path filter at all.
            if let Ok(cookies) = webview.cookies() {
                if cookies.iter().any(|c| c.name() == "sAccessToken") {
                    found = Some(cookies.iter().map(|c| (c.name().to_string(), c.value().to_string())).collect());
                }
            }
        } else if !navigated_to_profile {
            navigated_to_profile = true;
            profile_nav_at = Some(Instant::now());
            let _ = webview.load_url("https://webtor.io/profile");
        } else if let Some(nav_at) = profile_nav_at {
            let elapsed = nav_at.elapsed();
            // The profile page is client-rendered, not static HTML - give it
            // a moment to actually paint before reading its text.
            if !scrape_requested && elapsed >= Duration::from_millis(1500) {
                scrape_requested = true;
                let slot = scrape_result.clone();
                let _ = webview.evaluate_script_with_callback(SCRAPE_JS, move |result| {
                    *slot.lock().unwrap() = Some(parse_scrape_result(&result));
                });
            }
            if scrape_result.lock().unwrap().is_some() || elapsed >= Duration::from_millis(4000) {
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

    let cookies = found.ok_or_else(|| anyhow!("window closed before signing in"))?;
    let scrape = scrape_result.lock().unwrap().take().unwrap_or(ProfileScrape { full_text: None });
    Ok(LoginCapture { cookies, scrape })
}
