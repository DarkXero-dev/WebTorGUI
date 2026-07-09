# Webtor Desktop Client - Scaffold & Linux Packaging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up a buildable, themed egui/eframe skeleton for the Webtor desktop client and produce working Arch (PKGBUILD), Fedora (rpm), and Debian (deb) packaging for it.

**Architecture:** Single Rust crate (bin `webtorapp` + `pub mod ui`), immediate-mode GUI via egui/eframe, ported project layout/conventions from `~/xwork/RDTool` (sibling app) but as a clean-sheet crate (no leftover `src-tauri` naming - that was dead cruft in RDTool from an abandoned Tauri prototype). This plan covers only the scaffold + settings/db foundation + Linux packaging. Auth, the webtor.io API client, torrent/streaming pages, and the download engine are separate follow-on plans (see Out of Scope).

**Tech Stack:** Rust 2021, egui/eframe 0.34, rusqlite (SQLite), serde/serde_json, anyhow, dirs, rfd (native folder picker), egui-phosphor (icon font), image crate.

## Global Constraints

- Package/binary name: `webtorapp`. No `src-tauri`-style legacy directory nesting - source lives directly under `src/`.
- Icon must be an **original** design using webtor's color palette (bg `#0f172a`, pink `#e84393`, cyan `#00cec9`) - do not copy webtor.io's actual favicon/logo artwork (trademark/impersonation risk for a redistributable package).
- Dark theme only for this plan (webtor.io itself only ships a "night" theme in its CSS - no light-mode toggle yet; can be added later without breaking this design).
- Linux packaging only this phase: PKGBUILD (Arch), `.rpm` via `cargo-generate-rpm` (Fedora), `.deb` via `cargo-packager` (Debian). Windows/macOS installers are a later plan.
- Never run `git commit`/`git push` during execution - stage nothing; hand the user exact git commands to run themselves at each natural checkpoint (per standing user instruction).
- Placeholder repo URL `https://github.com/techxero/WebTorApp` is used in PKGBUILD/Cargo.toml homepage fields since this directory has no git remote yet - flag to the user to correct once the real repo exists.
- No network/backend code in this plan (no reqwest, no tokio) - those arrive with the auth/API plan.

## Out of Scope (future plans)

1. **Auth + webtor.io API client plan** - session-cookie login, AES-GCM encrypted credential storage, `WebtorApi` trait + mock, real endpoint wiring once network calls are captured from a live premium session.
2. **Download engine plan** - port RDTool's `downloads/{queue,engine,scheduler}.rs`, wire to real download queue UI.
3. **Torrent/streaming UI plan** - Add Torrent page, file list, stream hand-off to external player, Login screen.
4. **Cross-platform packaging plan** - Windows `.msi`/`.exe`, macOS `.dmg`.

---

### Task 1: Cargo scaffold + minimal running window

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/lib.rs`
- Create: `src/ui/mod.rs`
- Create: `src/ui/app.rs`

**Interfaces:**
- Produces: `webtorapp_lib::run()` (called by `main.rs`); `ui::app::WebtorApp` struct implementing `eframe::App`, constructed via `WebtorApp::new(cc: &eframe::CreationContext) -> Self`.

- [ ] **Step 1: Write `Cargo.toml`**

```toml
[package]
name = "webtorapp"
version = "0.1.0"
description = "Webtor Desktop - native premium client for webtor.io"
authors = ["techxero"]
license = "MIT"
edition = "2021"

[[bin]]
name = "webtorapp"
path = "src/main.rs"

[lib]
name = "webtorapp_lib"
crate-type = ["rlib"]

[dependencies]
eframe = { version = "0.34", default-features = false, features = ["glow", "default_fonts", "accesskit", "x11", "wayland"] }
egui = "0.34"
egui_extras = { version = "0.34", features = ["image", "http"] }
egui-phosphor = "0.12"
image = { version = "0.25", default-features = false, features = ["png"] }
rfd = "0.15"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
dirs = "5"
rusqlite = { version = "0.31" }
chrono = { version = "0.4", features = ["serde"] }

[target.'cfg(not(target_os = "linux"))'.dependencies]
rusqlite = { version = "0.31", features = ["bundled"] }

[package.metadata.packager]
name = "WebtorApp"
identifier = "com.techxero.webtorapp"
description = "Native premium desktop client for webtor.io"
homepage = "https://github.com/techxero/WebTorApp"
icons = ["icons/32x32.png", "icons/128x128.png", "icons/128x128@2x.png"]
out-dir = "target/release/bundle"

[package.metadata.packager.deb]
desktop-template = "packaging/webtorapp.desktop"

[package.metadata.generate-rpm]
assets = [
    { source = "target/release/webtorapp", dest = "/usr/bin/webtorapp", mode = "0755" },
    { source = "icons/icon.png", dest = "/usr/share/pixmaps/webtorapp.png", mode = "0644" },
    { source = "icons/128x128.png", dest = "/usr/share/icons/hicolor/128x128/apps/webtorapp.png", mode = "0644" },
    { source = "packaging/webtorapp.desktop", dest = "/usr/share/applications/webtorapp.desktop", mode = "0644" },
]

[package.metadata.generate-rpm.requires]
openssl-libs = "*"
```

Note: the `[package.metadata.packager*]` and `[package.metadata.generate-rpm*]` blocks belong here (not deferred to Task 7) because `cargo` parses the whole manifest up front - writing them now means Task 7 only adds the two packaging files that don't exist yet (`packaging/webtorapp.desktop`, `PKGBUILD`).

- [ ] **Step 2: Write `src/main.rs`**

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    webtorapp_lib::run();
}
```

- [ ] **Step 3: Write `src/lib.rs`**

Only `ui` is wired in for now - `db` and `settings` modules arrive in Tasks 4-5 and get added to this file then.

```rust
pub mod ui;

pub fn run() {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 750.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Webtor Desktop"),
        ..Default::default()
    };

    eframe::run_native(
        "Webtor Desktop",
        native_options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);

            Ok(Box::new(ui::app::WebtorApp::new(cc)))
        }),
    )
    .expect("eframe run");
}
```

- [ ] **Step 4: Write `src/ui/mod.rs`**

```rust
pub mod app;
```

- [ ] **Step 5: Write `src/ui/app.rs`**

Note: eframe 0.34.3's `App` trait requires `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)`, not the older `update(ctx, frame)` (confirmed against the installed crate source at `~/.cargo/registry/src/*/eframe-0.34.3/src/epi.rs` - `update` still exists but is `#[deprecated]` with a no-op default body and is never invoked by the runner). Panels use `.show_inside(ui, ...)` instead of `.show(ctx, ...)`.

```rust
pub struct WebtorApp {}

impl WebtorApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {}
    }
}

impl eframe::App for WebtorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.label("Webtor Desktop");
        });
    }
}
```

- [ ] **Step 6: Verify it builds**

Run: `cargo build`
Expected: `Compiling webtorapp v0.1.0 ...` then `Finished` with no errors.

- [ ] **Step 7: Verify the window actually opens**

Run: `timeout 8 cargo run || true`
Expected: exit code 124 (timeout killed it while still running = the window launched and stayed open) rather than an immediate panic/exit. This machine has a live Wayland/X11 session (`DISPLAY=:0`), so the window will actually appear on screen for ~8s - visually confirm the title bar reads "Webtor Desktop" and the label is visible, then let the timeout close it (don't leave it running in the background).

---

### Task 2: Original app icon

**Files:**
- Create: `assets/icon.svg`
- Create: `icons/icon.png` (512x512, source raster)
- Create: `icons/128x128.png`
- Create: `icons/128x128@2x.png` (256x256)
- Create: `icons/32x32.png`

**Interfaces:**
- Produces: icon files consumed by Task 1's `[package.metadata.packager]`/`generate-rpm` config and Task 7's `.desktop`/PKGBUILD `install` steps.

- [ ] **Step 1: Write the original icon SVG**

A generic magnet-link glyph (public-domain symbol, not webtor's specific mark) in webtor's palette: dark navy rounded-square background, pink horseshoe body, cyan pole tips.

```xml
<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512">
  <rect width="512" height="512" rx="112" fill="#0f172a"/>
  <path d="M 180 140 L 180 270 A 76 76 0 0 0 332 270 L 332 140"
        fill="none" stroke="#e84393" stroke-width="32" stroke-linecap="round"/>
  <rect x="160" y="256" width="40" height="28" rx="8" fill="#00cec9"/>
  <rect x="312" y="256" width="40" height="28" rx="8" fill="#00cec9"/>
</svg>
```

- [ ] **Step 2: Rasterize to the required sizes**

Run:
```bash
mkdir -p icons
rsvg-convert -w 512 -h 512 assets/icon.svg -o icons/icon.png
rsvg-convert -w 128 -h 128 assets/icon.svg -o icons/128x128.png
rsvg-convert -w 256 -h 256 assets/icon.svg -o icons/128x128@2x.png
rsvg-convert -w 32 -h 32 assets/icon.svg -o icons/32x32.png
```

Expected: four PNG files created under `icons/`.

- [ ] **Step 3: Verify the files are valid PNGs at the right sizes**

Run: `file icons/*.png`
Expected: each line reports `PNG image data` with the matching dimensions (`512 x 512`, `128 x 128`, `256 x 256`, `32 x 32`).

---

### Task 3: Theme module (webtor palette)

**Files:**
- Create: `src/ui/theme.rs`
- Modify: `src/ui/mod.rs` (add `pub mod theme;`)
- Modify: `src/ui/app.rs` (call `theme::apply` once at startup, use `theme::card_frame()` for the label)

**Interfaces:**
- Produces: `theme::{PINK, CYAN, ERROR, WARNING, BG, PANEL, CARD, CARD_HOVER, BORDER, TEXT, MUTED}` constants; `theme::apply(ctx: &egui::Context)`; `theme::card_frame() -> egui::Frame`.

- [ ] **Step 1: Write `src/ui/theme.rs`**

Ported from RDTool's `src-tauri/src/ui/theme.rs`, swapping the single green accent for webtor's pink/cyan pair and its `night`-theme background:

```rust
use egui::{Color32, CornerRadius, Context, Stroke, Visuals};

pub const PINK: Color32 = Color32::from_rgb(0xe8, 0x43, 0x93);
pub const CYAN: Color32 = Color32::from_rgb(0x00, 0xce, 0xc9);
pub const ERROR: Color32 = Color32::from_rgb(239, 68, 68);
pub const WARNING: Color32 = Color32::from_rgb(234, 179, 8);

pub const BG: Color32 = Color32::from_rgb(0x0f, 0x17, 0x2a);
pub const PANEL: Color32 = Color32::from_rgb(0x17, 0x1f, 0x2e);
pub const CARD: Color32 = Color32::from_rgba_premultiplied(0x1c, 0x25, 0x36, 230);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x22, 0x2c, 0x40);
pub const BORDER: Color32 = Color32::from_rgba_premultiplied(25, 25, 25, 25);
pub const TEXT: Color32 = Color32::from_rgb(0xf1, 0xf5, 0xf9);
pub const MUTED: Color32 = Color32::from_rgb(0x94, 0xa3, 0xb8);

pub fn apply(ctx: &Context) {
    let mut v = Visuals::dark();

    v.hyperlink_color = CYAN;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(0xe8, 0x43, 0x93, 55);
    v.selection.stroke = Stroke::new(1.0, PINK);

    v.widgets.active.bg_fill = Color32::from_rgba_unmultiplied(0x80, 0x20, 0x50, 200);
    v.widgets.active.bg_stroke = Stroke::new(1.0, PINK);
    v.widgets.active.fg_stroke = Stroke::new(2.0, PINK);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN);

    v.panel_fill = PANEL;
    v.window_fill = BG;
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);

    ctx.set_visuals(v);
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
}
```

- [ ] **Step 2: Wire it into `src/ui/mod.rs`**

```rust
pub mod app;
pub mod theme;
```

- [ ] **Step 3: Apply the theme and use the card frame in `src/ui/app.rs`**

Note: eframe 0.34.3's `App` trait requires `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)`, not the older `update(ctx, frame)` (confirmed against the installed crate source - `update` still exists but is `#[deprecated]` with a no-op default body and is never called by the runner). Nested panels use `.show_inside(ui, ...)` instead of `.show(ctx, ...)`.

```rust
pub struct WebtorApp {}

impl WebtorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        super::theme::apply(&cc.egui_ctx);
        Self {}
    }
}

impl eframe::App for WebtorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            super::theme::card_frame().show(ui, |ui| {
                ui.colored_label(super::theme::TEXT, "Webtor Desktop");
            });
        });
    }
}
```

- [ ] **Step 4: Verify it builds and renders themed**

Run: `cargo build`
Expected: `Finished` with no errors.

Run: `timeout 8 cargo run || true`
Expected: window opens with a dark navy background and the card frame visible around the label; visually confirm the pink/cyan theme is applied (no default egui gray).

---

### Task 4: Settings module

**Files:**
- Create: `src/settings.rs`
- Modify: `src/lib.rs` (add `pub mod settings;`)

**Interfaces:**
- Produces: `settings::AppSettings { download_dir: String }` (Default, Serialize, Deserialize, Clone, Debug); `settings::load_settings() -> AppSettings`; `settings::save_settings(&AppSettings) -> anyhow::Result<()>`.

- [ ] **Step 1: Write the failing test in `src/settings.rs`**

```rust
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AppSettings {
    pub download_dir: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        let download_dir = dirs::download_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string();
        Self { download_dir }
    }
}

fn settings_path() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    let dir = base.join("webtorapp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("settings.json"))
}

pub fn load_settings() -> AppSettings {
    settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &AppSettings) -> Result<()> {
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(settings_path()?, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let original = AppSettings {
            download_dir: "/home/test/Downloads".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(original.download_dir, restored.download_dir);
    }
}
```

- [ ] **Step 2: Run it to make sure it passes (this is pure-function logic, no I/O to fake - the round-trip test is the meaningful unit here)**

Run: `cargo test round_trips_through_json`
Expected: `test settings::tests::round_trips_through_json ... ok`

- [ ] **Step 3: Wire into `src/lib.rs`**

```rust
pub mod settings;
pub mod ui;
```

- [ ] **Step 4: Verify full build still succeeds**

Run: `cargo build && cargo test`
Expected: `Finished` and all tests pass (1 passed so far).

---

### Task 5: DB module

**Files:**
- Create: `src/db.rs`
- Modify: `src/lib.rs` (add `pub mod db;`)

**Interfaces:**
- Produces: `db::init_db(&rusqlite::Connection) -> anyhow::Result<()>`; `db::open() -> anyhow::Result<rusqlite::Connection>`; `db::db_path() -> anyhow::Result<PathBuf>`.
- Schema matches RDTool's `downloads` table exactly (needed unchanged by the download-engine plan that ports RDTool's `downloads/queue.rs`).

- [ ] **Step 1: Write the failing test in `src/db.rs`**

```rust
use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

pub fn db_path() -> Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no data dir"))?;
    let dir = base.join("webtorapp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("downloads.db"))
}

pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS downloads (
            id          TEXT PRIMARY KEY,
            url         TEXT NOT NULL,
            filename    TEXT NOT NULL,
            dest_path   TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'queued',
            priority    INTEGER NOT NULL DEFAULT 0,
            threads     INTEGER NOT NULL DEFAULT 4,
            scheduled_at TEXT,
            total_bytes  INTEGER,
            bytes_done   INTEGER NOT NULL DEFAULT 0,
            error_msg   TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );",
    )?;
    Ok(())
}

pub fn open() -> Result<Connection> {
    let path = db_path()?;
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    init_db(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_downloads_table_with_expected_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(downloads);").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for expected in [
            "id", "url", "filename", "dest_path", "status", "priority", "threads",
            "scheduled_at", "total_bytes", "bytes_done", "error_msg", "created_at", "updated_at",
        ] {
            assert!(columns.contains(&expected.to_string()), "missing column {expected}");
        }
    }
}
```

- [ ] **Step 2: Run it to make sure it passes**

Run: `cargo test creates_downloads_table_with_expected_columns`
Expected: `test db::tests::creates_downloads_table_with_expected_columns ... ok`

- [ ] **Step 3: Wire into `src/lib.rs`**

```rust
pub mod db;
pub mod settings;
pub mod ui;
```

- [ ] **Step 4: Verify full build and test suite**

Run: `cargo build && cargo test`
Expected: `Finished`, 2 tests passed.

---

### Task 6: Minimal sidebar shell (Dashboard + Settings pages)

**Files:**
- Modify: `src/ui/app.rs` (replace the single-label body with a sidebar + page router)
- Modify: `src/lib.rs` (pass loaded `AppSettings` into `WebtorApp::new`)

**Interfaces:**
- Consumes: `settings::{AppSettings, load_settings, save_settings}` (Task 4), `theme::{apply, card_frame, PINK, CYAN, TEXT, MUTED, PANEL}` (Task 3).
- Produces: `ui::app::Page` enum `{ Dashboard, Settings }` used by later plans' page router.

- [ ] **Step 1: Update `src/lib.rs` to load settings before building the app**

```rust
pub mod db;
pub mod settings;
pub mod ui;

pub fn run() {
    let app_settings = settings::load_settings();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 750.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Webtor Desktop"),
        ..Default::default()
    };

    eframe::run_native(
        "Webtor Desktop",
        native_options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);

            Ok(Box::new(ui::app::WebtorApp::new(cc, app_settings)))
        }),
    )
    .expect("eframe run");
}
```

- [ ] **Step 2: Rewrite `src/ui/app.rs` with a sidebar + two pages**

Note: eframe 0.34.3's `App` trait requires `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)`, not `update(ctx, frame)` - panels use `.show_inside(ui, ...)`.

```rust
use crate::settings::{save_settings, AppSettings};
use egui::{RichText, Ui};

#[derive(PartialEq, Clone, Copy)]
pub enum Page {
    Dashboard,
    Settings,
}

pub struct WebtorApp {
    page: Page,
    settings: AppSettings,
}

impl WebtorApp {
    pub fn new(cc: &eframe::CreationContext<'_>, settings: AppSettings) -> Self {
        super::theme::apply(&cc.egui_ctx);
        Self {
            page: Page::Dashboard,
            settings,
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let tiles: &[(Page, &str, &str)] = &[
            (Page::Dashboard, egui_phosphor::regular::HOUSE, "Dashboard"),
            (Page::Settings, egui_phosphor::regular::GEAR, "Settings"),
        ];
        for (page, icon, label) in tiles {
            let active = self.page == *page;
            let text_color = if active { super::theme::PINK } else { super::theme::MUTED };
            let resp = ui.add(
                egui::Button::new(RichText::new(format!("{icon}  {label}")).color(text_color))
                    .fill(if active { super::theme::CARD_HOVER } else { super::theme::PANEL })
                    .min_size(egui::vec2(92.0, 56.0)),
            );
            if resp.clicked() {
                self.page = *page;
            }
        }
    }

    fn dashboard_page(&mut self, ui: &mut Ui) {
        super::theme::card_frame().show(ui, |ui| {
            ui.colored_label(super::theme::TEXT, RichText::new("Webtor Desktop").size(20.0).strong());
            ui.add_space(6.0);
            ui.colored_label(super::theme::MUTED, "Not signed in - login coming soon.");
        });
    }

    fn settings_page(&mut self, ui: &mut Ui) {
        super::theme::card_frame().show(ui, |ui| {
            ui.colored_label(super::theme::TEXT, RichText::new("Download Directory").strong());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.settings.download_dir);
                if ui.button("Browse").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.settings.download_dir = dir.to_string_lossy().to_string();
                    }
                }
            });
            ui.add_space(10.0);
            if ui.button("Save").clicked() {
                let _ = save_settings(&self.settings);
            }
        });
    }
}

impl eframe::App for WebtorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::SidePanel::left("sidebar")
            .exact_width(92.0)
            .frame(egui::Frame::new().fill(super::theme::PANEL))
            .show_inside(ui, |ui| {
                self.sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(super::theme::BG).inner_margin(egui::Margin::same(20)))
            .show_inside(ui, |ui| match self.page {
                Page::Dashboard => self.dashboard_page(ui),
                Page::Settings => self.settings_page(ui),
            });
    }
}
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build`
Expected: `Finished` with no errors.

- [ ] **Step 4: Verify it runs and both pages work**

Run: `timeout 10 cargo run || true`
Expected: window opens with a left sidebar (Dashboard/Settings tiles) and dark navy content area. Visually confirm: clicking "Settings" swaps the page and shows the download-directory field with a working "Browse" folder picker; clicking "Save" doesn't error; clicking back to "Dashboard" shows the "Not signed in" card.

---

### Task 7: Linux packaging (PKGBUILD, rpm, deb)

**Files:**
- Create: `packaging/webtorapp.desktop`
- Create: `PKGBUILD`
- Modify: nothing else (`Cargo.toml` packaging metadata was already added in Task 1)

**Interfaces:**
- Consumes: `icons/*.png` (Task 2), the packager/generate-rpm metadata already in `Cargo.toml` (Task 1).

- [ ] **Step 1: Write `packaging/webtorapp.desktop`**

```ini
[Desktop Entry]
Name=Webtor Desktop
Comment=Native premium desktop client for webtor.io
Exec=webtorapp
Icon=webtorapp
Terminal=false
Type=Application
Categories=Network;FileTransfer;
Keywords=torrent;stream;download;webtor;
```

- [ ] **Step 2: Write `PKGBUILD` at the repo root**

```bash
# Maintainer: techxero <steve@techxero.com>
pkgname=webtorapp
pkgver=0.1.0
pkgrel=1
pkgdesc="Native premium desktop client for webtor.io"
arch=('x86_64')
url="https://github.com/techxero/WebTorApp"
license=('MIT')
depends=('openssl' 'sqlite')
makedepends=('rust' 'cargo')

_tag="v${pkgver}"
_srcname="WebTorApp-${pkgver}"

source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/${_tag}.tar.gz")
sha256sums=('SKIP')

prepare() {
    cd "$srcdir/$_srcname"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --target "$CARCH-unknown-linux-gnu"
}

build() {
    cd "$srcdir/$_srcname"
    export RUSTUP_TOOLCHAIN=stable
    export RUSTFLAGS="-C opt-level=2"
    cargo build --release
}

package() {
    cd "$srcdir/$_srcname"

    install -Dm755 target/release/webtorapp \
        "$pkgdir/usr/bin/webtorapp"

    install -Dm644 icons/icon.png \
        "$pkgdir/usr/share/pixmaps/webtorapp.png"

    install -Dm644 icons/128x128.png \
        "$pkgdir/usr/share/icons/hicolor/128x128/apps/webtorapp.png"

    install -Dm644 packaging/webtorapp.desktop \
        "$pkgdir/usr/share/applications/webtorapp.desktop"
}
```

Note: unlike RDTool, this PKGBUILD has no `src-tauri` subdirectory to `cd` into and no GTK/rclone/xdotool runtime deps, since this plan's scaffold doesn't use tray/WebDAV yet - those deps get added back in the packaging update that ships alongside the tray/WebDAV-equivalent features, if any.

- [ ] **Step 3: Validate PKGBUILD syntax locally (no GitHub release/tag exists yet, so a full `makepkg` source fetch isn't possible until the user pushes and tags a release - this step only checks the PKGBUILD itself parses correctly)**

Run: `makepkg --printsrcinfo`
Expected: prints a valid `.SRCINFO`-formatted block (pkgbase, pkgname, pkgver, etc.) with no parse errors. Note that `makepkg -f` proper (which downloads `source=`) will fail until `https://github.com/techxero/WebTorApp` exists with a `v0.1.0` tag - that's expected at this stage, not a bug.

- [ ] **Step 4: Install packaging tools**

Run: `cargo install cargo-generate-rpm cargo-packager --locked`
Expected: both binaries installed successfully to `~/.cargo/bin`.

- [ ] **Step 5: Build the release binary**

Run: `cargo build --release`
Expected: `Finished` in release mode, binary at `target/release/webtorapp`.

- [ ] **Step 6: Generate the Fedora rpm**

Run: `cargo generate-rpm`
Expected: an rpm produced under `target/generate-rpm/webtorapp-0.1.0-1.x86_64.rpm`. Verify with `rpm -qlp target/generate-rpm/webtorapp-0.1.0-1.x86_64.rpm` if `rpm` tooling is present; otherwise confirm the file exists and is non-empty (`ls -la target/generate-rpm/`) - full install verification needs a Fedora machine/container since this dev box is Arch.

- [ ] **Step 7: Generate the Debian deb**

Run: `cargo packager --release --formats deb`
Expected: a `.deb` produced under `target/release/bundle/deb/`. Confirm the file exists and is non-empty (`ls -la target/release/bundle/deb/`) - full install verification needs a Debian/Ubuntu machine/container since `dpkg-deb` isn't installed on this Arch box.

- [ ] **Step 8: Ready to commit**

Do not run git commands yourself. Once all steps above pass, hand the user this to run themselves:

```bash
git init
git add Cargo.toml src/ icons/ assets/ packaging/ PKGBUILD docs/
git commit -m "Scaffold webtorapp: themed egui shell + Linux packaging (PKGBUILD/rpm/deb)"
```

---

## Verification Summary (run after all tasks)

```bash
cargo build --release
cargo test
makepkg --printsrcinfo
cargo generate-rpm
cargo packager --release --formats deb
```

All five commands should succeed with no errors. `timeout 10 cargo run` should show a themed sidebar app with working Dashboard/Settings pages.
