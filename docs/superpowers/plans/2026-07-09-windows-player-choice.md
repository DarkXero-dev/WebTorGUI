# Windows First-Launch Player Choice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On Windows only, ask the user once (first launch) whether to use
the bundled mpv (embedded, in-app playback) or a different player of their
own choosing (which plays externally, in its own window, since only mpv
supports the `--wid` embedding contract `player_windows.rs` relies on).
Linux is untouched.

**Architecture:** A new `Option<WindowsPlayerChoice>` settings field (`None`
= never asked, triggers the popup) drives a Windows-only first-launch popup
and a branch in the "Play Embedded" button's handler: bundled-mpv choice
keeps today's `EmbeddedPlayer::spawn` behavior; external choice spawns the
chosen `.exe` directly against the source instead of embedding, and the Now
Playing card shows a distinct status line so it's clear nothing is supposed
to appear inside the app window.

**Tech Stack:** Rust, egui/eframe, serde (settings persistence), rfd (file
picker, already used elsewhere in this codebase for the same synchronous
`pick_file()` pattern).

## Global Constraints

- Everything in this plan is Windows-only behavior (`#[cfg(target_os =
  "windows")]`), except the settings field itself, which exists
  unconditionally (simplest - no cfg on the settings struct) but is only
  ever read/written on Windows.
- `#[serde(default)]` on the new settings field - existing `settings.json`
  files (Linux or pre-this-feature Windows) must keep loading unchanged.
- No changes to Linux behavior, the existing "Open Externally" button, or
  `player.rs` (X11 implementation) at all.
- Match this codebase's existing rfd usage pattern exactly: `rfd::
  FileDialog::new().add_filter(...).pick_file()` called synchronously
  inline in the handler (see `src/ui/app.rs:1973` for the existing
  `.torrent` file picker using this exact shape).
- Verification in this environment is Linux-build-and-test only (no
  Windows machine available) - every task's test step is `cargo build
  --bin webtorapp` (must stay clean on Linux, proving the new Windows-only
  code doesn't break the Linux build via cfg misuse) plus `cargo test`
  where the task adds/touches a real unit test.

---

### Task 1: Settings - `WindowsPlayerChoice` enum and field

**Files:**
- Modify: `src/settings.rs`

**Interfaces:**
- Produces: `pub enum WindowsPlayerChoice { BundledMpv, External(String) }`
  and `AppSettings.windows_player_choice: Option<WindowsPlayerChoice>`,
  consumed by Task 2 (popup) and Task 3 (playback branch).

- [ ] **Step 1: Add the enum next to `CloseAction`**

In `src/settings.rs`, right after the existing `CloseAction` enum (find it
via `pub enum CloseAction {`), add:

```rust
/// Which player backend to use for in-app video playback on Windows -
/// mpv is the only player `player_windows.rs` can actually embed (via
/// `--wid`), so anyone who'd rather use a different player gets it
/// launched externally (its own window) instead of embedded. `None` on
/// `AppSettings.windows_player_choice` means "never asked yet" and
/// triggers the first-launch popup; irrelevant on Linux, which always
/// embeds via mpv/X11 with no choice involved.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WindowsPlayerChoice {
    BundledMpv,
    External(String),
}
```

- [ ] **Step 2: Add the field to `AppSettings`**

In the `AppSettings` struct (find `pub remembered_close_action:
Option<CloseAction>,`), add directly after it:

```rust
    /// `None` until the Windows first-launch popup resolves it (or
    /// forever, on Linux, which never shows that popup and never reads
    /// this field).
    #[serde(default)]
    pub windows_player_choice: Option<WindowsPlayerChoice>,
```

- [ ] **Step 3: Update `Default for AppSettings`**

In the `Default` impl's `Self { ... }` literal (find `remembered_close_action:
None,`), add directly after it:

```rust
            windows_player_choice: None,
```

- [ ] **Step 4: Update the existing round-trip test**

In `src/settings.rs`'s `mod tests`, find `round_trips_through_json`'s
`AppSettings { ... }` literal (has `remembered_close_action: None,` in it)
and add directly after it:

```rust
            windows_player_choice: None,
```

- [ ] **Step 5: Add a dedicated round-trip test for the new enum**

Add this test to `src/settings.rs`'s `mod tests`, alongside the existing
tests:

```rust
    #[test]
    fn windows_player_choice_round_trips_through_json() {
        let mut settings = AppSettings::default();
        settings.windows_player_choice = Some(WindowsPlayerChoice::External("C:\\Players\\vlc.exe".to_string()));

        let json = serde_json::to_string(&settings).unwrap();
        let restored: AppSettings = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.windows_player_choice, Some(WindowsPlayerChoice::External("C:\\Players\\vlc.exe".to_string())));
    }
```

- [ ] **Step 6: Build and test**

Run: `cargo build --bin webtorapp && cargo test`
Expected: clean build, all tests pass including the two new/updated ones
(`round_trips_through_json`, `windows_player_choice_round_trips_through_json`).

- [ ] **Step 7: Commit**

Leave uncommitted - this repo's owner runs all commits/pushes themselves
via `tagp.sh`. Just confirm the working tree has these changes staged
mentally for the next task (no `git add`/`git commit` here).

---

### Task 2: First-launch player-choice popup (Windows only)

**Files:**
- Modify: `src/ui/app.rs`

**Interfaces:**
- Consumes: `crate::settings::WindowsPlayerChoice` (Task 1), `self.settings:
  Arc<Mutex<AppSettings>>` (already exists), `save_settings` (already
  imported in this file via `use crate::settings::{..., save_settings,
  ...};`).
- Produces: `fn render_player_choice_popup(&mut self, ctx: &egui::Context)`,
  called once per frame from `eframe::App::ui`, gated `#[cfg(target_os =
  "windows")]` end to end (method definition and call site both).

- [ ] **Step 1: Add the popup method**

In `src/ui/app.rs`, add this method near `render_tray_notice` (same `impl
WebtorApp` block - find `fn render_tray_notice(&mut self, ctx:
&egui::Context) {` and add this right before or after it):

```rust
    /// One-time popup, Windows only: mpv (bundled) is the only player
    /// `player_windows.rs` can actually embed - offer a way out for anyone
    /// who'd rather use a different player, understanding it'll open
    /// externally (its own window) rather than embedded in this one.
    /// Never shows again once `windows_player_choice` is `Some(_)`.
    #[cfg(target_os = "windows")]
    fn render_player_choice_popup(&mut self, ctx: &egui::Context) {
        if self.settings.lock().unwrap().windows_player_choice.is_some() {
            return;
        }
        let content_w = 420.0_f32;
        let mut resolved: Option<settings::WindowsPlayerChoice> = None;
        egui::Window::new("Webtor Desktop")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(super::theme::PANEL)
                    .stroke(Stroke::new(1.0, super::theme::BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(24)),
            )
            .show(ctx, |ui| {
                ui.set_width(content_w);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(egui_phosphor::regular::PLAY_CIRCLE).size(32.0).color(super::theme::PINK));
                    ui.add_space(10.0);
                    ui.label(RichText::new("Video Playback").size(15.0).strong().color(super::theme::TEXT));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Webtor Desktop uses mpv (bundled with this install) for in-app embedded video playback. \
                             Prefer a different player? It'll open as its own separate window instead of embedded here.",
                        )
                        .size(13.0)
                        .color(super::theme::MUTED),
                    );
                });
                ui.add_space(16.0);
                ui.columns(2, |cols| {
                    // ui.columns forces a left-aligned layout that Button
                    // inherits for its own text - recenter explicitly so
                    // labels aren't pinned left of a full-width button.
                    cols[0].with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Use Bundled mpv").color(Color32::from_rgb(20, 8, 14)))
                                    .fill(super::theme::PINK)
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                            )
                            .clicked()
                        {
                            resolved = Some(settings::WindowsPlayerChoice::BundledMpv);
                        }
                    });
                    cols[1].with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Use a Different Player").color(super::theme::TEXT))
                                    .fill(Color32::from_gray(45))
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                            )
                            .clicked()
                        {
                            // Cancelling the picker falls back to bundled
                            // mpv rather than leaving this unresolved -
                            // otherwise the popup would nag every launch
                            // just because they backed out once.
                            resolved = Some(
                                rfd::FileDialog::new()
                                    .add_filter("Executable", &["exe"])
                                    .pick_file()
                                    .map(|p| settings::WindowsPlayerChoice::External(p.to_string_lossy().to_string()))
                                    .unwrap_or(settings::WindowsPlayerChoice::BundledMpv),
                            );
                        }
                    });
                });
            });

        if let Some(choice) = resolved {
            let mut settings = self.settings.lock().unwrap();
            settings.windows_player_choice = Some(choice);
            let _ = save_settings(&settings);
        }
    }
```

- [ ] **Step 2: Wire it into the per-frame render loop**

Find `fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {` in
the `impl eframe::App for WebtorApp` block. It currently starts:

```rust
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render_tray_notice(&ui.ctx().clone());
        self.drain_download_events();
```

Change to:

```rust
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render_tray_notice(&ui.ctx().clone());
        #[cfg(target_os = "windows")]
        self.render_player_choice_popup(&ui.ctx().clone());
        self.drain_download_events();
```

- [ ] **Step 3: Build and test**

Run: `cargo build --bin webtorapp && cargo test`
Expected: clean build on Linux (the new method and its call site are both
`#[cfg(target_os = "windows")]`, so neither compiles here at all - this
step is proving the cfg gating doesn't break the Linux build, not
exercising the popup itself). All existing tests still pass.

- [ ] **Step 4: Commit**

Leave uncommitted, same as Task 1.

---

### Task 3: External-player playback branch and status text

**Files:**
- Modify: `src/ui/app.rs`

**Interfaces:**
- Consumes: `settings::WindowsPlayerChoice` (Task 1), existing
  `self.now_playing: Option<String>`, `self.playing_embedded: bool`,
  `self.player_error: Option<String>` fields.
- Produces: new `WebtorApp` field `now_playing_external_player:
  Option<String>` (the chosen player's path, `None` when playing via mpv
  embedded or the OS-default "Open Externally" path) - read by the Now
  Playing status text, written by the new branch in `play_embedded_at`.

- [ ] **Step 1: Add the new field**

In the `WebtorApp` struct, find `now_playing: Option<String>,` and add
directly after it:

```rust
    /// Set only when playback went through a user-chosen external player
    /// (Windows, see `settings::WindowsPlayerChoice::External`) rather
    /// than mpv-embedded or the OS-default "Open Externally" handoff -
    /// distinguishes the three cases for the Now Playing status text.
    now_playing_external_player: Option<String>,
```

Find `now_playing: None,` in the constructor's `Self { ... }` literal and
add directly after it:

```rust
            now_playing_external_player: None,
```

- [ ] **Step 2: Reset the new field in the two existing playback paths**

In `play_external` (find `fn play_external(&mut self) {`), it currently
ends:

```rust
        match open::that(&target) {
            Ok(()) => self.now_playing = Some(target),
            Err(e) => self.player_error = Some(format!("Could not open player: {e}")),
        }
    }
```

Change the `Ok` arm to also clear the new field:

```rust
        match open::that(&target) {
            Ok(()) => {
                self.now_playing_external_player = None;
                self.now_playing = Some(target);
            }
            Err(e) => self.player_error = Some(format!("Could not open player: {e}")),
        }
    }
```

In `play_embedded_at` (find `fn play_embedded_at(&mut self, x: i32, y:
i32, w: u32, h: u32) {`), it currently ends:

```rust
        match EmbeddedPlayer::spawn(parent_handle, x, y, w, h, &target) {
            Ok(player) => {
                self.embedded = Some(player);
                self.playing_embedded = true;
                self.now_playing = Some(target);
            }
            Err(e) => self.player_error = Some(format!("Could not start embedded playback: {e}")),
        }
    }
```

Change the `Ok` arm the same way:

```rust
        match EmbeddedPlayer::spawn(parent_handle, x, y, w, h, &target) {
            Ok(player) => {
                self.embedded = Some(player);
                self.playing_embedded = true;
                self.now_playing_external_player = None;
                self.now_playing = Some(target);
            }
            Err(e) => self.player_error = Some(format!("Could not start embedded playback: {e}")),
        }
    }
```

- [ ] **Step 3: Branch `play_embedded_at` on the Windows player choice**

Still in `play_embedded_at`, the full current body is:

```rust
    fn play_embedded_at(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.player_error = None;
        let target = self.stream_input.trim().to_string();
        if target.is_empty() {
            self.player_error = Some("Enter a URL or file path first.".to_string());
            return;
        }
        let Some(parent_handle) = self.own_window_handle else {
            self.player_error = Some("Embedded playback needs a window handle this backend isn't exposing.".to_string());
            return;
        };
        match EmbeddedPlayer::spawn(parent_handle, x, y, w, h, &target) {
            Ok(player) => {
                self.embedded = Some(player);
                self.playing_embedded = true;
                self.now_playing_external_player = None;
                self.now_playing = Some(target);
            }
            Err(e) => self.player_error = Some(format!("Could not start embedded playback: {e}")),
        }
    }
```

Replace the whole method with (adds a Windows-only early branch before the
existing embedding logic runs):

```rust
    fn play_embedded_at(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.player_error = None;
        let target = self.stream_input.trim().to_string();
        if target.is_empty() {
            self.player_error = Some("Enter a URL or file path first.".to_string());
            return;
        }

        #[cfg(target_os = "windows")]
        {
            let choice = self.settings.lock().unwrap().windows_player_choice.clone();
            if let Some(settings::WindowsPlayerChoice::External(player_path)) = choice {
                match std::process::Command::new(&player_path).arg(&target).spawn() {
                    Ok(_) => {
                        self.playing_embedded = false;
                        self.now_playing_external_player = Some(player_path);
                        self.now_playing = Some(target);
                    }
                    Err(e) => self.player_error = Some(format!("Could not start {player_path}: {e}")),
                }
                return;
            }
        }

        let Some(parent_handle) = self.own_window_handle else {
            self.player_error = Some("Embedded playback needs a window handle this backend isn't exposing.".to_string());
            return;
        };
        match EmbeddedPlayer::spawn(parent_handle, x, y, w, h, &target) {
            Ok(player) => {
                self.embedded = Some(player);
                self.playing_embedded = true;
                self.now_playing_external_player = None;
                self.now_playing = Some(target);
            }
            Err(e) => self.player_error = Some(format!("Could not start embedded playback: {e}")),
        }
    }
```

- [ ] **Step 4: Update the Now Playing status text for the third case**

Find this block in `streaming_page` (in the `full_card(ui, |ui| { ...
"NOW PLAYING" ... })` near the end of the function):

```rust
                Some(target) => {
                    ui.label(RichText::new(target).size(14.0).color(super::theme::TEXT));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(if self.playing_embedded {
                            "Playing embedded via mpv."
                        } else {
                            "Handed off to your system's default player."
                        })
                        .size(12.0)
                        .color(super::theme::MUTED),
                    );
                }
```

Replace with:

```rust
                Some(target) => {
                    ui.label(RichText::new(target).size(14.0).color(super::theme::TEXT));
                    ui.add_space(4.0);
                    let status = match (&self.now_playing_external_player, self.playing_embedded) {
                        (Some(player_path), _) => format!(
                            "Playing externally via {} - opened in its own window, not embedded.",
                            std::path::Path::new(player_path).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| player_path.clone())
                        ),
                        (None, true) => "Playing embedded via mpv.".to_string(),
                        (None, false) => "Handed off to your system's default player.".to_string(),
                    };
                    ui.label(RichText::new(status).size(12.0).color(super::theme::MUTED));
                }
```

- [ ] **Step 5: Build and test**

Run: `cargo build --bin webtorapp && cargo test`
Expected: clean build, all tests still pass. The `#[cfg(target_os =
"windows")]` block inside `play_embedded_at` doesn't compile at all on
Linux, so this proves the cfg gating is correct without being able to
exercise the branch itself here.

- [ ] **Step 6: Commit**

Leave uncommitted, same as Tasks 1-2.

---

## Final Note

Every task's verification is Linux-build-only, same limitation as the
rest of the Windows embedded-player work this session - the popup
appearing correctly, the file picker working, and the external process
actually launching can only be confirmed by a real Windows run (CI build
or the repo owner trying the installer). Flag this plainly when reporting
completion rather than claiming it's "verified."
