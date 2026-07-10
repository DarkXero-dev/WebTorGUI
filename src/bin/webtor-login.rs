// Separate login-window process for Windows. See src/browser_login_windows.rs
// for why the login webview must not share the GUI process: two winit-family
// event loops (eframe's winit + this tao/WebView2 loop) in one process corrupt
// each other and crash inside a window procedure. This binary owns its own true
// main thread and runs exactly one event loop.
//
// It opens webtor.io/login in a real WebView2 window, waits for the user to sign
// in, reads the SuperTokens session cookies, prints them to stdout as
// `WEBTOR_COOKIES <json>`, and exits 0. On cancel/failure it prints a reason to
// stderr and exits non-zero. The parent (browser_login_windows.rs) reads that.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    #[cfg(all(target_os = "windows", feature = "embedded-login"))]
    imp::main_impl();

    #[cfg(not(all(target_os = "windows", feature = "embedded-login")))]
    {
        // Linux signs in through the in-process webkit2gtk path; this helper is
        // Windows-only and never invoked elsewhere.
        eprintln!("webtor-login is only used on Windows");
        std::process::exit(2);
    }
}

#[cfg(all(target_os = "windows", feature = "embedded-login"))]
mod imp {
    use anyhow::{anyhow, Result};
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    #[derive(Debug)]
    enum AppEvent {
        LoggedIn,
    }

    // SuperTokens sets `sFrontToken` as a NON-HttpOnly cookie specifically so the
    // frontend can see it; its appearance means the session cookies (including
    // the HttpOnly `sAccessToken`) are now set. JS can't read `sAccessToken`, so
    // it just pings us and we read the real cookies out-of-band below.
    //
    // The cookie is `sFrontToken`, NOT `front-token` - that is the name of the
    // *header* SuperTokens sends it in. Watching for `front-token=` here matches
    // nothing, so the window hangs open forever after a successful sign-in.
    // Verified against a live session: `document.cookie` on the post-login page
    // reads `lang=en; st-last-access-token-update=...; sFrontToken=...`.
    //
    // This ping is only an auto-close convenience. Sign-in is *detected* by
    // reading the real cookie jar once the loop ends, so a wrong guess here
    // costs the user an extra window close, not a broken login.
    const DETECT_JS: &str = r#"
        setInterval(function () {
          if (document.cookie.indexOf('sFrontToken=') !== -1) {
            try { window.ipc.postMessage('logged-in'); } catch (e) {}
          }
        }, 400);
    "#;

    pub fn main_impl() {
        match run() {
            Ok(cookies) => {
                let json = serde_json::to_string(&cookies).unwrap_or_else(|_| "[]".to_string());
                println!("WEBTOR_COOKIES {json}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    fn run() -> Result<Vec<(String, String)>> {
        let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();

        let window = WindowBuilder::new()
            .with_title("Sign in to webtor.io")
            .with_inner_size(tao::dpi::LogicalSize::new(480.0, 720.0))
            .build(&event_loop)
            .map_err(|e| anyhow!("could not open browser window: {e}"))?;

        let ipc_proxy = proxy.clone();
        let webview = WebViewBuilder::new()
            .with_url("https://webtor.io/login")
            .with_initialization_script(DETECT_JS)
            .with_ipc_handler(move |req: wry::http::Request<String>| {
                if req.body() == "logged-in" {
                    // Wakes the loop below; safe to call from the WebView2 thread.
                    let _ = ipc_proxy.send_event(AppEvent::LoggedIn);
                }
            })
            .build(&window)
            .map_err(|e| anyhow!("could not create embedded browser: {e}"))?;

        event_loop.run_return(|event, _target, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                // Either the page told us it signed in, or the user closed the
                // window. Both mean "stop waiting and go look at the cookies" -
                // the jar, not the event, decides whether we actually have a
                // session. That way sign-in still works if the auto-close ping
                // never arrives (the user just closes the window when done).
                Event::UserEvent(AppEvent::LoggedIn)
                | Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => *control_flow = ControlFlow::Exit,
                _ => {}
            }
        });

        // Read cookies only AFTER run_return has returned. `cookies_for_url` is
        // synchronous only because wry pumps a nested Win32 message loop until
        // WebView2's async GetCookies completes; calling it from inside the
        // event-loop callback would re-enter the (non-reentrant) runner. With
        // the loop finished there is no active callback to re-enter.
        let cookies = webview
            .cookies_for_url("https://webtor.io")
            .map_err(|e| anyhow!("could not read the session cookies: {e}"))?;
        let pairs: Vec<(String, String)> = cookies
            .iter()
            .map(|c| (c.name().to_string(), c.value().to_string()))
            .collect();

        drop(webview);
        drop(window);

        if !pairs.iter().any(|(n, _)| n == "sAccessToken") {
            return Err(anyhow!("window closed before signing in"));
        }
        Ok(pairs)
    }
}
