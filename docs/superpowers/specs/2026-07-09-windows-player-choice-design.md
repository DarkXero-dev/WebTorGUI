# Windows First-Launch Player Choice Design

## Goal

`player_windows.rs` embeds mpv into the app window via `--wid=<hwnd>`, a
CLI contract only mpv (and mpv-based forks) actually support - most other
players (VLC's bare exe, MPC-HC, PotPlayer, Windows Media Player) have no
equivalent. Rather than pretending any player can be embedded, give Windows
users an explicit, one-time choice on first launch: stick with the bundled
mpv (embedded, in-app), or point to a different player of their own
choosing, which plays externally (its own separate window) instead. Linux
is unaffected - it already always embeds via mpv/X11, no choice needed
there.

## Settings

`src/settings.rs`'s `AppSettings` gets a new field:

```rust
#[serde(default)]
pub windows_player_choice: Option<WindowsPlayerChoice>,
```

```rust
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WindowsPlayerChoice {
    BundledMpv,
    External(String), // absolute path to the chosen player .exe
}
```

`None` is the "never asked" state that triggers the first-launch popup;
`Some(_)` (either variant) means it's been resolved and the popup never
shows again. This field exists in the struct on every platform (simplest -
no cfg on the settings struct itself), but is only ever read or written on
Windows; `#[serde(default)]` keeps existing Linux settings.json files
loading unchanged (they'll just carry a `None` they never look at).

## First-launch popup

New method on `WebtorApp`, Windows-only, called once per frame like the
existing tray-notice/remove-confirm popups:

```rust
#[cfg(target_os = "windows")]
fn render_player_choice_popup(&mut self, ctx: &egui::Context) {
    if self.settings.lock().unwrap().windows_player_choice.is_some() {
        return;
    }
    // ... egui::Window, same visual pattern as render_tray_notice:
    // - explanatory text: mpv is bundled and used for in-app embedded
    //   playback; using a different player means it opens externally,
    //   not embedded in this window.
    // - "Use Bundled mpv" button (primary/pink) -> saves
    //   `Some(WindowsPlayerChoice::BundledMpv)`, closes popup.
    // - "Use a Different Player" button (secondary) -> opens
    //   `rfd::FileDialog::new().add_filter("Executable", &["exe"]).pick_file()`.
    //   - Some(path) -> saves `Some(External(path))`, closes.
    //   - None (cancelled) -> saves `Some(BundledMpv)` anyway, so the
    //     popup doesn't reappear every launch just because they backed
    //     out of the file picker once.
}
```

Called from the same place `render_tray_notice` is called (`eframe::App::
ui`), guarded `#[cfg(target_os = "windows")]` so it's simply absent from
the Linux binary.

## Stream page behavior change

`play_embedded_at` (the handler behind the "Play Embedded" button) branches
on the setting, Windows only:

- `BundledMpv` (or non-Windows): today's behavior, unchanged -
  `EmbeddedPlayer::spawn(...)`.
- `External(path)`: instead of embedding, spawn the chosen executable
  directly against the source (`std::process::Command::new(path).arg
  (&target).spawn()`), set `self.now_playing` the same way `play_external`
  already does, and surface a clear status message (e.g. "Playing
  externally via <player name> - opened in its own window") rather than
  showing the embedded video area's "Starting mpv..." placeholder, so it's
  obvious to the user nothing is supposed to appear inside the app.

The existing "Open Externally" button (OS default file association via
`open::that`) is untouched - this is a separate, explicit "use MY chosen
player" path, not a replacement for it.

## Out of scope

- Any validation that the chosen external player can actually open the
  given source (same trust level as "Open Externally" already has - if it
  fails, the OS/process spawn error surfaces the same way).
- Re-exposing this choice from Settings page for changing later (only a
  first-launch prompt for now - out of scope unless requested later).
- Any change to Linux behavior whatsoever.

## Testing / verification limits

Same limits as the rest of the Windows player work: no Windows machine
available here. `rfd`'s file dialog and `std::process::Command::spawn` are
both already used elsewhere in this codebase (`play_external`, `.torrent`
file upload) so this isn't new unproven surface, but the actual popup
flow and external-spawn-instead-of-embed branch can only be truly verified
by a real Windows run.
