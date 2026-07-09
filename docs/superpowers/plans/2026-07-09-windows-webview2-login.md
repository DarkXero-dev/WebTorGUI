# Windows WebView2 Login Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

## Context

**The Windows build cannot be logged into. At all.** It is not a bug, a
regression, or a broken flow — the login UI is compiled out of the binary,
and no alternative login mechanism exists on any platform.

webtor.io cannot be authenticated against with a plain HTTP client. The site
sits behind a Cloudflare bot challenge that `reqwest` cannot pass (verified
by the original author: identical headers, curl gets through and reqwest
doesn't — it comes down to TLS/HTTP client fingerprinting, not anything we
send). The site also has no password field, only an email code or
Google/Patreon OAuth whose redirect URI is fixed to webtor's own web
callback rather than a localhost port we could intercept. See the doc
comments at `src/webtor_auth.rs:9` and `src/browser_login.rs:12`.

So the only way in is a real browser engine. `src/browser_login.rs` opens a
`wry` webview at `https://webtor.io/login`, lets the user finish the
email-code flow, polls the webview's cookie jar until the SuperTokens
`sAccessToken` cookie appears, and hands those cookies back to
`WebtorAuth::import_cookies`. That module is `#[cfg(target_os = "linux")]`
(`src/lib.rs:7`), and `wry`/`tao`/`gtk` are declared only under
`[target.'cfg(target_os = "linux")'.dependencies]` (`Cargo.toml:69`).

**What a Windows user sees today:** the branded login screen renders, and
where Linux shows a pink "Sign in with Browser" button, Windows shows a grey
sentence — *"Sign-in isn't available on this platform yet - the login window
needs a Linux-only windowing feature."* (`src/ui/app.rs:1010-1016`). There is
no button, no field, no OAuth, no token box. Because `ui()` returns early
whenever `!self.logged_in` (`src/ui/app.rs:3674`), the sidebar and all pages
are unreachable. The app can render exactly one screen, forever.

Windows cannot inherit a session either. `WebtorApp::new` does attempt
auto-login on every platform (`src/ui/app.rs:716`), but the only caller of
`save_session()` is inside the Linux-only webview handler
(`src/ui/app.rs:896`), so the encrypted session file is never written on
Windows. The decryption key is machine-bound via `machine_uid`
(`src/auth.rs:17`), so hand-copying a session blob from a Linux box would
not work either.

**Intended outcome:** Windows gets a real "Sign in with Browser" button that
opens a WebView2 login window and captures the same cookies, reaching full
parity with Linux. Nothing downstream of `import_cookies` changes — the
cookie jar, the encryption at rest, and the whole authenticated app already
compile and work on Windows today.

## The key research finding (do not re-derive this)

**`wry` already supports Windows via WebView2.** The `Cargo.toml:69` comment
saying "wry/tao need GTK on Linux" is easy to misread as "wry is Linux-only."
It is not. It means: *on Linux*, wry needs GTK. On Windows, wry drives
WebView2 through `webview2-com`, and that dependency is already resolved in
`Cargo.lock` at `webview2-com 0.38.2`.

Three facts, each verified against upstream source/docs rather than assumed:

1. **`WebView::cookies_for_url(&self, url: &str) -> Result<Vec<Cookie>>`
   carries no platform-specific restriction.** Only `cookies()` documents an
   unsupported platform, and that platform is Android. The Windows/WebView2
   backend supports cookie reads. This is the single API the whole login
   flow depends on, and it is the same call the Linux code already makes.

2. **`WebViewBuilder::build<W: HasWindowHandle>(self, window: &'a W)` is the
   generic path and works on Windows.** `build_gtk()` exists only because
   tao's *Linux* window is a GTK widget rather than a raw surface. A tao
   `Window` on Windows implements `HasWindowHandle`, so the Windows
   implementation calls plain `.build(&window)`.

3. **`tao::platform::windows::EventLoopBuilderExtWindows::with_any_thread`
   exists**, mirroring the Unix trait the Linux code already uses. Its doc
   warns "any Window created on the new thread will be destroyed when the
   thread terminates" — which is exactly the desired behavior here, since
   the login thread ends once the cookie is captured.

**The `src/lib.rs:2` comment about a "multi-viewport redesign" does not
block this work.** Read it carefully: it says the off-main-thread event loop
"works on X11 but is a hard incompatibility with **Cocoa** (macOS requires
all windowing on the true main thread)." That is a macOS constraint. Windows
has no such rule, and `with_any_thread` is explicitly provided for it. Do not
let that comment scare you into a viewport rewrite.

## Architecture

Mirror the **existing `player.rs` / `player_windows.rs` pattern** exactly
(`src/lib.rs:15-20`): two files, one `#[path]`-selected module name, one
shared public API, zero cfg gates for callers.

```
src/browser_login.rs          (unchanged, Linux, GTK/webkit2gtk)
src/browser_login_windows.rs  (new, Windows, WebView2)
        both expose:  pub type CookieResult
                      pub fn open_login_window(tx: Sender<CookieResult>)
```

The four login call sites in `src/ui/app.rs` currently gated
`#[cfg(target_os = "linux")]` widen to
`#[cfg(any(target_os = "linux", target_os = "windows"))]`, and the dead
"not available" branch narrows to `#[cfg(not(any(...)))]` so macOS keeps
showing it.

## Tech Stack

Rust, egui/eframe, `tao` 0.35.3 + `wry` 0.55.1 (already pinned in
`Cargo.lock` for Linux; this plan adds them to the Windows target too).

## Global Constraints

- **No Linux behavior changes whatsoever.** `src/browser_login.rs` is not
  edited. `player.rs`, `tray.rs`, and `single_instance.rs` stay Linux-only.
- **`windows` crate version hazard.** `Cargo.toml:48` carries a hard-won
  warning: our direct `windows = "0.62"` dep is pinned to match
  `accesskit_windows`, because a mismatch puts two mutually-incompatible
  `windows_core` crates in the graph and breaks windows-rs's `Param`/
  `TypeKind` trait machinery. wry pulls `webview2-com 0.38.2` →
  `windows 0.61.3` / `windows-core 0.61.2`, which **coexists safely** with
  our `0.62.2` *only because we never pass windows-rs types across the wry
  boundary*. Do not `use` anything from wry's transitive `windows` crate,
  and do not "fix" the duplicate by loosening our own pin.
- **Do not hand-roll raw `ICoreWebView2CookieManager` COM.** wry wraps it.
  A raw COM port would reintroduce exactly the `windows_core` boundary
  problem the comment above describes.
- **Verification is build-only in the dev environment** (see Verification).
  Never claim the login flow is "verified" without a real Windows run.

---

### Task 1: Add `tao` + `wry` to the Windows target

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Correct the misleading Linux dependency comment**

In `Cargo.toml`, the `[target.'cfg(target_os = "linux")'.dependencies]`
block's comment currently reads "The login webview (embedded browser) and
system tray are Linux-only for now". Replace with:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
x11rb = "0.13"
# tao/wry here are the Linux half of the login webview - on Linux they need
# GTK/webkit2gtk, which is why the deps differ per platform (the Windows
# half, below, drives WebView2 through the same wry API). The system tray is
# still genuinely Linux-only: ksni's StatusNotifierItem protocol is
# D-Bus-specific. See src/lib.rs for the matching #[cfg] gates.
tao = "0.35.3"
wry = "0.55"
gtk = "0.18"
ksni = { version = "0.3.5", default-features = false, features = ["async-io", "blocking"] }
```

- [ ] **Step 2: Add them to the Windows block**

In the existing `[target.'cfg(target_os = "windows")'.dependencies]` block
(the one holding `windows = { version = "0.62", ... }`), append:

```toml
# The Windows half of the login webview. wry drives WebView2 here (via its
# own transitive webview2-com / windows 0.61), with no GTK involved - the
# generic WebViewBuilder::build() path, not build_gtk(). Deliberately NOT
# unified with the `windows` dep above: nothing from wry's windows-rs
# version ever crosses into our code, so the two coexist. See
# src/browser_login_windows.rs.
tao = "0.35.3"
wry = "0.55"
```

- [ ] **Step 3: Verify the graph resolves**

Run: `cargo tree --target x86_64-pc-windows-msvc -i windows`
Expected: both `windows 0.61.x` (under wry/webview2-com) and `windows 0.62.x`
(direct + accesskit) present. That duplication is expected and safe. If
`cargo tree` cannot resolve the Windows target from this host, `cargo
check --target x86_64-pc-windows-msvc` failing on *toolchain* grounds is
fine — a *resolution* error is not. Defer to CI (Task 5) if unsure.

---

### Task 2: `src/browser_login_windows.rs`

**Files:**
- Create: `src/browser_login_windows.rs`

**Interfaces:**
- Produces: `pub type CookieResult` and `pub fn open_login_window(Sender<CookieResult>)`,
  byte-identical in signature to `src/browser_login.rs:10` and `:20`.
  Consumed by Task 4 (`src/ui/app.rs`).

- [ ] **Step 1: Write the file**

Read `src/browser_login.rs` first and mirror its structure and comment
density. The Windows version is the same shape minus GTK: no `gtk::init()`,
no `default_vbox()`, no `main_iteration_do` pump, and `.build(&window)`
instead of `.build_gtk(vbox)`.

```rust
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
```

**Implementation notes for whoever runs this:**
- `ControlFlow::Poll` busy-polls the cookie jar every frame. The Linux code
  does the same and it is acceptable for a short-lived login window, but if
  WebView2 shows high CPU, switch to
  `ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(200))`.
- If `build(&window)` rejects the tao window over a `raw-window-handle`
  version mismatch (wry and tao must agree on the `HasWindowHandle` trait),
  check `cargo tree -p raw-window-handle`. The crate is already a direct
  dependency at `0.6` (`Cargo.toml:41`) for the player, so agreement is
  expected — but this is the single most likely compile failure.
- The Linux file's trailing `main_iteration_do` pump exists to flush GTK
  destroy events. **Do not port it.** WebView2 tears down with the thread.

---

### Task 3: Wire the module into `src/lib.rs`

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Replace the Linux-only gate with the two-file pattern**

Replace lines 2-8 (the comment plus `#[cfg(target_os = "linux")] pub mod
browser_login;`) with the `#[path]` pattern already used for `player` just
below it:

```rust
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
#[cfg(target_os = "linux")]
#[path = "browser_login.rs"]
pub mod browser_login;
#[cfg(target_os = "windows")]
#[path = "browser_login_windows.rs"]
pub mod browser_login;
```

Note the macOS caveat **moves here from its old home** — it was describing
`browser_login`, not the crate. Do not delete it.

---

### Task 4: Widen the login gates in `src/ui/app.rs`

**Files:**
- Modify: `src/ui/app.rs`

There are exactly **five** sites. Find them with
`grep -n 'target_os' src/ui/app.rs` — do not trust the line numbers below
once earlier tasks have shifted the file.

- [ ] **Step 1: The `browser_login_rx` struct field (~line 592)**

```rust
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    browser_login_rx: Option<Receiver<crate::browser_login::CookieResult>>,
```

- [ ] **Step 2: Its initializer in `WebtorApp::new` (~line 769)**

```rust
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            browser_login_rx: None,
```

- [ ] **Step 3: The webview-result polling block in `login_page` (~line 886)**

Change only the `#[cfg(...)]` attribute on the block. The body — which calls
`import_cookies`, sets `logged_in`, and calls `save_session` — is already
platform-agnostic and must not be touched:

```rust
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            if let Some(rx) = &self.browser_login_rx {
```

- [ ] **Step 4: The "Sign in with Browser" button (~line 975)**

```rust
                        #[cfg(any(target_os = "linux", target_os = "windows"))]
                        {
                            ui.label(
                                RichText::new("webtor.io's bot protection blocks a plain login form here - sign in through a real browser window instead.")
```

The `crate::browser_login::open_login_window(tx)` call inside now resolves on
both platforms via Task 3. No other edit inside this block.

- [ ] **Step 5: Narrow the "not available" fallback (~line 1010)**

```rust
                        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                        {
                            ui.label(
                                RichText::new("Sign-in isn't available on this platform yet - the login window needs a windowing feature this build doesn't have. Linux and Windows builds support it today.")
                                    .size(12.0)
                                    .color(super::theme::MUTED),
                            );
                        }
```

- [ ] **Step 6: Fix the stale doc comment on `login_page` (~lines 878-884)**

It currently claims the email-code flow is one "a plain HTTP client can do
end-to-end" and that `login_page` implements it. That is false and
contradicts `webtor_auth.rs:9` — it is a leftover from an abandoned
approach. Replace the final sentence so it describes the webview:

```rust
    /// no password field on the real site, only an email code + Google/Patreon
    /// OAuth. Neither is reachable from a plain HTTP client: webtor.io's
    /// Cloudflare challenge rejects reqwest outright (see
    /// `crate::webtor_auth`), and webtor's OAuth redirect URI is fixed to
    /// their own web callback, not a localhost port we could intercept. So
    /// this hands off to a real embedded browser window
    /// (`crate::browser_login`) and imports the cookies it captures.
```

- [ ] **Step 7: Confirm no gate was missed**

Run: `grep -n 'target_os' src/ui/app.rs`
Expected: the remaining `linux`-only gates are the tray/close ones (~3450,
~3455) and the `windows`-only player ones (~2537, ~3484, ~3670). **No
login-related site should still say bare `target_os = "linux"`.**

---

### Task 5: Build verification

**Files:** none

- [ ] **Step 1: Linux build must stay clean**

Run: `cargo build --bin webtorapp && cargo test`
Expected: clean build, all tests pass. This proves the widened cfgs did not
break Linux and that `browser_login_windows.rs` is correctly excluded.

- [ ] **Step 2: Typecheck the Windows target from Linux**

Do this **before** pushing — it catches every expected Task 2 failure mode
except runtime behavior, in seconds rather than a CI round-trip.

```
rustup target add x86_64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc --bin webtorapp
```

This works without any MSVC toolchain because `cargo check` never links the
target crate, and proc-macros / build scripts compile for the **host**. In
particular `build.rs:17` gates the `winresource` icon embedding on
`#[cfg(target_os = "windows")]`, which in a build script means the *host*
platform — so cross-checking from Linux skips resource compilation entirely
and never reaches for `rc.exe`.

> Do not "fix" `build.rs:17` to use `CARGO_CFG_TARGET_OS`. It looks like a
> bug (and technically is — it means a cross-compiled exe gets no icon), but
> CI builds on `windows-latest` where host == target, so the icon works
> where it ships, and the current form is what makes this Linux cross-check
> possible. Out of scope.

If `cargo check` fails on a *dependency's* build script rather than on our
code (most likely `webview2-com-sys`), don't fight it — fall back to
`cargo xwin check --target x86_64-pc-windows-msvc` (`cargo install
cargo-xwin`), which supplies the MSVC headers/libs, or skip to Step 3.

- [ ] **Step 3: Real Windows compile via CI**

Push the branch and let the existing `windows-latest` job in
`.github/workflows/build.yml` compile it. It runs a plain `cargo build
--release` with no extra flags, so it picks up the new deps automatically.
**No CI changes are needed or in scope.**

Expected failure modes, in likelihood order:
1. `raw-window-handle` version disagreement between tao and wry (see Task 2).
2. `EventLoopBuilderExtWindows` not in scope — the `use` is easy to forget.
3. `windows_core` trait-machinery errors → you violated the Task 1
   constraint and let a wry windows-rs type touch our code.

- [ ] **Step 4: Commit**

Leave uncommitted for review, consistent with the sibling Windows plans in
this directory.

---

## Verification

**Build verification is not behavior verification.** A green `cargo build` on
both platforms proves the cfg gating is right and nothing else. State that
plainly when reporting completion rather than claiming the login "works."

The flow can only be confirmed by a real Windows run. **This is the only
step that requires touching a Windows machine, and it needs no toolchain
there — just install the CI-built `.exe` and click.**

1. Download the NSIS `.exe` the CI `windows` job publishes, install, launch.
2. The login screen must now show a pink **"Sign in with Browser"** button
   where it previously showed the grey "not available" sentence.
3. Click it. A 480×720 window titled *"Sign in to webtor.io"* must open on
   `https://webtor.io/login` — and must render the real page, not a
   Cloudflare block. (If it shows a challenge, the WebView2 engine is being
   fingerprinted differently than webkit2gtk, and this whole approach needs
   rethinking. Surface that immediately; it invalidates the plan.)
4. Complete the email-code sign-in. On success the login window must close by
   itself, and the main app must advance past the login screen to the
   sidebar and Discover page.
5. Fully quit and relaunch. The app must come up **already signed in** —
   this is the real proof, since it means `save_session` wrote the encrypted
   blob (`src/ui/app.rs:896`) and `load_session` decrypted it against this
   machine's `machine_uid` (`src/auth.rs:17`). This step never passed on
   Windows before, at all.
6. Close the login window without signing in. Expect the error
   *"Browser window closed without a valid session."* and no crash.

## Known deployment risk (out of scope, flag it, do not fix it here)

WebView2 requires the Evergreen Runtime on the target machine. Windows 11
ships it; Windows 10 has it on any box with a current Edge, which is nearly
all of them. If `WebViewBuilder::build()` fails at runtime on a clean Win10
VM, the fix is bundling the WebView2 bootstrapper in the NSIS installer —
that is a packaging change, explicitly **not** part of this plan. Note it in
the completion report if it comes up.

## Where to run this

**All development happens on Linux.** The repo owner works exclusively on
Linux; the Windows machine is a gaming box, kept only for running things,
never for building them. Plan accordingly — this is not a preference to be
optimized away by suggesting a Windows toolchain install.

The workflow:

| Step | Where | Needs |
|---|---|---|
| Tasks 1–4 (all code) | Linux | nothing extra |
| Task 5 Step 1 (Linux build + tests) | Linux | nothing extra |
| Task 5 Step 2 (Windows typecheck) | Linux | `rustup target add x86_64-pc-windows-msvc` |
| Task 5 Step 3 (Windows compile) | CI | push the branch |
| Verification (login actually works) | Windows gaming box | install the CI `.exe` |

The one irreducible Windows step is the final Verification run, and it needs
no compiler there — download the installer CI publishes, run it, click
through the login. That is the entire Windows footprint of this task.

**Do not suggest installing Visual Studio Build Tools on the Windows box.**
It was checked on 2026-07-09: `rustup 1.29.0` / `rustc 1.97.0` are present
(host `x86_64-pc-windows-msvc`) but there is no MSVC linker, no VS, no
Windows SDK. Installing them would work, and is beside the point — the
machine is not a development machine. CI is the Windows compiler.

Two toolchain facts to save the next session some confusion:

- `cargo check --target x86_64-pc-windows-msvc` from Linux is the fast
  feedback loop and needs no MSVC anything (see Task 5 Step 2 for why).
- On that Windows box specifically, a bare `cargo build` fails with
  ``linking with `link.exe` failed ... link: extra operand`` because Git
  Bash's `/usr/bin/link.exe` (GNU coreutils) shadows the absent MSVC
  linker. That error is a red herring; ignore it, and don't chase it.

Finally, the git history offers no advantage on either machine: the Windows
clone is a full, clean clone of `origin/main` with all 13 commits present.
Nothing about this task lives only on one box.
