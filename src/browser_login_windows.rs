use anyhow::{anyhow, Result};
use std::sync::mpsc::Sender;

pub type CookieResult = Result<Vec<(String, String)>, String>;

/// Windows twin of `browser_login.rs`. Same public shape, same cookie contract.
///
/// Unlike Linux (where the webkit2gtk webview runs on a background thread inside
/// this process), the Windows login window runs in a **separate helper process**
/// (`webtor-login.exe`, installed beside us). Two reasons it cannot run
/// in-process:
///
/// 1. eframe drives a winit event loop on our main thread; a second
///    tao/WebView2 event loop in the same process corrupts winit's per-window
///    state and faults inside a window procedure (observed as an access
///    violation on the login thread the instant the webview started).
/// 2. winit-family event loops want their process's true main thread. A helper
///    process gives the webview exactly that, with nothing else contending.
///
/// The helper captures the SuperTokens session cookies and writes them to its
/// stdout; we spawn it, wait, and hand the cookies back over `result_tx` -
/// identical to what the Linux path sends.
pub fn open_login_window(result_tx: Sender<CookieResult>) {
    std::thread::spawn(move || {
        let result = run_helper();
        let _ = result_tx.send(result.map_err(|e| e.to_string()));
    });
}

/// Marker line the helper prints on success: `WEBTOR_COOKIES <json array>`.
const COOKIE_PREFIX: &str = "WEBTOR_COOKIES ";

fn run_helper() -> Result<Vec<(String, String)>> {
    let exe = std::env::current_exe().map_err(|e| anyhow!("cannot locate our own exe: {e}"))?;
    let helper = exe
        .parent()
        .ok_or_else(|| anyhow!("our exe has no parent directory"))?
        .join("webtor-login.exe");
    if !helper.exists() {
        return Err(anyhow!(
            "login helper missing at {} - reinstall the app",
            helper.display()
        ));
    }

    let output = std::process::Command::new(&helper)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("could not start the login window: {e}"))?
        .wait_with_output()
        .map_err(|e| anyhow!("the login window exited abnormally: {e}"))?;

    if !output.status.success() {
        // The helper prints a human-readable reason to stderr (e.g. the user
        // closed the window before signing in).
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .last()
            .unwrap_or("sign-in was cancelled");
        return Err(anyhow!("{reason}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout
        .lines()
        .find_map(|l| l.strip_prefix(COOKIE_PREFIX))
        .ok_or_else(|| anyhow!("the login window returned no session"))?;

    let cookies: Vec<(String, String)> =
        serde_json::from_str(json).map_err(|e| anyhow!("could not read the captured session: {e}"))?;
    Ok(cookies)
}
