# Windows Embedded Player Design

## Goal

Webtor Desktop currently fails to compile on Windows at all: `src/ui/app.rs`
unconditionally imports `crate::player::EmbeddedPlayer`, but the `player`
module is gated `#[cfg(target_os = "linux")]` in `src/lib.rs`. This adds a
real Windows implementation of embedded video playback (mpv reparented into
our own window), mirroring the existing Linux X11 approach with Win32
equivalents, so the app builds and runs correctly on both platforms with
matching functionality.

## Background: how the Linux implementation works

`src/player.rs`'s `EmbeddedPlayer`:
- Creates a real X11 child window (`create_window`) parented to our own
  app's XID, matching our window's depth/visual.
- Spawns `mpv --wid=<that window> --input-ipc-server=<unix-socket-path>
  <source>`, so mpv renders directly into that child window (not through
  egui's texture pipeline).
- Reads mpv's JSON IPC socket non-blockingly each frame to detect mpv's own
  fullscreen toggle (its OSC button or `f` key), since X11 window managers
  only manage direct children of the root - an EWMH fullscreen request on a
  nested child is ignored.
- On fullscreen: reparents the child window to the X11 root and resizes it
  to the monitor's bounds; reverses this on fullscreen-exit.
- `reposition`: moves/resizes the child window to track the egui-painted
  video area each frame (no-op while native-fullscreened).
- `set_hidden`: unmaps/remaps the window - added recently so egui popups
  (e.g. the tray close-confirmation dialog) can appear on top of the video,
  since X11 always paints a child window over its parent's own rendering in
  that screen region regardless of anything egui does.
- `Drop`: kills the mpv child process, destroys the X11 window, removes the
  socket file.

## Windows implementation

### File layout

- `src/player_windows.rs` (new) - Win32-based `EmbeddedPlayer`, same public
  API shape as `src/player.rs`: `spawn`, `poll_fullscreen_toggle`,
  `is_native_fullscreen`, `enter_native_fullscreen`, `exit_native_fullscreen`,
  `reposition`, `set_hidden`, `is_running`, `Drop`, plus a free function
  replacing `own_window_xid`.
- `src/lib.rs` - `pub mod player` resolves to whichever file backs it per
  platform:
  ```rust
  #[cfg(target_os = "linux")]
  #[path = "player.rs"]
  pub mod player;
  #[cfg(target_os = "windows")]
  #[path = "player_windows.rs"]
  pub mod player;
  ```
  `src/ui/app.rs` keeps its single unconditional `use crate::player::
  EmbeddedPlayer;` and all existing call sites unchanged.

### Shared type fix (both platforms)

An X11 XID is `u32`; a Win32 `HWND` is pointer-sized. To let `app.rs` hold
one field type across platforms:
- `crate::player::own_window_xid` is renamed `own_window_handle` in both
  `player.rs` and `player_windows.rs`, returning `Option<isize>` on both
  (the X11 XID cast to `isize`; the raw HWND value on Windows).
- `EmbeddedPlayer::spawn`'s first parameter becomes `parent_handle: isize`
  on both platforms (X11 `create_window`'s `parent` param takes `parent_handle
  as u32`; Win32's `CreateWindowExW` takes it as `HWND(parent_handle as
  *mut _)`).
- `WebtorApp.own_xid: Option<u32>` becomes `own_window_handle: Option<isize>`
  in `app.rs`; the one call site constructing it and the one call site
  passing it to `EmbeddedPlayer::spawn` are updated accordingly.

### Win32 mechanics

- **Dependency**: `windows` crate (windows-rs), Windows-only
  (`[target.'cfg(target_os = "windows")'.dependencies]`), with features:
  `Win32_Foundation`, `Win32_UI_WindowsAndMessaging`, `Win32_Graphics_Gdi`
  (for monitor bounds).
- **Child window creation**: register a minimal window class once
  (`RegisterClassExW`, `DefWindowProcW` as the window proc - we don't need
  custom message handling, mpv owns all rendering/input inside it), then
  `CreateWindowExW` with `WS_CHILD | WS_VISIBLE`, parent = our own HWND from
  `parent_handle`.
- **Spawn mpv**: `mpv.exe --wid=<hwnd-as-decimal> --input-ipc-server=<pipe
  path> <source>`. `mpv.exe` is located next to our own executable
  (`std::env::current_exe()?.parent()?.join("mpv.exe")`) - see Packaging
  below for how it gets there. Same `--force-window=yes --keep-open=yes
  --panscan=1.0 --script-opts=osc-scalewindowed=1.6,osc-scalefullscreen=1.4`
  flags as Linux for behavior parity.
- **IPC (fullscreen detection)**: mpv's JSON IPC uses a named pipe on
  Windows (`--input-ipc-server=\\.\pipe\webtorapp-mpv-<uuid>`), not a Unix
  socket. Named pipe client handles don't support the same non-blocking
  `read()` semantics we rely on for the Linux socket, so instead: spawn a
  background `std::thread` that opens the pipe (retrying briefly, mpv needs
  a moment to create it) and does blocking line-reads, parsing each JSON
  line and forwarding `property-change`/`fullscreen` events through an
  `mpsc::channel`. `poll_fullscreen_toggle` drains that channel
  non-blockingly with `try_recv` - the same "background thread/task +
  mpsc + non-blocking drain" shape already used for `torrent_add_rx` in
  `ui/app.rs`, not a new pattern for this codebase.
- **Fullscreen enter**: `SetParent(hwnd, None)` (desktop), then
  `MonitorFromWindow` + `GetMonitorInfoW` for the monitor's `rcMonitor`
  bounds, then `SetWindowPos` to that rect with `HWND_TOP` (stacks above its
  siblings without pinning it permanently above every other app's window the
  way `HWND_TOPMOST` would - matches Linux's plain `StackMode::ABOVE`).
- **Fullscreen exit**: `SetParent(hwnd, Some(parent_handle))`, `SetWindowPos`
  back to the tracked app-relative geometry.
- **`reposition`**: `SetWindowPos` with the new x/y/w/h, no-op while
  native-fullscreened (same guard as Linux).
- **`set_hidden`**: `ShowWindow(hwnd, SW_HIDE)` / `ShowWindow(hwnd,
  SW_SHOW)`.
- **`is_running`**: same `Child::try_wait()` check as Linux (the mpv
  `std::process::Child` handle is platform-agnostic).
- **`Drop`**: kill+wait the mpv child, `DestroyWindow(hwnd)`. Named pipes
  don't leave a filesystem entry behind the way Unix sockets do, so no
  cleanup-file step is needed (unlike Linux's `remove_file(&ipc_path)`).

### Packaging / CI

- `.github/workflows/build.yml`'s `windows` job gets a new step, before
  `cargo packager`, that downloads mpv's official Windows build (a released
  archive from mpv.io's Windows builds) and extracts `mpv.exe` to a known
  path the packager config picks up as a bundled resource, so every CI
  build produces an installer with `mpv.exe` shipped alongside
  `webtorapp.exe` - no manual step, no binary committed to the repo.
- `Cargo.toml`'s `[package.metadata.packager]` gets a resource entry so
  `cargo-packager`'s NSIS output includes that `mpv.exe` next to the
  installed binary (same directory `current_exe()?.parent()` resolves to at
  runtime).

## Error handling

Same shape as Linux: `EmbeddedPlayer::spawn` returns `Result`, and
`play_embedded_at` in `app.rs` already surfaces the `Err` as
`self.player_error` - no changes needed there. Win32 API calls that can fail
(`CreateWindowExW`, `RegisterClassExW`) return errors via the `windows`
crate's `Result`, propagated with `anyhow`, matching the `anyhow!(...)`
style already used in `player.rs`.

## Testing / verification limits

This is being built and iterated on a Linux machine with no Windows box or
CI access available interactively. Verification here means: cross-compiling
for `x86_64-pc-windows-gnu` (via `rustup target add` + mingw-w64, since that
target cross-compiles from Linux without needing MSVC's linker) to prove the
code compiles and links cleanly, including the `windows` crate usage. It
does **not** prove the HWND embedding, fullscreen reparenting, or
named-pipe IPC actually behave correctly on a real Windows desktop - that
can only be confirmed by an actual Windows run (CI build + manual test, or
the user trying the installer), which is out of scope for this
implementation pass and should be treated as a known gap until someone runs
it for real.

## Out of scope

- Any change to the Linux implementation's behavior (only the
  `own_window_xid` → `own_window_handle` rename and `spawn`'s first
  parameter type change, both mechanical).
- A unified/shared rendering approach (e.g. mpv's libmpv render API drawn
  directly into an egui texture) that would sidestep child-window quirks on
  both platforms entirely - a much larger rewrite, not attempted here.
- macOS support (`player` module has no macOS variant; out of scope, no
  change).
