use crate::downloads::engine::DownloadEvent;
use crate::downloads::{queue, scheduler};
use crate::player::EmbeddedPlayer;
use crate::settings;
use crate::settings::{detect_file_category, resolve_dest_dir, save_settings, AddonSource, AppSettings, FileCategory};
use crate::torrent;
use crate::webtor_auth::{self, WebtorAuth};
use egui::{Align2, Color32, CornerRadius, FontId, Margin, RichText, Stroke, Ui};
use rusqlite::Connection;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

#[derive(PartialEq, Clone, Copy)]
pub enum Page {
    Dashboard,
    Discover,
    Streaming,
    Downloads,
    AddOns,
    Settings,
}

/// A verified-playable open-content clip, offered as a quick-fill on the
/// Stream page (not on Discover - Discover mirrors webtor.io's real catalog,
/// whose titles we can't actually resolve to playable URLs ourselves).
struct SampleClip {
    name: &'static str,
    url: &'static str,
    size_label: &'static str,
}

fn sample_clips() -> &'static [SampleClip] {
    // Every URL here was verified (both `curl` 200 and an actual `mpv` decode/playback
    // test) - the original googleapis.com "commondatastorage" bucket used in an earlier
    // pass now returns 403 Forbidden for all of these titles and was replaced.
    &[
        SampleClip {
            name: "Big Buck Bunny",
            url: "https://media.w3.org/2010/05/bunny/movie.mp4",
            size_label: "238 MB",
        },
        SampleClip {
            name: "Sintel (trailer)",
            url: "https://media.w3.org/2010/05/sintel/trailer.mp4",
            size_label: "4.2 MB",
        },
        SampleClip {
            name: "Tears of Steel",
            url: "https://media.xiph.org/tearsofsteel/tears_of_steel_1080p.webm",
            size_label: "545 MB",
        },
        SampleClip {
            name: "Elephants Dream",
            url: "https://download.blender.org/ED/ED_1024.avi",
            size_label: "425 MB",
        },
    ]
}

/// One entry from a Discover catalog - same Stremio addon protocol
/// webtor.io itself uses for its Discover page (Cinemeta by default, or any
/// other installed addon).
#[derive(Clone)]
struct DiscoverEntry {
    id: String,
    name: String,
    poster: String,
    year: String,
    imdb_rating: String,
}

#[derive(PartialEq, Clone, Copy)]
enum DiscoverType {
    Movie,
    Series,
}

impl DiscoverType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
        }
    }
}

/// Cinemeta (and most Stremio addons) return this many results per catalog
/// page (confirmed empirically) - used as a heuristic for "is there a next
/// page" since the protocol has no total count.
const DISCOVER_PAGE_SIZE: usize = 50;

/// One catalog declared by an addon's manifest.json, e.g. Cinemeta's
/// "Popular" movies catalog with its real declared genre list.
#[derive(Clone)]
struct ManifestCatalog {
    kind: String, // "movie" or "series"
    id: String,
    label: String,
    genres: Vec<String>,
    supports_search: bool,
}

/// A validated addon's manifest - fetched live from `{base_url}/manifest.json`,
/// never hardcoded, so any Stremio-compatible addon works, not just Cinemeta.
/// Addons declare different `resources`: catalog addons (like Cinemeta) list
/// "catalog" and provide browsable metadata; stream addons (like Torrentio)
/// list "stream" and resolve real torrent/stream sources for a title instead -
/// `catalogs` is empty for those, which is normal, not an error.
#[derive(Clone)]
struct AddonManifest {
    name: String,
    resources: Vec<String>,
    catalogs: Vec<ManifestCatalog>,
}

async fn fetch_addon_manifest(base_url: &str) -> Result<AddonManifest, String> {
    let url = format!("{}/manifest.json", base_url.trim_end_matches('/'));
    let resp = reqwest::get(&url).await.map_err(|e| format!("network error: {e}"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("bad response: {e}"))?;
    let name = json
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "manifest missing 'name'".to_string())?
        .to_string();
    // `resources` entries are either plain strings ("stream") or objects
    // ({"name": "stream", ...}) depending on the addon - handle both.
    let resources = json
        .get("resources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r.as_str().map(str::to_string).or_else(|| r.get("name").and_then(|n| n.as_str()).map(str::to_string)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let catalogs = json
        .get("catalogs")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let kind = c.get("type")?.as_str()?.to_string();
                    if kind != "movie" && kind != "series" {
                        return None;
                    }
                    let id = c.get("id")?.as_str()?.to_string();
                    let label = c.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
                    let genres = c
                        .get("extra")
                        .and_then(|v| v.as_array())
                        .and_then(|extras| extras.iter().find(|e| e.get("name").and_then(|n| n.as_str()) == Some("genre")))
                        .and_then(|g| g.get("options"))
                        .and_then(|o| o.as_array())
                        .map(|arr| arr.iter().filter_map(|g| g.as_str().map(str::to_string)).collect::<Vec<_>>())
                        .unwrap_or_default();
                    // Addons declare search support two different ways
                    // depending on manifest schema version - a newer
                    // `extra: [{"name": "search", ...}]` entry, or the
                    // older flat `extraSupported: ["search", ...]` array.
                    let supports_search = c
                        .get("extra")
                        .and_then(|v| v.as_array())
                        .is_some_and(|extras| extras.iter().any(|e| e.get("name").and_then(|n| n.as_str()) == Some("search")))
                        || c.get("extraSupported")
                            .and_then(|v| v.as_array())
                            .is_some_and(|extras| extras.iter().any(|e| e.as_str() == Some("search")));
                    Some(ManifestCatalog { kind, id, label, genres, supports_search })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(AddonManifest { name, resources, catalogs })
}

/// One entry from Stremio's real, official addon catalog - the same endpoint
/// the official Stremio app uses for its built-in "Community Addons" browser.
#[derive(Clone)]
struct StoreAddon {
    transport_url: String,
    name: String,
    description: String,
    logo: String,
    resources: Vec<String>,
}

const ADDON_STORE_URL: &str = "https://api.strem.io/addonscollection.json";

async fn fetch_addon_store() -> Result<Vec<StoreAddon>, String> {
    let resp = reqwest::get(ADDON_STORE_URL).await.map_err(|e| format!("network error: {e}"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("bad response: {e}"))?;
    let arr = json.as_array().ok_or_else(|| "unexpected response shape".to_string())?;
    let addons = arr
        .iter()
        .filter_map(|entry| {
            let transport_url = entry.get("transportUrl")?.as_str()?.to_string();
            let manifest = entry.get("manifest")?;
            let name = manifest.get("name")?.as_str()?.to_string();
            let description = manifest.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let logo = manifest.get("logo").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let resources = manifest
                .get("resources")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| r.as_str().map(str::to_string).or_else(|| r.get("name").and_then(|n| n.as_str()).map(str::to_string)))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(StoreAddon { transport_url, name, description, logo, resources })
        })
        .collect();
    Ok(addons)
}

fn build_catalog_url(base_url: &str, kind: &str, catalog_id: &str, genre: Option<&str>, skip: usize, search: Option<&str>) -> String {
    let mut extras = Vec::new();
    if let Some(q) = search {
        extras.push(format!("search={}", percent_encoding::utf8_percent_encode(q, percent_encoding::NON_ALPHANUMERIC)));
    }
    if let Some(g) = genre {
        extras.push(format!("genre={g}"));
    }
    if skip > 0 {
        extras.push(format!("skip={skip}"));
    }
    let base = format!("{}/catalog/{kind}/{catalog_id}", base_url.trim_end_matches('/'));
    if extras.is_empty() {
        format!("{base}.json")
    } else {
        format!("{base}/{}.json", extras.join("&"))
    }
}

async fn fetch_discover_catalog_entries(url: &str) -> Result<Vec<DiscoverEntry>, String> {
    let resp = reqwest::get(url).await.map_err(|e| format!("network error: {e}"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("bad response: {e}"))?;
    let metas = json
        .get("metas")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "unexpected response shape".to_string())?;
    let entries = metas
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let name = m.get("name")?.as_str()?.to_string();
            let poster = m.get("poster")?.as_str()?.to_string();
            let year = m.get("year").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let imdb_rating = m.get("imdbRating").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Some(DiscoverEntry { id, name, poster, year, imdb_rating })
        })
        .collect::<Vec<_>>();
    Ok(entries)
}

async fn fetch_discover_catalog(tx: std::sync::mpsc::Sender<Result<Vec<DiscoverEntry>, String>>, url: String) {
    let _ = tx.send(fetch_discover_catalog_entries(&url).await);
}

/// Searches every installed Discover addon at once (not just the currently
/// selected one) - for each, fetches its manifest fresh, picks the first
/// catalog of `kind` that declares search support, and queries it. Results
/// are merged and deduped by id; addons with no search-capable catalog for
/// this `kind` are silently skipped (not every addon supports search).
async fn search_all_discover_addons(addons: Vec<AddonSource>, kind: String, query: String) -> Result<Vec<DiscoverEntry>, String> {
    let mut results = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut any_searchable = false;
    let mut last_err = String::new();
    for addon in addons {
        let manifest = match fetch_addon_manifest(&addon.base_url).await {
            Ok(m) => m,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        let Some(catalog) = manifest.catalogs.iter().find(|c| c.kind == kind && c.supports_search) else {
            continue;
        };
        any_searchable = true;
        let url = build_catalog_url(&addon.base_url, &kind, &catalog.id, None, 0, Some(&query));
        match fetch_discover_catalog_entries(&url).await {
            Ok(entries) => {
                for entry in entries {
                    if seen_ids.insert(entry.id.clone()) {
                        results.push(entry);
                    }
                }
            }
            Err(e) => last_err = e,
        }
    }
    if !any_searchable {
        return Err("None of your installed sources support search.".to_string());
    }
    if results.is_empty() && !last_err.is_empty() {
        return Err(last_err);
    }
    Ok(results)
}

/// One real torrent/stream option resolved by a stream addon (e.g. Torrentio)
/// for a specific title - `magnet` is built from the addon's `infoHash`,
/// which is all these addons return (no tracker list), so common public
/// trackers are appended to give the swarm a real chance to be found.
struct StreamResult {
    filename: String,
    quality: String,
    seeders: String,
    size: String,
    uploader: String,
    /// Falls back to this raw line when the addon doesn't use Torrentio's
    /// "👤 seeders 💾 size ⚙️ uploader" convention, so nothing is silently lost.
    raw_meta: String,
    magnet: String,
}

const PUBLIC_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.tracker.cl:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://exodus.desync.com:6969/announce",
];

fn percent_encode_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => c.encode_utf8(&mut [0; 4]).bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

/// Parses Torrentio-convention markers ("👤 16", "💾 43.69 GB", "⚙️
/// ilCorSaRoNeRo", also seen as "👥 215 seeders") out of the *whole* stream
/// title/name text, not just one fixed line - addons vary in how many lines
/// they use and in what order. Returns empty strings for any part that isn't
/// present.
fn parse_stream_meta(text: &str) -> (String, String, String) {
    let after_marker = |marker: char| text.split(marker).nth(1).map(|s| s.trim_start_matches('\u{fe0f}'));
    let take_until_next_marker = |s: &str| {
        s.split(['👤', '👥', '💾', '⚙', '🔗', '\n'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let seeders = after_marker('👤').or_else(|| after_marker('👥')).map(take_until_next_marker).unwrap_or_default();
    let size = after_marker('💾').map(take_until_next_marker).unwrap_or_default();
    let uploader = after_marker('⚙').map(take_until_next_marker).unwrap_or_default();
    (seeders, size, uploader)
}

async fn fetch_streams(base_url: &str, kind: &str, id: &str) -> Result<Vec<StreamResult>, String> {
    let url = format!("{}/stream/{kind}/{id}.json", base_url.trim_end_matches('/'));
    let resp = reqwest::get(&url).await.map_err(|e| format!("network error: {e}"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("bad response: {e}"))?;
    let streams = json.get("streams").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let results = streams
        .iter()
        .filter_map(|s| {
            let info_hash = s.get("infoHash")?.as_str()?.to_string();
            let title_field = s
                .get("title")
                .and_then(|v| v.as_str())
                .or_else(|| s.get("name").and_then(|v| v.as_str()))
                .unwrap_or("Unknown source");
            let filename = title_field.split('\n').next().unwrap_or("Unknown source").trim().to_string();
            let rest = title_field.splitn(2, '\n').nth(1).unwrap_or("");
            let (seeders, mut size, uploader) = parse_stream_meta(rest);
            // A structured byte count (part of the Stremio addon spec) beats
            // whatever an addon's title text happens to say, and several
            // real installed addons only populate this, not the emoji text.
            if let Some(bytes) = s.get("behaviorHints").and_then(|b| b.get("videoSize")).and_then(|v| v.as_u64()) {
                size = format_bytes(bytes);
            }
            let raw_meta = if seeders.is_empty() && size.is_empty() && uploader.is_empty() {
                rest.trim().to_string()
            } else {
                String::new()
            };
            let quality = s
                .get("name")
                .and_then(|v| v.as_str())
                .and_then(|n| n.split('\n').nth(1))
                .unwrap_or("")
                .trim()
                .to_string();

            let behavior_filename = s.get("behaviorHints").and_then(|b| b.get("filename")).and_then(|v| v.as_str());
            let dn = behavior_filename.or(Some(filename.as_str())).map(|f| format!("&dn={}", percent_encode_filename(f))).unwrap_or_default();
            let trackers: String = PUBLIC_TRACKERS.iter().map(|t| format!("&tr={t}")).collect();
            let magnet = format!("magnet:?xt=urn:btih:{info_hash}{dn}{trackers}");
            Some(StreamResult { filename, quality, seeders, size, uploader, raw_meta, magnet })
        })
        .collect();
    Ok(results)
}

/// A real, actively-managed torrent (librqbit) - not a stub. `handle` is
/// queried live each frame for metadata (file list) and progress, so there's
/// no separate "resolved: bool"/cached file list to drift out of sync.
struct AddedTorrent {
    id: usize,
    handle: Arc<librqbit::ManagedTorrent>,
    title: String,
    source_label: &'static str,
    output_dir: std::path::PathBuf,
    /// Which file indices are selected to actually download. `None` means
    /// "everything" (the default, matching librqbit's own default).
    selected_files: Option<std::collections::HashSet<usize>>,
    /// File indices already moved into their category folder (see
    /// `settings::resolve_dest_dir`) - moved once, not re-checked every frame.
    routed_files: std::collections::HashSet<usize>,
}

/// `AddedTorrent::source_label` is `&'static str` (it's always one of a
/// handful of literals set at add time) - restoring from the DB gives us an
/// owned `String` we can't turn into one without leaking, so map it back to
/// the matching literal instead.
fn known_source_label(s: &str) -> &'static str {
    match s {
        "Magnet link" => "Magnet link",
        "Stream source" => "Stream source",
        "Uploaded .torrent" => "Uploaded .torrent",
        _ => "Restored",
    }
}

/// A long torrent title in an `egui::Window`'s title bar stretches the
/// whole window wider than its body's explicit width - the title bar isn't
/// bound by `set_max_width`/`set_width` the way content is.
fn truncate_title(title: &str, max_chars: usize) -> String {
    if title.chars().count() > max_chars {
        format!("{}...", title.chars().take(max_chars).collect::<String>())
    } else {
        title.to_string()
    }
}

/// A real webtor.io add-on/integration - things that plug webtor into other
/// software, sourced from webtor.io's own homepage. Deliberately excludes
/// core in-product features (Direct Download Links, Instant Streaming, ZIP
/// Archive, Cloud Library, AI Discover) that aren't add-ons in any sense.
struct FeatureCard {
    icon: &'static str,
    title: &'static str,
    desc: &'static str,
    badges: &'static [&'static str],
    link: Option<&'static str>,
    link_label: &'static str,
}

fn webtor_features() -> Vec<FeatureCard> {
    vec![
        FeatureCard {
            icon: egui_phosphor::regular::PUZZLE_PIECE,
            title: "Chrome Extension",
            desc: "Install the extension and every torrent or magnet link you click opens automatically in Webtor.",
            badges: &[],
            link: Some("https://chromewebstore.google.com/detail/webtorio-watch-torrents-o/ngkpdaefpmokglfnmienfiaioffjodam"),
            link_label: "Open in Chrome Web Store",
        },
        FeatureCard {
            icon: egui_phosphor::regular::TELEVISION,
            title: "Stremio Integration",
            desc: "Watch your library on any smart TV or device running Stremio via the addon link from your webtor.io profile.",
            badges: &[],
            link: None,
            link_label: "",
        },
        FeatureCard {
            icon: egui_phosphor::regular::HARD_DRIVES,
            title: "WebDAV Support",
            desc: "Access your library as a network folder via WebDAV. Works with Mountain Duck, RaiDrive, Owlfiles.",
            badges: &[],
            link: None,
            link_label: "",
        },
        FeatureCard {
            icon: egui_phosphor::regular::CODE,
            title: "Developer SDK",
            desc: "Embed the torrent streaming player on your own site with the open-source JavaScript SDK.",
            badges: &["MIT License"],
            link: Some("https://github.com/webtor-io/embed-sdk-js"),
            link_label: "View on GitHub",
        },
    ]
}

fn magnet_display_name(magnet: &str) -> String {
    magnet
        .split('&')
        .find_map(|part| part.strip_prefix("dn="))
        .map(urlencoding_decode)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown torrent".to_string())
}

fn urlencoding_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned()
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.2} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn format_speed(bytes_per_sec: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec))
}

/// Moves a completed torrent file into its category folder
/// (`settings::resolve_dest_dir`) in the background. Tries a plain rename
/// first (instant, same filesystem); falls back to a streamed copy through a
/// small fixed buffer - never the whole file - so this doesn't spike RAM on
/// a multi-gigabyte file, then removes the source.
fn route_completed_file(src: std::path::PathBuf, filename: String, settings: AppSettings) {
    tokio::spawn(async move {
        let dest_dir = resolve_dest_dir(&settings, &filename);
        let dest_path = std::path::Path::new(&dest_dir).join(&filename);
        if let Some(parent) = dest_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if tokio::fs::rename(&src, &dest_path).await.is_ok() {
            return;
        }
        let (Ok(mut src_file), Ok(mut dest_file)) = (tokio::fs::File::open(&src).await, tokio::fs::File::create(&dest_path).await) else {
            return;
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 64 * 1024];
        loop {
            match src_file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if dest_file.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
        let _ = tokio::fs::remove_file(&src).await;
    });
}

/// A card that always claims the full available width, instead of
/// shrink-wrapping to its narrowest child (the cause of the "tiny card
/// floating in a sea of empty space" layout bug).
fn full_card(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    super::theme::card_frame().show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        add_contents(ui);
    });
}

/// A slightly lighter, more compact card for list rows (installed addons,
/// source results) - full_card's padding is tuned for whole-page sections.
fn addon_row_frame(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(super::theme::CARD_HOVER)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add_contents(ui);
        });
}

/// A small colored pill for stream metadata (quality, seeders, size, uploader).
fn stream_badge(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 30))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(10.5).color(color));
        });
}

pub struct WebtorApp {
    page: Page,
    settings: Arc<Mutex<AppSettings>>,
    db_conn: Connection,
    dl_tx: Sender<DownloadEvent>,
    dl_rx: Receiver<DownloadEvent>,

    logged_in: bool,
    login_email: String,
    login_error: Option<String>,
    login_loading: bool,
    webtor_auth: WebtorAuth,
    #[cfg(target_os = "linux")]
    browser_login_rx: Option<Receiver<crate::browser_login::CookieResult>>,

    tray_notice_open: bool,
    never_ask_again_checked: bool,

    settings_saved: bool,

    torrent_engine: Arc<crate::torrent_engine::TorrentEngine>,
    magnet_input: String,
    torrents: Vec<AddedTorrent>,
    torrent_add_rx: Vec<Receiver<Result<(crate::torrent_engine::AddedHandle, String, &'static str), String>>>,
    /// Torrents waiting their turn - a new add starts immediately only if
    /// nothing else is still downloading, otherwise it queues here and
    /// `advance_torrent_queue` starts it once the current one finishes.
    pending_torrent_queue: std::collections::VecDeque<(crate::torrent_engine::AddSource, String, &'static str)>,
    /// Which torrent's file-selection popup is open, keyed by its librqbit id.
    file_picker_torrent_id: Option<usize>,
    /// Which torrent's remove confirmation (keep files vs delete them too)
    /// is open, keyed by its librqbit id.
    remove_confirm_torrent_id: Option<usize>,
    /// Whether a torrent was actively fetching pieces as of the last frame -
    /// used to catch the moment everything finishes, so the progress pill
    /// can flash a completion state instead of just vanishing.
    download_pill_was_active: bool,
    /// Set when everything just finished - shows a green "Download
    /// complete" pill until this instant, then goes back to nothing.
    download_complete_pill_until: Option<std::time::Instant>,
    notice: Option<String>,

    download_url_input: String,
    download_speeds: std::collections::HashMap<String, u64>,

    stream_input: String,
    now_playing: Option<String>,
    /// Set only when playback went through a user-chosen external player
    /// (Windows, see `settings::WindowsPlayerChoice::External`) rather
    /// than mpv-embedded or the OS-default "Open Externally" handoff -
    /// distinguishes the three cases for the Now Playing status text.
    now_playing_external_player: Option<String>,
    playing_embedded: bool,
    player_error: Option<String>,
    own_window_handle: Option<isize>,
    embedded: Option<EmbeddedPlayer>,

    discover_catalog: Vec<DiscoverEntry>,
    discover_rx: Option<Receiver<Result<Vec<DiscoverEntry>, String>>>,
    discover_loading: bool,
    discover_error: Option<String>,
    discover_type: DiscoverType,
    discover_genre: Option<String>,
    discover_skip: usize,

    discover_addon_index: usize,
    discover_catalog_id: String,
    discover_manifest: Option<AddonManifest>,
    discover_manifest_rx: Option<Receiver<Result<AddonManifest, String>>>,
    discover_manifest_loading: bool,
    discover_manifest_error: Option<String>,

    discover_search_input: String,
    /// `None` = normal catalog browsing; `Some(_)` = showing search
    /// results instead (searched across every installed Discover addon,
    /// not just the currently selected one).
    discover_search_results: Option<Vec<DiscoverEntry>>,
    discover_search_rx: Option<Receiver<Result<Vec<DiscoverEntry>, String>>>,
    discover_search_loading: bool,
    discover_search_error: Option<String>,

    new_addon_url: String,
    addon_install_rx: Option<Receiver<Result<(String, AddonManifest), String>>>,
    addon_install_loading: bool,
    addon_install_error: Option<String>,

    new_stream_addon_url: String,
    stream_addon_install_rx: Option<Receiver<Result<(String, AddonManifest), String>>>,
    stream_addon_install_loading: bool,
    stream_addon_install_error: Option<String>,

    source_picker_open: bool,
    source_picker_title: String,
    source_picker_results: Vec<StreamResult>,
    source_picker_rx: Option<Receiver<Result<Vec<StreamResult>, String>>>,
    source_picker_loading: bool,
    source_picker_error: Option<String>,

    sources_popup_open: bool,
    sources_popup_tab: SourcesTab,

    features_popup_open: bool,
    features_popup_tab: SourcesTab,

    addon_store_open: bool,
    addon_store_catalog: Vec<StoreAddon>,
    addon_store_rx: Option<Receiver<Result<Vec<StoreAddon>, String>>>,
    addon_store_loading: bool,
    addon_store_error: Option<String>,
    addon_store_search: String,
}

#[derive(PartialEq, Clone, Copy)]
enum SourcesTab {
    Sources,
    Stremio,
}

impl WebtorApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Arc<Mutex<AppSettings>>,
        db_conn: Connection,
        dl_tx: Sender<DownloadEvent>,
        dl_rx: Receiver<DownloadEvent>,
        torrent_engine: Arc<crate::torrent_engine::TorrentEngine>,
    ) -> Self {
        super::theme::apply(&cc.egui_ctx);

        let own_window_handle = {
            use raw_window_handle::HasWindowHandle;
            cc.window_handle().ok().and_then(|h| crate::player::own_window_handle(h.as_raw()))
        };

        // Auto-login if we have a real, still-valid webtor.io session saved
        // (encrypted, machine-bound cookie jar) - see crate::webtor_auth.
        let webtor_auth = webtor_auth::load_session().unwrap_or_else(|| WebtorAuth::new().expect("build http client"));
        let logged_in = webtor_auth.has_session();
        let login_email = if logged_in {
            webtor_auth.account_label().unwrap_or_else(|| "webtor.io account".to_string())
        } else {
            String::new()
        };

        // librqbit's own session persistence already resumed whatever was
        // downloading last run before we get here - without this, the
        // Downloads page would come back empty every restart (looking like
        // download history got wiped, when the data itself was fine all
        // along), since `self.torrents` otherwise only grows as new adds
        // resolve.
        let restored_torrents: Vec<AddedTorrent> = torrent_engine
            .list_existing(&db_conn)
            .into_iter()
            .map(|added| {
                let info_hash = added.handle.info_hash().as_string();
                let stored = crate::downloads::torrents::get(&db_conn, &info_hash);
                let title = stored
                    .as_ref()
                    .map(|s| s.title.clone())
                    .or_else(|| added.handle.name())
                    .unwrap_or_else(|| "Unknown torrent".to_string());
                let source_label = stored
                    .as_ref()
                    .map(|s| known_source_label(&s.source_label))
                    .unwrap_or("Restored");
                let routed_files = stored.map(|s| s.routed_files.into_iter().collect()).unwrap_or_default();
                AddedTorrent {
                    id: added.id,
                    handle: added.handle,
                    title,
                    source_label,
                    output_dir: added.output_dir,
                    selected_files: None,
                    routed_files,
                }
            })
            .collect();

        let app = Self {
            page: Page::Discover,
            settings,
            db_conn,
            dl_tx,
            dl_rx,
            logged_in,
            login_email,
            login_error: None,
            login_loading: false,
            webtor_auth,
            #[cfg(target_os = "linux")]
            browser_login_rx: None,
            tray_notice_open: false,
            never_ask_again_checked: false,
            settings_saved: false,
            torrent_engine,
            magnet_input: String::new(),
            torrents: restored_torrents,
            torrent_add_rx: Vec::new(),
            pending_torrent_queue: std::collections::VecDeque::new(),
            file_picker_torrent_id: None,
            remove_confirm_torrent_id: None,
            download_pill_was_active: false,
            download_complete_pill_until: None,
            notice: None,
            download_url_input: String::new(),
            download_speeds: std::collections::HashMap::new(),
            stream_input: String::new(),
            now_playing: None,
            now_playing_external_player: None,
            playing_embedded: false,
            player_error: None,
            own_window_handle,
            embedded: None,
            discover_catalog: Vec::new(),
            discover_rx: None,
            discover_loading: false,
            discover_error: None,
            discover_type: DiscoverType::Movie,
            discover_genre: None,
            discover_skip: 0,
            discover_addon_index: 0,
            discover_catalog_id: String::new(),
            discover_manifest: None,
            discover_manifest_rx: None,
            discover_manifest_loading: false,
            discover_manifest_error: None,
            discover_search_input: String::new(),
            discover_search_results: None,
            discover_search_rx: None,
            discover_search_loading: false,
            discover_search_error: None,
            new_addon_url: String::new(),
            addon_install_rx: None,
            addon_install_loading: false,
            addon_install_error: None,
            new_stream_addon_url: String::new(),
            stream_addon_install_rx: None,
            stream_addon_install_loading: false,
            stream_addon_install_error: None,
            source_picker_open: false,
            source_picker_title: String::new(),
            source_picker_results: Vec::new(),
            source_picker_rx: None,
            source_picker_loading: false,
            source_picker_error: None,
            sources_popup_open: false,
            sources_popup_tab: SourcesTab::Sources,
            features_popup_open: false,
            features_popup_tab: SourcesTab::Sources,
            addon_store_open: false,
            addon_store_catalog: Vec::new(),
            addon_store_rx: None,
            addon_store_loading: false,
            addon_store_error: None,
            addon_store_search: String::new(),
        };
        app
    }

    /// Drain download-engine events and persist them to the queue DB. Called
    /// once per frame regardless of which page is showing, so status stays
    /// correct even when the user isn't looking at the Downloads page.
    fn drain_download_events(&mut self) {
        while let Ok(event) = self.dl_rx.try_recv() {
            match event {
                DownloadEvent::Progress(p) => {
                    let _ = queue::update_progress(&self.db_conn, &p.id, p.bytes_done, p.total_bytes);
                    self.download_speeds.insert(p.id, p.speed_bps);
                }
                DownloadEvent::Complete(c) => {
                    let _ = queue::update_progress(&self.db_conn, &c.id, c.bytes_done, Some(c.bytes_done));
                    let _ = queue::update_status(&self.db_conn, &c.id, queue::DownloadStatus::Completed);
                    self.download_speeds.remove(&c.id);
                }
                DownloadEvent::Error(e) => {
                    let _ = queue::set_error(&self.db_conn, &e.id, &e.error);
                    self.download_speeds.remove(&e.id);
                }
            }
        }
    }

    fn enqueue_download(&mut self, url: String, filename: String) {
        let settings = self.settings.lock().unwrap().clone();
        let dest_dir = resolve_dest_dir(&settings, &filename);
        let dest_path = format!("{}/{}", dest_dir.trim_end_matches('/'), filename);
        let opts = queue::DownloadOpts {
            threads: None,
            scheduled_at: None,
            priority: 0,
        };
        if queue::enqueue(&self.db_conn, url, filename, dest_path, opts, settings.threads_per_download).is_ok() {
            scheduler::tick_now(Arc::clone(&self.settings), self.dl_tx.clone());
        }
    }

    // ---------------------------------------------------------------- login

    /// Real login against webtor.io's actual auth backend (SuperTokens
    /// Passwordless) - confirmed from their own frontend bundle that there is
    /// no password field on the real site, only an email code + Google/Patreon
    /// OAuth. OAuth isn't feasible from a desktop app without an embedded
    /// browser (webtor's OAuth redirect URI is fixed to their own web
    /// callback, not a localhost port we could intercept), so this implements
    /// the email-code flow, which a plain HTTP client can do end-to-end.
    fn login_page(&mut self, ui: &mut Ui) {
        #[cfg(target_os = "linux")]
        {
            if let Some(rx) = &self.browser_login_rx {
                if let Ok(result) = rx.try_recv() {
                    match result {
                        Ok(cookies) => match self.webtor_auth.import_cookies(cookies) {
                            Ok(()) if self.webtor_auth.has_session() => {
                                self.logged_in = true;
                                self.login_error = None;
                                self.login_email = self.webtor_auth.account_label().unwrap_or_else(|| "webtor.io account".to_string());
                                let _ = webtor_auth::save_session(&self.webtor_auth);
                            }
                            _ => self.login_error = Some("Browser window closed without a valid session.".to_string()),
                        },
                        Err(e) => self.login_error = Some(e),
                    }
                    self.login_loading = false;
                    self.browser_login_rx = None;
                }
            }
            if self.browser_login_rx.is_some() {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            }
        }

        let content_w = 420.0_f32;
        let form_h = 560.0_f32;
        let footer_h = 40.0_f32;

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(super::theme::BG))
            .show_inside(ui, |ui| {
                let avail = ui.available_rect_before_wrap();
                let panel_w = avail.width();
                let panel_h = avail.height();
                let top_y = avail.min.y + ((panel_h - footer_h - form_h) / 2.0).max(20.0);
                let form_x = avail.min.x + (panel_w - content_w) / 2.0;

                {
                    let p = ui.painter();
                    let ghost = Color32::from_rgba_unmultiplied(255, 255, 255, 14);
                    p.text(
                        egui::pos2(avail.min.x + panel_w * 0.10, avail.min.y + panel_h * 0.18),
                        Align2::CENTER_CENTER,
                        egui_phosphor::regular::MAGNET,
                        FontId::proportional(150.0),
                        ghost,
                    );
                    p.text(
                        egui::pos2(avail.min.x + panel_w * 0.90, avail.min.y + panel_h * 0.24),
                        Align2::CENTER_CENTER,
                        egui_phosphor::regular::CLOUD_ARROW_DOWN,
                        FontId::proportional(170.0),
                        ghost,
                    );
                    p.text(
                        egui::pos2(avail.min.x + panel_w * 0.87, avail.min.y + panel_h * 0.82),
                        Align2::CENTER_CENTER,
                        egui_phosphor::regular::DOWNLOAD_SIMPLE,
                        FontId::proportional(120.0),
                        ghost,
                    );
                }

                let form_rect = egui::Rect::from_min_size(egui::pos2(form_x, top_y), egui::vec2(content_w, form_h));
                ui.scope_builder(egui::UiBuilder::new().max_rect(form_rect), |ui| {
                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        let mut logo = egui::text::LayoutJob::default();
                        logo.append(
                            "web",
                            0.0,
                            egui::TextFormat { font_id: FontId::proportional(72.0), color: super::theme::TEXT, ..Default::default() },
                        );
                        logo.append(
                            "tor",
                            0.0,
                            egui::TextFormat { font_id: FontId::proportional(72.0), color: super::theme::PINK, ..Default::default() },
                        );
                        ui.label(logo);
                        ui.add_space(18.0);
                        ui.label(RichText::new("Stream & download").size(25.0).strong().color(super::theme::TEXT));
                        ui.label(RichText::new("premium torrents, instantly").size(25.0).strong().color(super::theme::PINK));
                        ui.add_space(6.0);
                        ui.label(RichText::new("Sign in with your real webtor.io account").size(13.0).color(super::theme::MUTED));

                        ui.add_space(22.0);
                        ui.separator();
                        ui.add_space(22.0);

                        #[cfg(target_os = "linux")]
                        {
                            ui.label(
                                RichText::new("webtor.io's bot protection blocks a plain login form here - sign in through a real browser window instead.")
                                    .size(12.0)
                                    .color(super::theme::MUTED),
                            );
                            ui.add_space(16.0);

                            let can_click = !self.login_loading;
                            let clicked = if can_click {
                                ui.add(
                                    egui::Button::new(RichText::new("Sign in with Browser").size(16.0).strong().color(Color32::from_rgb(20, 8, 14)))
                                        .fill(super::theme::PINK)
                                        .min_size(egui::vec2(content_w, 48.0)),
                                )
                                .clicked()
                            } else {
                                ui.add(
                                    egui::Button::new(RichText::new("Waiting for browser sign-in...").size(16.0).color(super::theme::MUTED))
                                        .fill(Color32::from_gray(32))
                                        .stroke(Stroke::new(1.0, Color32::from_gray(45)))
                                        .min_size(egui::vec2(content_w, 48.0)),
                                );
                                false
                            };

                            if clicked {
                                self.login_error = None;
                                self.login_loading = true;
                                let (tx, rx) = std::sync::mpsc::channel();
                                crate::browser_login::open_login_window(tx);
                                self.browser_login_rx = Some(rx);
                            }
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            ui.label(
                                RichText::new("Sign-in isn't available on this platform yet - the login window needs a Linux-only windowing feature. Linux builds support it today.")
                                    .size(12.0)
                                    .color(super::theme::MUTED),
                            );
                        }

                        if let Some(err) = self.login_error.clone() {
                            ui.add_space(12.0);
                            egui::Frame::new()
                                .fill(Color32::from_rgb(60, 20, 30))
                                .stroke(Stroke::new(1.0, super::theme::ERROR))
                                .corner_radius(CornerRadius::same(6))
                                .inner_margin(Margin::same(8))
                                .show(ui, |ui| {
                                    ui.set_min_width(content_w - 16.0);
                                    ui.label(RichText::new(err).size(13.0).color(super::theme::ERROR));
                                });
                        }

                        ui.add_space(16.0);
                        ui.label(RichText::new("No free tier - premium webtor.io accounts only.").size(11.0).color(super::theme::MUTED));
                    });
                });
            });
    }

    // -------------------------------------------------------------- sidebar

    fn sidebar(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.add_space(14.0);
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(RichText::new("web").size(15.0).color(super::theme::TEXT));
                ui.label(RichText::new("tor").size(15.0).strong().color(super::theme::PINK));
            });
        });
        ui.add_space(10.0);
        ui.add(egui::Separator::default().spacing(0.0));
        ui.add_space(10.0);

        let nav: &[(&str, &str, Page)] = &[
            (egui_phosphor::regular::COMPASS, "Discover", Page::Discover),
            (egui_phosphor::regular::DOWNLOAD_SIMPLE, "Downloads", Page::Downloads),
            (egui_phosphor::regular::PLAY_CIRCLE, "Stream", Page::Streaming),
            (egui_phosphor::regular::PUZZLE_PIECE, "Add-ons", Page::AddOns),
            (egui_phosphor::regular::USER_CIRCLE, "Account", Page::Dashboard),
            (egui_phosphor::regular::GEAR, "Settings", Page::Settings),
        ];

        for (icon, label, p) in nav {
            let active = self.page == *p;
            let item_size = egui::vec2(80.0, 60.0);
            let (rect, resp) = ui.allocate_exact_size(item_size, egui::Sense::click());

            if resp.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            if ui.is_rect_visible(rect) {
                let bg = if active {
                    super::theme::PINK_DIM
                } else if resp.hovered() {
                    super::theme::CARD
                } else {
                    Color32::TRANSPARENT
                };
                let color = if active {
                    super::theme::PINK
                } else if resp.hovered() {
                    super::theme::TEXT
                } else {
                    super::theme::MUTED
                };

                ui.painter().rect_filled(rect, CornerRadius::same(8), bg);

                if active {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height())),
                        CornerRadius::same(2),
                        super::theme::PINK,
                    );
                }

                ui.painter().text(
                    egui::pos2(rect.center().x, rect.center().y - 8.0),
                    Align2::CENTER_CENTER,
                    icon,
                    FontId::proportional(21.0),
                    color,
                );
                ui.painter().text(
                    egui::pos2(rect.center().x, rect.bottom() - 9.0),
                    Align2::CENTER_CENTER,
                    label,
                    FontId::proportional(10.5),
                    color,
                );
            }

            if resp.clicked() {
                self.page = *p;
            }
            ui.add_space(2.0);
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            ui.add_space(10.0);
            if ui
                .add(
                    egui::Button::new(RichText::new("Sign out").size(11.0).color(super::theme::MUTED))
                        .min_size(egui::vec2(76.0, 26.0)),
                )
                .clicked()
            {
                self.stop_embedded();
                self.logged_in = false;
                let _ = webtor_auth::clear_session();
                self.webtor_auth = WebtorAuth::new().expect("build http client");
                self.login_email.clear();
                self.page = Page::Dashboard;
            }
            let name = if self.login_email.len() > 12 {
                format!("{}…", &self.login_email[..12])
            } else {
                self.login_email.clone()
            };
            ui.label(RichText::new(name).size(11.0).color(super::theme::MUTED));
            ui.add_space(6.0);
        });
    }

    // ----------------------------------------------------------- dashboard

    fn dashboard_page(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Account").size(26.0).strong().color(super::theme::TEXT));
        ui.add_space(4.0);
        ui.label(RichText::new("Account overview").size(14.0).color(super::theme::MUTED));
        ui.add_space(24.0);

        self.render_download_progress_pill(ui);

        ui.columns(2, |cols| {
            super::theme::card_frame().show(&mut cols[0], |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new("ACCOUNT").size(11.0).color(super::theme::MUTED).strong());
                ui.add_space(6.0);
                ui.label(RichText::new(&self.login_email).size(18.0).strong().color(super::theme::TEXT));
            });
            super::theme::card_frame().show(&mut cols[1], |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new("PLAN").size(11.0).color(super::theme::MUTED).strong());
                ui.add_space(6.0);
                match self.webtor_auth.plan_label() {
                    Some(plan) => {
                        ui.label(
                            RichText::new(format!("{}  {plan}", egui_phosphor::regular::CROWN))
                                .size(18.0)
                                .strong()
                                .color(super::theme::PINK),
                        );
                    }
                    None => {
                        ui.label(RichText::new("Signed in").size(18.0).strong().color(super::theme::TEXT));
                    }
                }
            });
        });
        ui.add_space(12.0);

        full_card(ui, |ui| {
            ui.label(RichText::new("SUBSCRIPTION & STORAGE").size(11.0).color(super::theme::MUTED).strong());
            ui.add_space(10.0);
            match self.webtor_auth.plan_label() {
                Some(plan) => {
                    ui.label(RichText::new(format!("Current plan: {plan}")).size(14.0).color(super::theme::TEXT));
                }
                None => {
                    ui.label(RichText::new("Plan details aren't exposed by webtor.io's session data.").size(13.0).color(super::theme::MUTED));
                }
            }
            ui.add_space(6.0);
            match self.webtor_auth.storage_usage() {
                Some((used, total)) => {
                    ui.label(
                        RichText::new(format!("Storage used: {} / {}", format_bytes(used), format_bytes(total)))
                            .size(14.0)
                            .color(super::theme::TEXT),
                    );
                    ui.add_space(6.0);
                    ui.add(egui::ProgressBar::new(used as f32 / total.max(1) as f32).desired_height(14.0));
                }
                None => {
                    ui.label(RichText::new("Storage used: N/A").size(14.0).color(super::theme::MUTED));
                }
            }
        });
        ui.add_space(16.0);

        full_card(ui, |ui| {
            ui.label(RichText::new("STATUS").size(11.0).color(super::theme::MUTED).strong());
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Real BitTorrent engine active - magnets and .torrent files download and stream for real, piece-by-piece, right from this app.",
                )
                .size(13.0)
                .color(super::theme::TEXT),
            );
        });
    }

    // ------------------------------------------------------------- discover

    fn trigger_manifest_fetch(&mut self, base_url: String) {
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            let _ = tx.send(fetch_addon_manifest(&base_url).await);
        });
        self.discover_manifest_rx = Some(rx);
        self.discover_manifest_loading = true;
        self.discover_manifest_error = None;
        self.discover_manifest = None;
    }

    fn trigger_discover_fetch(&mut self, base_url: &str) {
        let url = build_catalog_url(base_url, self.discover_type.as_str(), &self.discover_catalog_id, self.discover_genre.as_deref(), self.discover_skip, None);
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::spawn(fetch_discover_catalog(tx, url));
        self.discover_rx = Some(rx);
        self.discover_loading = true;
        self.discover_error = None;
        self.discover_catalog.clear();
    }

    fn trigger_discover_search(&mut self) {
        let query = self.discover_search_input.trim().to_string();
        if query.is_empty() {
            self.clear_discover_search();
            return;
        }
        let addons = self.settings.lock().unwrap().discover_addons.clone();
        let kind = self.discover_type.as_str().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            let _ = tx.send(search_all_discover_addons(addons, kind, query).await);
        });
        self.discover_search_rx = Some(rx);
        self.discover_search_loading = true;
        self.discover_search_error = None;
        self.discover_search_results = None;
    }

    fn clear_discover_search(&mut self) {
        self.discover_search_input.clear();
        self.discover_search_results = None;
        self.discover_search_rx = None;
        self.discover_search_loading = false;
        self.discover_search_error = None;
    }

    fn discover_page(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Discover").size(26.0).strong().color(super::theme::TEXT));
        ui.add_space(4.0);
        ui.label(
            RichText::new("Same catalog webtor.io's Discover uses - browse whichever addon is installed below")
                .size(14.0)
                .color(super::theme::MUTED),
        );
        ui.add_space(16.0);

        self.render_download_progress_pill(ui);
        if let Some(notice) = self.notice.clone() {
            self.notice_banner(ui, &notice);
        }

        if self.source_picker_open {
            self.render_source_picker(ui);
        }

        let addons = self.settings.lock().unwrap().discover_addons.clone();
        if self.discover_addon_index >= addons.len() {
            self.discover_addon_index = 0;
        }
        let Some(addon) = addons.get(self.discover_addon_index).cloned() else {
            ui.label(RichText::new("No Discover sources installed - add one from the Add-ons page.").size(14.0).color(super::theme::MUTED));
            return;
        };

        if self.discover_manifest.is_none() && self.discover_manifest_rx.is_none() && !self.discover_manifest_loading {
            self.trigger_manifest_fetch(addon.base_url.clone());
        }
        if let Some(rx) = &self.discover_manifest_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(manifest) => {
                        self.discover_catalog_id.clear();
                        self.discover_manifest = Some(manifest);
                    }
                    Err(e) => self.discover_manifest_error = Some(e),
                }
                self.discover_manifest_loading = false;
                self.discover_manifest_rx = None;
            }
        }

        full_card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let mut filters_changed = false;

                if addons.len() > 1 {
                    egui::ComboBox::from_id_salt("discover_addon")
                        .selected_text(addon.name.clone())
                        .show_ui(ui, |ui| {
                            for (i, a) in addons.iter().enumerate() {
                                if ui.selectable_label(i == self.discover_addon_index, &a.name).clicked() && i != self.discover_addon_index {
                                    self.discover_addon_index = i;
                                    self.discover_manifest = None;
                                    self.discover_catalog_id.clear();
                                    self.discover_genre = None;
                                    self.discover_skip = 0;
                                }
                            }
                        });
                    ui.separator();
                }

                if ui.selectable_label(self.discover_type == DiscoverType::Movie, "Movies").clicked()
                    && self.discover_type != DiscoverType::Movie
                {
                    self.discover_type = DiscoverType::Movie;
                    self.discover_genre = None;
                    self.discover_catalog_id.clear();
                    self.clear_discover_search();
                    filters_changed = true;
                }
                if ui.selectable_label(self.discover_type == DiscoverType::Series, "TV Shows").clicked()
                    && self.discover_type != DiscoverType::Series
                {
                    self.discover_type = DiscoverType::Series;
                    self.discover_genre = None;
                    self.discover_catalog_id.clear();
                    self.clear_discover_search();
                    filters_changed = true;
                }

                ui.separator();

                if let Some(manifest) = &self.discover_manifest {
                    let type_str = self.discover_type.as_str();
                    let matching: Vec<&ManifestCatalog> = manifest.catalogs.iter().filter(|c| c.kind == type_str).collect();

                    if !matching.iter().any(|c| c.id == self.discover_catalog_id) {
                        if let Some(first) = matching.first() {
                            self.discover_catalog_id = first.id.clone();
                            self.discover_genre = None;
                            filters_changed = true;
                        }
                    }

                    let current_catalog_label = matching.iter().find(|c| c.id == self.discover_catalog_id).map(|c| c.label.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("discover_catalog_id")
                        .selected_text(current_catalog_label)
                        .show_ui(ui, |ui| {
                            for c in &matching {
                                if ui.selectable_label(c.id == self.discover_catalog_id, &c.label).clicked() && c.id != self.discover_catalog_id {
                                    self.discover_catalog_id = c.id.clone();
                                    self.discover_genre = None;
                                    filters_changed = true;
                                }
                            }
                        });

                    let genres = matching.iter().find(|c| c.id == self.discover_catalog_id).map(|c| c.genres.clone()).unwrap_or_default();
                    if !genres.is_empty() {
                        ui.separator();
                        let current_label = self.discover_genre.clone().unwrap_or_else(|| "All Genres".to_string());
                        egui::ComboBox::from_id_salt("discover_genre")
                            .selected_text(current_label)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.discover_genre.is_none(), "All Genres").clicked() {
                                    self.discover_genre = None;
                                    filters_changed = true;
                                }
                                for g in &genres {
                                    let selected = self.discover_genre.as_deref() == Some(g.as_str());
                                    if ui.selectable_label(selected, g).clicked() {
                                        self.discover_genre = Some(g.clone());
                                        filters_changed = true;
                                    }
                                }
                            });
                    }
                }

                ui.separator();
                if ui.button(format!("{} Manage Sources", egui_phosphor::regular::PUZZLE_PIECE)).clicked() {
                    self.page = Page::AddOns;
                }

                // Fills whatever's left of THIS row (to the right of
                // Manage Sources) - searches every installed Discover
                // addon at once, not just the currently selected one.
                // `available_width()` reports space until the outer
                // container's boundary, not "what's left on the current
                // line before wrapping" - that's `available_size_before_wrap`,
                // which is what's needed here to actually stay on this row
                // instead of overflowing onto a new one.
                ui.separator();
                let showing_search = self.discover_search_results.is_some() || self.discover_search_loading || self.discover_search_error.is_some();
                let icon_btn_w = ui.spacing().button_padding.x * 2.0 + 18.0;
                let reserved = icon_btn_w + ui.spacing().item_spacing.x + if showing_search { icon_btn_w + ui.spacing().item_spacing.x } else { 0.0 };
                let text_w = (ui.available_size_before_wrap().x - reserved).max(80.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.discover_search_input)
                        .desired_width(text_w)
                        .hint_text("Search all installed sources...")
                        .text_color(super::theme::TEXT),
                );
                let submitted = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let search_clicked = ui.add(egui::Button::new(egui_phosphor::regular::MAGNIFYING_GLASS).fill(super::theme::PINK)).clicked();
                if submitted || search_clicked {
                    self.trigger_discover_search();
                }
                if showing_search && ui.button(egui_phosphor::regular::X).clicked() {
                    self.clear_discover_search();
                }

                if filters_changed {
                    self.discover_skip = 0;
                    self.trigger_discover_fetch(&addon.base_url);
                }
            });
        });
        ui.add_space(16.0);

        if let Some(rx) = &self.discover_search_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(entries) => self.discover_search_results = Some(entries),
                    Err(e) => self.discover_search_error = Some(e),
                }
                self.discover_search_loading = false;
                self.discover_search_rx = None;
            }
        }

        if self.discover_search_loading {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.add_space(8.0);
                ui.label(RichText::new("Searching all installed sources...").size(13.0).color(super::theme::MUTED));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }

        if let Some(err) = self.discover_search_error.clone() {
            egui::Frame::new()
                .fill(Color32::from_rgb(60, 20, 30))
                .stroke(Stroke::new(1.0, super::theme::ERROR))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(format!("Search failed: {err}")).size(13.0).color(super::theme::ERROR));
                });
            return;
        }

        if let Some(results) = self.discover_search_results.clone() {
            if results.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.label(RichText::new("No results found.").size(14.0).color(super::theme::MUTED));
                });
            } else {
                self.render_discover_grid(ui, &results);
            }
            return;
        }

        if self.discover_manifest_loading {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.add_space(8.0);
                ui.label(RichText::new(format!("Loading {}'s catalog list...", addon.name)).size(13.0).color(super::theme::MUTED));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }

        if let Some(err) = self.discover_manifest_error.clone() {
            egui::Frame::new()
                .fill(Color32::from_rgb(60, 20, 30))
                .stroke(Stroke::new(1.0, super::theme::ERROR))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(format!("Could not load \"{}\": {err}", addon.name)).size(13.0).color(super::theme::ERROR));
                });
            return;
        }

        if let Some(rx) = &self.discover_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(entries) => self.discover_catalog = entries,
                    Err(e) => self.discover_error = Some(e),
                }
                self.discover_loading = false;
                self.discover_rx = None;
            }
        }

        if self.discover_loading {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.add_space(8.0);
                ui.label(RichText::new(format!("Loading catalog from {}...", addon.name)).size(13.0).color(super::theme::MUTED));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }

        if let Some(err) = self.discover_error.clone() {
            egui::Frame::new()
                .fill(Color32::from_rgb(60, 20, 30))
                .stroke(Stroke::new(1.0, super::theme::ERROR))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(format!("Could not load Discover catalog: {err}")).size(13.0).color(super::theme::ERROR));
                });
            return;
        }

        self.render_discover_grid(ui, &self.discover_catalog.clone());

        ui.add_space(10.0);
        self.discover_pagination_row(ui, &addon.base_url);
    }

    /// The poster grid shared by normal catalog browsing and search results -
    /// takes `entries` directly rather than always reading `self.discover_catalog`
    /// so search results (merged from every installed addon, not paginated)
    /// can reuse the exact same rendering.
    fn render_discover_grid(&mut self, ui: &mut Ui, entries: &[DiscoverEntry]) {
        const COLS: usize = 4;
        let spacing = ui.spacing().item_spacing.x;
        let col_w = (ui.available_width() - spacing * (COLS as f32 - 1.0)) / COLS as f32;
        let poster_w = col_w - 28.0; // minus card_frame's inner margin (14 each side)
        let poster_h = poster_w * 1.5; // standard poster aspect ratio, same box for every card

        let stream_addons = self.settings.lock().unwrap().stream_addons.clone();
        let discover_kind = self.discover_type.as_str().to_string();
        let mut open_picker_for: Option<(String, String, String)> = None; // (title, kind, id)

        egui::ScrollArea::vertical().max_height(ui.available_height() - 60.0).show(ui, |ui| {
            // `ui.columns` tracks each column's height independently, so a
            // single call spanning the whole catalog drifts out of row
            // alignment the moment card heights differ (e.g. a wrapped
            // title) or the item count isn't a multiple of COLS - columns
            // run dry at different points and later rows show blank gaps.
            // Calling it fresh per row resets that alignment every time.
            for row in entries.chunks(COLS) {
                ui.columns(COLS, |cols| {
                    for (i, item) in row.iter().enumerate() {
                        super::theme::card_frame().show(&mut cols[i], |ui| {
                            ui.set_width(poster_w);
                            // An empty URL never loads at all; a present
                            // but broken/unreachable one polls to `Err`
                            // once egui's loader actually tries and fails -
                            // either way that's exactly when the built-in
                            // "broken image" triangle would otherwise show.
                            let poster_broken = item.poster.is_empty()
                                || ui.ctx().try_load_image(&item.poster, egui::SizeHint::default()).is_err();
                            if poster_broken {
                                // Reserve the exact same box a real poster
                                // would take up, so a title with no
                                // thumbnail doesn't collapse the card down
                                // to a tiny, oddly-sized sliver next to its
                                // full-height neighbors.
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(poster_w, poster_h), egui::Sense::hover());
                                ui.painter().rect_filled(rect, CornerRadius::same(8), Color32::from_gray(35));
                                ui.painter().text(rect.center(), Align2::CENTER_CENTER, "N/A", FontId::proportional(18.0), super::theme::MUTED);
                            } else {
                                ui.add(
                                    egui::Image::new(&item.poster)
                                        .fit_to_exact_size(egui::vec2(poster_w, poster_h))
                                        .corner_radius(CornerRadius::same(8))
                                        .show_loading_spinner(true),
                                );
                            }
                            ui.add_space(8.0);
                            // Wrapping to 2 lines for longer titles made
                            // cards in the same row different heights -
                            // `ui.columns` tracks each column's height
                            // independently, so the shorter card's rounded
                            // bottom ended early, leaving plain page
                            // background above the next row that looked
                            // like a missing/square corner. Truncating to
                            // one line keeps every card the same height.
                            ui.add(egui::Label::new(RichText::new(&item.name).size(14.0).strong().color(super::theme::TEXT)).truncate());
                            let meta = match (item.year.is_empty(), item.imdb_rating.is_empty()) {
                                (false, false) => format!("{} - {} {}", item.year, egui_phosphor::regular::STAR, item.imdb_rating),
                                (false, true) => item.year.clone(),
                                _ => String::new(),
                            };
                            // Reserve the meta line's height even when absent, so every card in a row stays the same height.
                            ui.label(RichText::new(if meta.is_empty() { " " } else { &meta }).size(11.0).color(super::theme::MUTED));
                            ui.add_space(6.0);
                            ui.vertical_centered(|ui| {
                                if stream_addons.is_empty() {
                                    ui.add_enabled(
                                        false,
                                        egui::Button::new("Find Sources").min_size(egui::vec2(120.0, 28.0)),
                                    )
                                    .on_disabled_hover_text("Install a Stream Source (e.g. Torrentio) from Add-ons first");
                                } else if ui
                                    .add(
                                        egui::Button::new(RichText::new("Find Sources").color(Color32::from_rgb(20, 8, 14)))
                                            .fill(super::theme::PINK)
                                            .min_size(egui::vec2(120.0, 28.0)),
                                    )
                                    .clicked()
                                {
                                    open_picker_for = Some((item.name.clone(), discover_kind.clone(), item.id.clone()));
                                }
                            });
                        });
                    }
                });
                ui.add_space(10.0);
            }
        });

        if let Some((title, kind, id)) = open_picker_for {
            if let Some(addon) = stream_addons.first() {
                self.open_source_picker(title, kind, id, addon.base_url.clone());
            }
        }
    }

    /// `ui.horizontal` claims the parent's full available width up front
    /// (see egui's `horizontal_with_main_wrap_dyn`), so wrapping it in
    /// `vertical_centered` does not actually center a multi-widget row -
    /// there is no leftover space left to center within. Measuring the
    /// real rendered width of each piece and inserting a matching leading
    /// gap is the only way to center it precisely.
    fn discover_pagination_row(&mut self, ui: &mut Ui, base_url: &str) {
        let prev_text = format!("{} Previous", egui_phosphor::regular::CARET_LEFT);
        let next_text = format!("Next {}", egui_phosphor::regular::CARET_RIGHT);
        let page_text = format!("Page {}", self.discover_skip / DISCOVER_PAGE_SIZE + 1);

        let btn_font = ui.style().text_styles.get(&egui::TextStyle::Button).cloned().unwrap_or(FontId::proportional(14.0));
        let body_font = ui.style().text_styles.get(&egui::TextStyle::Body).cloned().unwrap_or(FontId::proportional(13.0));
        let text_w = |ui: &Ui, text: &str, font: FontId| -> f32 {
            ui.painter().layout_no_wrap(text.to_string(), font, Color32::WHITE).size().x
        };
        let btn_pad = ui.spacing().button_padding.x * 2.0;
        let spacing = ui.spacing().item_spacing.x;

        let prev_w = text_w(ui, &prev_text, btn_font.clone()) + btn_pad;
        let next_w = text_w(ui, &next_text, btn_font) + btn_pad;
        let page_w = text_w(ui, &page_text, body_font);
        let total_w = prev_w + next_w + page_w + spacing * 2.0;
        let lead_gap = ((ui.available_width() - total_w) / 2.0).max(0.0);

        ui.horizontal(|ui| {
            ui.add_space(lead_gap);
            if ui.add_enabled(self.discover_skip > 0, egui::Button::new(prev_text)).clicked() {
                self.discover_skip = self.discover_skip.saturating_sub(DISCOVER_PAGE_SIZE);
                self.trigger_discover_fetch(base_url);
            }
            ui.label(RichText::new(page_text).size(13.0).color(super::theme::MUTED));
            if ui
                .add_enabled(self.discover_catalog.len() >= DISCOVER_PAGE_SIZE, egui::Button::new(next_text))
                .clicked()
            {
                self.discover_skip += DISCOVER_PAGE_SIZE;
                self.trigger_discover_fetch(base_url);
            }
        });
    }

    fn pill_banner(&self, ui: &mut Ui, text: &str, color: Color32) {
        ui.vertical_centered(|ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 24))
                .stroke(Stroke::new(1.0, color))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::symmetric(14, 6))
                .show(ui, |ui| {
                    ui.label(RichText::new(text).size(12.5).color(color));
                });
        });
        ui.add_space(12.0);
    }

    fn notice_banner(&self, ui: &mut Ui, text: &str) {
        self.pill_banner(ui, text, super::theme::CYAN);
    }

    /// Small centered status pill showing aggregate progress across
    /// whatever's actively downloading - a text-only mini version of the
    /// per-torrent progress bar on the Downloads page, meant to be visible
    /// from any page instead of a full-width banner that never went away.
    fn render_download_progress_pill(&mut self, ui: &mut Ui) {
        let active: Vec<_> = self.torrents.iter().filter(|t| !t.handle.stats().finished && t.handle.stats().total_bytes > 0).collect();

        if !active.is_empty() {
            let (progress, total) = active
                .iter()
                .map(|t| t.handle.stats())
                .fold((0u64, 0u64), |(p, t), s| (p + s.progress_bytes, t + s.total_bytes));
            let pct = (progress as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
            // Only call it "in progress" if something's actually fetching
            // pieces right now - a paused torrent isn't downloading just
            // because it has a nonzero total, and saying otherwise is wrong.
            let status = if active.iter().any(|t| !t.handle.is_paused()) { "Download in progress" } else { "Download paused" };
            self.pill_banner(ui, &format!("{status}... {pct:.0}%"), super::theme::PINK);
            self.download_pill_was_active = true;
            self.download_complete_pill_until = None;
            // Without this, the pill only redraws when something else
            // triggers a repaint (mouse move, etc.) - on an otherwise idle
            // page the percentage just freezes.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            return;
        }

        if self.download_pill_was_active {
            self.download_pill_was_active = false;
            self.download_complete_pill_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
        }

        if let Some(until) = self.download_complete_pill_until {
            let now = std::time::Instant::now();
            if now >= until {
                self.download_complete_pill_until = None;
                return;
            }
            self.pill_banner(ui, "Download complete", super::theme::GREEN);
            ui.ctx().request_repaint_after(until - now);
        }
    }

    // ----------------------------------------------------------- downloads

    fn add_magnet(&mut self) {
        if self.magnet_input.trim().is_empty() {
            return;
        }
        let title = magnet_display_name(&self.magnet_input);
        let magnet = self.magnet_input.clone();
        self.queue_torrent_source(crate::torrent_engine::AddSource::MagnetOrUrl(magnet), title, "Magnet link");
        self.magnet_input.clear();
    }

    fn add_magnet_titled(&mut self, title: String, magnet: String) {
        self.queue_torrent_source(crate::torrent_engine::AddSource::MagnetOrUrl(magnet), title, "Stream source");
    }

    /// Starts a torrent right away if nothing else is currently downloading,
    /// otherwise queues it - so adding several titles back to back downloads
    /// them one at a time instead of all fighting over the same bandwidth.
    fn queue_torrent_source(&mut self, source: crate::torrent_engine::AddSource, title: String, source_label: &'static str) {
        let anything_active = !self.torrent_add_rx.is_empty() || self.torrents.iter().any(|t| !t.handle.stats().finished);
        if anything_active {
            self.notice = Some(format!("Queued \"{title}\" - starts once the current download finishes."));
            self.pending_torrent_queue.push_back((source, title, source_label));
        } else {
            self.start_torrent_source(source, title, source_label);
        }
    }

    /// Starts the next queued torrent once nothing else is actively
    /// downloading. Called every frame `downloads_page` is visible.
    fn advance_torrent_queue(&mut self) {
        let anything_active = !self.torrent_add_rx.is_empty() || self.torrents.iter().any(|t| !t.handle.stats().finished);
        if anything_active {
            return;
        }
        if let Some((source, title, source_label)) = self.pending_torrent_queue.pop_front() {
            self.start_torrent_source(source, title, source_label);
        }
    }

    /// A themed modal for picking which files in a torrent actually
    /// download - Select All/None plus per-file checkboxes, matching the
    /// look of the other popups (Sources, Add-on Store) rather than the
    /// inline expandable list this used to be.
    fn render_file_picker_popup(&mut self, ui: &mut Ui) {
        let Some(torrent_id) = self.file_picker_torrent_id else { return };
        let Some(t) = self.torrents.iter_mut().find(|t| t.id == torrent_id) else {
            self.file_picker_torrent_id = None;
            return;
        };
        let Some(file_infos) = t.handle.with_metadata(|meta| meta.file_infos.clone()).ok() else {
            self.file_picker_torrent_id = None;
            return;
        };

        let content_w = 440.0_f32;
        let mut open = true;
        let mut done_clicked = false;
        let short_title = truncate_title(&t.title, 40);
        egui::Window::new(format!("Files - {short_title}"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(super::theme::PANEL)
                    .stroke(Stroke::new(1.0, super::theme::BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(16)),
            )
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                // Fixed, capped width - without this, the size label on each
                // file row (or any other child) claiming "the rest of the
                // available width" makes the whole window balloon out to
                // match, since there's nothing else bounding it.
                ui.set_max_width(content_w);
                ui.set_min_width(content_w);
                ui.horizontal(|ui| {
                    if ui.button("Select All").clicked() {
                        t.selected_files = None;
                        self.torrent_engine.set_only_files(t.handle.clone(), (0..file_infos.len()).collect());
                    }
                    if ui.button("Select None").clicked() {
                        t.selected_files = Some(std::collections::HashSet::new());
                        self.torrent_engine.set_only_files(t.handle.clone(), std::collections::HashSet::new());
                    }
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for (file_id, info) in file_infos.iter().enumerate() {
                        let mut selected = t.selected_files.as_ref().is_none_or(|s| s.contains(&file_id));
                        let label = format!("{}   ({})", info.relative_filename.to_string_lossy(), format_bytes(info.len));
                        if ui.checkbox(&mut selected, label).changed() {
                            let set = t.selected_files.get_or_insert_with(|| (0..file_infos.len()).collect());
                            if selected {
                                set.insert(file_id);
                            } else {
                                set.remove(&file_id);
                            }
                            self.torrent_engine.set_only_files(t.handle.clone(), set.clone());
                        }
                    }
                });
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Download Selected").color(Color32::from_rgb(20, 8, 14)))
                                .fill(super::theme::PINK)
                                .min_size(egui::vec2(180.0, 30.0)),
                        )
                        .clicked()
                    {
                        done_clicked = true;
                    }
                });
            });

        if !open || done_clicked {
            self.torrent_engine.start_download(t.handle.clone());
            self.file_picker_torrent_id = None;
        }
    }

    /// Confirms whether removing a torrent should also delete its
    /// downloaded data from disk, or just drop it from the app's list -
    /// removing was previously instant and always kept the files, with no
    /// way to actually delete them.
    fn render_remove_confirm_popup(&mut self, ui: &mut Ui) {
        let Some(torrent_id) = self.remove_confirm_torrent_id else { return };
        let Some(t) = self.torrents.iter().find(|t| t.id == torrent_id) else {
            self.remove_confirm_torrent_id = None;
            return;
        };

        let content_w = 380.0_f32;
        let mut open = true;
        let mut chosen: Option<bool> = None; // Some(delete_files)
        egui::Window::new(format!("Remove \"{}\"?", truncate_title(&t.title, 40)))
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
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.set_width(content_w);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(egui_phosphor::regular::TRASH).size(32.0).color(super::theme::PINK));
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Just remove it from the app, or also delete the downloaded files from disk?")
                            .size(13.0)
                            .color(super::theme::MUTED),
                    );
                });
                ui.add_space(16.0);
                ui.columns(2, |cols| {
                    // `ui.columns` gives each column a left-aligned layout,
                    // which Button inherits for its own text alignment -
                    // recenter explicitly so labels aren't pinned left of a
                    // button stretched to the full column width.
                    cols[0].with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Remove from App").color(super::theme::TEXT))
                                    .fill(Color32::from_gray(45))
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                            )
                            .clicked()
                        {
                            chosen = Some(false);
                        }
                    });
                    cols[1].with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Delete Files Too").color(Color32::from_rgb(20, 8, 14)))
                                    .fill(super::theme::ERROR)
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                            )
                            .clicked()
                        {
                            chosen = Some(true);
                        }
                    });
                });
            });

        if !open {
            self.remove_confirm_torrent_id = None;
        }
        if let Some(delete_files) = chosen {
            let info_hash = t.handle.info_hash().as_string();
            if delete_files {
                // librqbit's own delete_files only cleans up its managed
                // output folder - completed files already got moved out of
                // there into their category folder (route_completed_file),
                // so those have to be deleted separately from where they
                // actually ended up.
                if let Ok(file_infos) = t.handle.with_metadata(|meta| meta.file_infos.clone()) {
                    let settings_snapshot = self.settings.lock().unwrap().clone();
                    for &file_id in &t.routed_files {
                        if let Some(info) = file_infos.get(file_id) {
                            let filename = info.relative_filename.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                            if !filename.is_empty() {
                                let dest_dir = resolve_dest_dir(&settings_snapshot, &filename);
                                let dest_path = std::path::Path::new(&dest_dir).join(&filename);
                                tokio::spawn(async move {
                                    let _ = tokio::fs::remove_file(dest_path).await;
                                });
                            }
                        }
                    }
                }
            }
            let _ = crate::downloads::torrents::remove(&self.db_conn, &info_hash);
            self.torrent_engine.remove(torrent_id, delete_files);
            self.torrents.retain(|t| t.id != torrent_id);
            self.remove_confirm_torrent_id = None;
        }
    }

    /// Hands a magnet/URL/.torrent bytes to the real librqbit engine and
    /// tracks the pending add - `downloads_page` drains `torrent_add_rx` each
    /// frame and moves whatever resolves into `self.torrents`.
    fn start_torrent_source(&mut self, source: crate::torrent_engine::AddSource, title: String, source_label: &'static str) {
        self.notice = Some(format!("Adding \"{title}\"..."));
        let engine = self.torrent_engine.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            // For a magnet, librqbit's own add_torrent doesn't return until it
            // has actually found a peer willing to hand over the metadata -
            // for a rare/dead swarm that can take forever, with nothing to
            // show for it. Bound it so a bad magnet reports an error instead
            // of leaving "Adding..." on screen indefinitely.
            let result = match tokio::time::timeout(std::time::Duration::from_secs(45), engine.add(source)).await {
                Ok(Ok(added)) => Ok((added, title, source_label)),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("Timed out looking for peers with this torrent's metadata - the swarm may be dead.".to_string()),
            };
            let _ = tx.send(result);
        });
        self.torrent_add_rx.push(rx);
    }

    fn poll_torrent_adds(&mut self) {
        let pending = std::mem::take(&mut self.torrent_add_rx);
        for rx in pending {
            match rx.try_recv() {
                Ok(Ok((added, title, source_label))) => {
                    // Added paused (see TorrentEngine::add) - nothing
                    // downloads until the file picker's Done button starts
                    // it, so the popup has to appear now, not after the
                    // fact.
                    self.file_picker_torrent_id = Some(added.id);
                    self.notice = None;
                    let _ = crate::downloads::torrents::upsert(
                        &self.db_conn,
                        &added.handle.info_hash().as_string(),
                        &title,
                        source_label,
                        &added.output_dir.to_string_lossy(),
                        &std::collections::HashSet::new(),
                    );
                    self.torrents.push(AddedTorrent {
                        id: added.id,
                        handle: added.handle,
                        title,
                        source_label,
                        output_dir: added.output_dir,
                        selected_files: None,
                        routed_files: std::collections::HashSet::new(),
                    });
                }
                Ok(Err(e)) => self.notice = Some(format!("Couldn't add torrent: {e}")),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.torrent_add_rx.push(rx),
                // The add task is gone without ever sending a result (it
                // panicked) - without this, "Adding..." would sit on screen
                // forever with no indication anything went wrong.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.notice = Some("Couldn't add torrent: the add task ended unexpectedly.".to_string());
                }
            }
        }
    }

    fn open_source_picker(&mut self, title: String, kind: String, id: String, base_url: String) {
        self.source_picker_open = true;
        self.source_picker_title = title;
        self.source_picker_results.clear();
        self.source_picker_error = None;
        self.source_picker_loading = true;
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            let _ = tx.send(fetch_streams(&base_url, &kind, &id).await);
        });
        self.source_picker_rx = Some(rx);
    }

    fn render_source_picker(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if let Some(rx) = &self.source_picker_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(results) => self.source_picker_results = results,
                    Err(e) => self.source_picker_error = Some(e),
                }
                self.source_picker_loading = false;
                self.source_picker_rx = None;
            }
        }

        let mut open = self.source_picker_open;
        let mut to_add: Option<usize> = None;
        egui::Window::new(format!("Sources for \"{}\"", self.source_picker_title))
            .collapsible(false)
            .resizable(true)
            .default_size([820.0, 520.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(super::theme::PANEL)
                    .stroke(Stroke::new(1.0, super::theme::BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(16)),
            )
            .open(&mut open)
            .show(&ctx, |ui| {
                if self.source_picker_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.add_space(6.0);
                        ui.label(RichText::new("Searching stream sources...").color(super::theme::MUTED));
                    });
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    return;
                }
                if let Some(err) = &self.source_picker_error {
                    ui.label(RichText::new(err).color(super::theme::ERROR));
                    return;
                }
                if self.source_picker_results.is_empty() {
                    ui.label(RichText::new("No sources found for this title.").color(super::theme::MUTED));
                    return;
                }
                ui.label(
                    RichText::new(format!("{} sources found", self.source_picker_results.len()))
                        .size(11.0)
                        .color(super::theme::MUTED),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                    for (i, r) in self.source_picker_results.iter().enumerate() {
                        addon_row_frame(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&r.filename).size(13.0).strong().color(super::theme::TEXT));
                                    ui.add_space(4.0);
                                    if r.seeders.is_empty() && r.size.is_empty() && r.uploader.is_empty() && r.raw_meta.is_empty() {
                                        // nothing to show
                                    } else if r.raw_meta.is_empty() {
                                        ui.horizontal_wrapped(|ui| {
                                            if !r.quality.is_empty() {
                                                stream_badge(ui, &r.quality, super::theme::PINK);
                                            }
                                            if !r.seeders.is_empty() {
                                                stream_badge(ui, &format!("{} {}", egui_phosphor::regular::ARROW_UP, r.seeders), super::theme::SUCCESS);
                                            }
                                            if !r.size.is_empty() {
                                                stream_badge(ui, &format!("{} {}", egui_phosphor::regular::HARD_DRIVE, r.size), super::theme::CYAN);
                                            }
                                            if !r.uploader.is_empty() {
                                                stream_badge(ui, &format!("{} {}", egui_phosphor::regular::USERS, r.uploader), super::theme::MUTED);
                                            }
                                        });
                                    } else {
                                        ui.label(RichText::new(&r.raw_meta).size(11.0).color(super::theme::MUTED));
                                    }
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui
                                        .add(egui::Button::new(RichText::new("Add to Downloads").color(Color32::from_rgb(20, 8, 14))).fill(super::theme::PINK))
                                        .clicked()
                                    {
                                        to_add = Some(i);
                                    }
                                });
                            });
                        });
                        ui.add_space(8.0);
                    }
                });
            });
        self.source_picker_open = open;

        if let Some(i) = to_add {
            let magnet = self.source_picker_results[i].magnet.clone();
            let title = self.source_picker_title.clone();
            self.add_magnet_titled(title, magnet);
            self.source_picker_open = false;
            self.page = Page::Downloads;
        }
    }

    fn add_torrent_file(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("torrent", &["torrent"]).pick_file() else {
            return;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            self.notice = Some("Could not read that .torrent file.".to_string());
            return;
        };
        let title = torrent::parse_torrent_file(&bytes).map(|m| m.name).unwrap_or_else(|_| "Uploaded torrent".to_string());
        self.queue_torrent_source(crate::torrent_engine::AddSource::TorrentBytes(bytes), title, "Uploaded .torrent");
    }

    fn downloads_page(&mut self, ui: &mut Ui) {
        self.poll_torrent_adds();
        self.advance_torrent_queue();
        if !self.torrent_add_rx.is_empty() || !self.pending_torrent_queue.is_empty() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
        }
        if self.file_picker_torrent_id.is_some() {
            self.render_file_picker_popup(ui);
        }
        if self.remove_confirm_torrent_id.is_some() {
            self.render_remove_confirm_popup(ui);
        }

        ui.label(RichText::new("Downloads").size(26.0).strong().color(super::theme::TEXT));
        ui.add_space(4.0);
        ui.label(RichText::new("Add a torrent, then track it in the queue below").size(14.0).color(super::theme::MUTED));
        ui.add_space(20.0);

        if let Some(notice) = self.notice.clone() {
            self.notice_banner(ui, &notice);
        }

        // Step 1: add
        full_card(ui, |ui| {
            ui.label(RichText::new("STEP 1 - ADD A TORRENT").size(11.0).color(super::theme::MUTED).strong());
            ui.add_space(8.0);
            ui.add(
                egui::TextEdit::multiline(&mut self.magnet_input)
                    .desired_rows(2)
                    .desired_width(ui.available_width())
                    .hint_text("magnet:?xt=urn:btih:..."),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(RichText::new("Add Magnet").color(Color32::from_rgb(20, 8, 14))).fill(super::theme::PINK))
                    .clicked()
                {
                    self.add_magnet();
                }
                if ui.button(format!("{} Upload .torrent file", egui_phosphor::regular::UPLOAD_SIMPLE)).clicked() {
                    self.add_torrent_file();
                }
            });

            if !self.torrents.is_empty() {
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(10.0);
                let mut to_stream: Option<String> = None;
                let mut any_live = false;
                for t in self.torrents.iter_mut() {
                    let stats = t.handle.stats();
                    let file_infos = t.handle.with_metadata(|meta| meta.file_infos.clone()).ok();

                    // Media file to stream/play, picked by extension - apps,
                    // archives, subs, nfo and cover art aren't things you
                    // "stream".
                    let video_file = file_infos.as_ref().and_then(|infos| {
                        infos
                            .iter()
                            .enumerate()
                            .find(|(_, info)| {
                                matches!(
                                    detect_file_category(&info.relative_filename.to_string_lossy()),
                                    FileCategory::Video | FileCategory::Audio
                                )
                            })
                            .map(|(id, info)| (id, info.len))
                    });

                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&t.title).size(15.0).strong().color(super::theme::TEXT));
                            ui.label(RichText::new(t.source_label).size(11.0).color(super::theme::MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(format!("{} Remove", egui_phosphor::regular::TRASH)).clicked() {
                                self.remove_confirm_torrent_id = Some(t.id);
                            }
                            // Nothing left to fetch on a finished download -
                            // pause/resume doesn't mean anything for it.
                            if !stats.finished {
                                if t.handle.is_paused() {
                                    if ui
                                        .add(egui::Button::new(RichText::new(format!("{} Resume", egui_phosphor::regular::PLAY)).color(Color32::from_rgb(20, 8, 14))).fill(super::theme::PINK))
                                        .clicked()
                                    {
                                        self.torrent_engine.start_download(t.handle.clone());
                                    }
                                } else if ui.button(format!("{} Pause", egui_phosphor::regular::PAUSE)).clicked() {
                                    self.torrent_engine.pause_download(t.handle.clone());
                                }
                            }
                            if let Some((file_id, _)) = video_file {
                                if ui
                                    .add(egui::Button::new(RichText::new("Stream").color(Color32::from_rgb(20, 8, 14))).fill(super::theme::PINK))
                                    .clicked()
                                {
                                    to_stream = Some(self.torrent_engine.stream_url(t.id, file_id));
                                }
                            }
                            if file_infos.as_ref().is_some_and(|f| f.len() > 1) {
                                if ui.button(format!("{} Files", egui_phosphor::regular::LIST_CHECKS)).clicked() {
                                    self.file_picker_torrent_id = Some(t.id);
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);

                    if let Some(err) = &stats.error {
                        ui.label(RichText::new(format!("Error: {err}")).size(12.0).color(super::theme::ERROR));
                    } else if stats.total_bytes == 0 {
                        ui.label(RichText::new("Resolving torrent metadata...").size(12.0).color(super::theme::MUTED));
                        any_live = true;
                    } else {
                        let frac = stats.progress_bytes as f32 / stats.total_bytes as f32;
                        ui.add(egui::ProgressBar::new(frac).desired_height(20.0).show_percentage());
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{} / {}", format_bytes(stats.progress_bytes), format_bytes(stats.total_bytes)))
                                    .size(11.0)
                                    .color(super::theme::MUTED),
                            );
                            if let Some(live) = &stats.live {
                                any_live = true;
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} down / {} up",
                                            format_speed((live.download_speed.mbps * 1024.0 * 1024.0) as u64),
                                            format_speed((live.upload_speed.mbps * 1024.0 * 1024.0) as u64)
                                        ))
                                        .size(11.0)
                                        .color(super::theme::MUTED),
                                    );
                                });
                            }
                        });

                        // Route each newly-completed file into its category
                        // folder (settings::resolve_dest_dir) - streamed with
                        // a small fixed buffer so a multi-GB file never sits
                        // fully in RAM during the move.
                        if stats.error.is_none() {
                            if let Some(infos) = &file_infos {
                                for (file_id, info) in infos.iter().enumerate() {
                                    let done = stats.file_progress.get(file_id).copied().unwrap_or(0);
                                    let selected = t.selected_files.as_ref().is_none_or(|s| s.contains(&file_id));
                                    if selected && info.len > 0 && done >= info.len && !t.routed_files.contains(&file_id) {
                                        t.routed_files.insert(file_id);
                                        let src = t.output_dir.join(&info.relative_filename);
                                        let filename = info.relative_filename.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                                        let settings_snapshot = self.settings.lock().unwrap().clone();
                                        route_completed_file(src, filename, settings_snapshot);
                                        let _ = crate::downloads::torrents::upsert(
                                            &self.db_conn,
                                            &t.handle.info_hash().as_string(),
                                            &t.title,
                                            t.source_label,
                                            &t.output_dir.to_string_lossy(),
                                            &t.routed_files,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    ui.add_space(10.0);
                }
                if any_live {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                }
                if let Some(url) = to_stream {
                    self.stop_embedded();
                    self.stream_input = url;
                    self.playing_embedded = true;
                    self.page = Page::Streaming;
                }

                if !self.pending_torrent_queue.is_empty() {
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(10.0);
                    ui.label(RichText::new(format!("UP NEXT ({})", self.pending_torrent_queue.len())).size(11.0).color(super::theme::MUTED).strong());
                    ui.add_space(6.0);
                    for (_, title, _) in &self.pending_torrent_queue {
                        ui.label(RichText::new(format!("{} {title}", egui_phosphor::regular::CLOCK)).size(13.0).color(super::theme::MUTED));
                    }
                }
            }
        });
        ui.add_space(16.0);

        // Step 2: add a direct download
        full_card(ui, |ui| {
            ui.label(RichText::new("STEP 2 - ADD DIRECT DOWNLOAD").size(11.0).color(super::theme::MUTED).strong());
            ui.add_space(4.0);
            ui.label(RichText::new("Paste any direct download URL").size(12.0).color(super::theme::MUTED));
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.download_url_input)
                    .desired_width(ui.available_width())
                    .text_color(super::theme::TEXT)
                    .hint_text("https://example.com/file.zip"),
            );
            ui.add_space(10.0);
            if ui
                .add(egui::Button::new(RichText::new("Add to Queue").color(Color32::from_rgb(20, 8, 14))).fill(super::theme::PINK))
                .clicked()
                && !self.download_url_input.trim().is_empty()
            {
                let url = self.download_url_input.trim().to_string();
                let filename = url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("download").to_string();
                self.enqueue_download(url, filename);
                self.download_url_input.clear();
            }
        });
        ui.add_space(16.0);

        // Download queue - its own card, not sharing the add-form's card, so
        // the empty state isn't just dead space floating inside a form.
        let items = queue::get_all(&self.db_conn).unwrap_or_default();
        let any_active = items.iter().any(|d| d.status == queue::DownloadStatus::Active);
        if any_active {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }

        if items.is_empty() {
            return;
        }

        full_card(ui, |ui| {
            ui.label(RichText::new("DOWNLOAD QUEUE").size(11.0).color(super::theme::MUTED).strong());
            ui.add_space(10.0);
            let mut to_remove: Option<String> = None;
            for item in &items {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let cat_icon = match detect_file_category(&item.filename) {
                        FileCategory::Video => egui_phosphor::regular::FILM_SLATE,
                        FileCategory::Audio => egui_phosphor::regular::MUSIC_NOTES,
                        FileCategory::Archive => egui_phosphor::regular::FILE_ZIP,
                        FileCategory::Programs => egui_phosphor::regular::TERMINAL_WINDOW,
                        FileCategory::Other => egui_phosphor::regular::FILE,
                    };
                    ui.label(RichText::new(cat_icon).size(18.0).color(super::theme::CYAN));
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&item.filename).size(14.0).strong().color(super::theme::TEXT));
                        let status_color = match item.status {
                            queue::DownloadStatus::Completed => super::theme::SUCCESS,
                            queue::DownloadStatus::Failed => super::theme::ERROR,
                            queue::DownloadStatus::Active => super::theme::PINK,
                            _ => super::theme::MUTED,
                        };
                        ui.label(RichText::new(item.status.as_str()).size(11.0).color(status_color));
                        if let Some(err) = &item.error_msg {
                            ui.label(RichText::new(err).size(11.0).color(super::theme::ERROR));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if matches!(
                            item.status,
                            queue::DownloadStatus::Queued | queue::DownloadStatus::Completed | queue::DownloadStatus::Failed
                        ) && ui.button(egui_phosphor::regular::TRASH).clicked()
                        {
                            to_remove = Some(item.id.clone());
                        }
                        let frac = match item.total_bytes {
                            Some(t) if t > 0 => item.bytes_done as f32 / t as f32,
                            _ => 0.0,
                        };
                        ui.add_sized([180.0, 18.0], egui::ProgressBar::new(frac).show_percentage());
                        if item.status == queue::DownloadStatus::Active {
                            let speed = self.download_speeds.get(&item.id).copied().unwrap_or(0);
                            if speed > 0 {
                                ui.label(RichText::new(format_speed(speed)).size(12.0).color(super::theme::CYAN));
                            }
                        }
                    });
                });
                ui.add_space(6.0);
                ui.separator();
            }
            if let Some(id) = to_remove {
                let _ = queue::remove(&self.db_conn, &id);
            }
        });
    }

    // ------------------------------------------------------------ streaming

    fn stop_embedded(&mut self) {
        self.embedded = None;
        self.playing_embedded = false;
    }

    fn play_external(&mut self) {
        self.stop_embedded();
        self.player_error = None;
        let target = self.stream_input.trim().to_string();
        if target.is_empty() {
            self.player_error = Some("Enter a URL or file path first.".to_string());
            return;
        }
        match open::that(&target) {
            Ok(()) => {
                self.now_playing_external_player = None;
                self.now_playing = Some(target);
            }
            Err(e) => self.player_error = Some(format!("Could not open player: {e}")),
        }
    }

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

    fn streaming_page(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Stream").size(26.0).strong().color(super::theme::TEXT));
        ui.add_space(4.0);
        ui.label(
            RichText::new("Play a direct video URL or local file - embedded via mpv, or handed off to your system player")
                .size(14.0)
                .color(super::theme::MUTED),
        );
        ui.add_space(20.0);

        self.render_download_progress_pill(ui);

        if let Some(err) = self.player_error.clone() {
            egui::Frame::new()
                .fill(Color32::from_rgb(60, 20, 30))
                .stroke(Stroke::new(1.0, super::theme::ERROR))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(err).size(13.0).color(super::theme::ERROR));
                });
            ui.add_space(12.0);
        }

        full_card(ui, |ui| {
            ui.label(RichText::new("SOURCE").size(11.0).color(super::theme::MUTED).strong());
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.stream_input)
                    .desired_width(ui.available_width())
                    .text_color(super::theme::TEXT)
                    .hint_text("https://... or /path/to/file.mkv"),
            );
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Browse local file").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        self.stream_input = path.to_string_lossy().to_string();
                    }
                }
                if ui.button(format!("{} Open Externally", egui_phosphor::regular::ARROW_SQUARE_OUT)).clicked() {
                    self.play_external();
                }
                if ui
                    .add(
                        egui::Button::new(RichText::new(format!("{} Play Embedded", egui_phosphor::regular::PLAY_CIRCLE)).color(Color32::from_rgb(20, 8, 14)))
                            .fill(super::theme::PINK),
                    )
                    .clicked()
                {
                    // Actual geometry is computed below, once the video area is laid out this frame.
                    self.playing_embedded = true;
                }
            });
            ui.add_space(10.0);
            ui.label(RichText::new("TRY A VERIFIED SAMPLE").size(10.5).color(super::theme::MUTED));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for clip in sample_clips() {
                    if ui.button(format!("{} ({})", clip.name, clip.size_label)).clicked() {
                        self.stream_input = clip.url.to_string();
                    }
                }
            });
        });
        ui.add_space(16.0);

        // Embedded video area: full width, 16:9 (capped), directly above the Now Playing bar.
        // The cap also accounts for remaining window height so the Now Playing
        // card below it never gets pushed past the visible area on a short window.
        let now_playing_reserve = 100.0;
        let avail_w = ui.available_width();
        let video_h = (avail_w * 9.0 / 16.0).min(420.0).min((ui.available_height() - now_playing_reserve).max(120.0));
        let (video_rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, video_h), egui::Sense::hover());
        ui.painter().rect_filled(video_rect, CornerRadius::same(10), Color32::from_rgb(0x05, 0x08, 0x10));

        if self.playing_embedded && self.embedded.is_none() {
            let ppp = ui.ctx().pixels_per_point();
            let x = (video_rect.min.x * ppp) as i32;
            let y = (video_rect.min.y * ppp) as i32;
            let w = (video_rect.width() * ppp) as u32;
            let h = (video_rect.height() * ppp) as u32;
            self.play_embedded_at(x, y, w, h);
        } else if let Some(player) = self.embedded.as_mut() {
            if player.is_running() {
                let ppp = ui.ctx().pixels_per_point();
                let x = (video_rect.min.x * ppp) as i32;
                let y = (video_rect.min.y * ppp) as i32;
                let w = (video_rect.width() * ppp) as u32;
                let h = (video_rect.height() * ppp) as u32;

                // mpv's own fullscreen (OSC button or `f` key) is reported over
                // its JSON IPC socket - react to it by reparenting the embedded
                // window to the root window so it actually covers the monitor.
                if let Some(fullscreen) = player.poll_fullscreen_toggle() {
                    if fullscreen {
                        player.enter_native_fullscreen();
                    } else {
                        player.exit_native_fullscreen(x, y, w, h);
                    }
                }
                player.reposition(x, y, w, h);
                // Poll the IPC socket frequently so the fullscreen toggle above
                // reacts promptly instead of only whenever egui next repaints.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else {
                // mpv exited on its own (e.g. its native quit) - without a Stop
                // button, this is the only place that notices and clears state.
                self.stop_embedded();
                self.now_playing = None;
            }
        }

        if self.embedded.is_none() {
            ui.painter().text(
                video_rect.center(),
                Align2::CENTER_CENTER,
                if self.playing_embedded {
                    "Starting mpv..."
                } else {
                    "No embedded playback active"
                },
                FontId::proportional(15.0),
                super::theme::MUTED,
            );
        }
        ui.add_space(16.0);

        // Now Playing spans full width, directly under the player.
        full_card(ui, |ui| {
            ui.label(RichText::new("NOW PLAYING").size(11.0).color(super::theme::MUTED).strong());
            ui.add_space(8.0);
            match &self.now_playing {
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
                None => {
                    ui.label(RichText::new("Nothing playing yet - paste a source above, or pick one from Discover.").size(13.0).color(super::theme::MUTED));
                }
            }
        });
    }

    // ------------------------------------------------------------- add-ons

    fn addons_page(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Add-ons").size(26.0).strong().color(super::theme::TEXT));
        ui.add_space(4.0);
        ui.label(
            RichText::new("What webtor.io supports beyond the desktop app")
                .size(14.0)
                .color(super::theme::MUTED),
        );
        ui.add_space(20.0);

        self.render_download_progress_pill(ui);

        if let Some(rx) = &self.addon_install_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok((_base_url, manifest)) if manifest.catalogs.is_empty() => {
                        self.addon_install_error =
                            Some(format!("\"{}\" has no movie/series catalogs to browse - it's a stream source, add it below instead.", manifest.name));
                    }
                    Ok((base_url, manifest)) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.discover_addons.push(AddonSource { name: manifest.name, base_url, built_in: false });
                        let _ = save_settings(&settings);
                        drop(settings);
                        self.new_addon_url.clear();
                    }
                    Err(e) => self.addon_install_error = Some(e),
                }
                self.addon_install_loading = false;
                self.addon_install_rx = None;
            }
        }

        if let Some(rx) = &self.stream_addon_install_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok((_base_url, manifest)) if !manifest.resources.iter().any(|r| r == "stream") => {
                        self.stream_addon_install_error = Some(format!("\"{}\" does not resolve streams - it's a catalog source, add it above instead.", manifest.name));
                    }
                    Ok((base_url, manifest)) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.stream_addons.push(AddonSource { name: manifest.name, base_url, built_in: false });
                        let _ = save_settings(&settings);
                        drop(settings);
                        self.new_stream_addon_url.clear();
                    }
                    Err(e) => self.stream_addon_install_error = Some(e),
                }
                self.stream_addon_install_loading = false;
                self.stream_addon_install_rx = None;
            }
        }

        full_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("ADD-ON SOURCES").size(11.0).color(super::theme::MUTED).strong());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Manage Discover catalogs and Stremio stream resolvers (e.g. Torrentio)")
                            .size(12.0)
                            .color(super::theme::MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new(format!("{} Manage Sources", egui_phosphor::regular::PUZZLE_PIECE)).color(Color32::from_rgb(20, 8, 14))).fill(super::theme::PINK))
                        .clicked()
                    {
                        self.sources_popup_open = true;
                    }
                });
            });
        });
        ui.add_space(16.0);

        if self.sources_popup_open {
            self.render_sources_popup(ui);
        }

        full_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("ADD-ONS / SOURCE STORE").size(11.0).color(super::theme::MUTED).strong());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Browse Stremio's real community addon catalog and install with one click - no URLs to hunt for")
                            .size(12.0)
                            .color(super::theme::MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new(format!("{} Add-Ons/Source Store", egui_phosphor::regular::STOREFRONT)).color(Color32::from_rgb(20, 8, 14))).fill(super::theme::PINK))
                        .clicked()
                    {
                        self.addon_store_open = true;
                    }
                });
            });
        });
        ui.add_space(16.0);

        if self.addon_store_open {
            self.render_addon_store_popup(ui);
        }

        full_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("WHAT'S SUPPORTED").size(11.0).color(super::theme::MUTED).strong());
                    ui.add_space(4.0);
                    ui.label(RichText::new("Integrations webtor.io offers beyond this app").size(12.0).color(super::theme::MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(format!("{} View Add-ons", egui_phosphor::regular::TELEVISION)).clicked() {
                        self.features_popup_open = true;
                    }
                });
            });
        });

        if self.features_popup_open {
            self.render_features_popup(ui);
        }
    }

    fn render_feature_card(ui: &mut Ui, f: &FeatureCard) {
        super::theme::card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(f.icon).size(24.0).color(super::theme::CYAN));
            ui.add_space(8.0);
            ui.label(RichText::new(f.title).size(15.0).strong().color(super::theme::TEXT));
            ui.add_space(4.0);
            ui.label(RichText::new(f.desc).size(12.5).color(super::theme::MUTED));
            if !f.badges.is_empty() {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for badge in f.badges {
                        egui::Frame::new()
                            .fill(Color32::from_rgba_unmultiplied(0x00, 0xce, 0xc9, 25))
                            .corner_radius(CornerRadius::same(4))
                            .inner_margin(Margin::symmetric(6, 2))
                            .show(ui, |ui| {
                                ui.label(RichText::new(*badge).size(10.0).color(super::theme::CYAN));
                            });
                    }
                });
            }
            if let Some(link) = f.link {
                ui.add_space(8.0);
                if ui.button(format!("{} {}", egui_phosphor::regular::ARROW_SQUARE_OUT, f.link_label)).clicked() {
                    let _ = open::that(link);
                }
            }
        });
    }

    fn render_features_popup(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut open = self.features_popup_open;
        egui::Window::new("Add-ons")
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 480.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(super::theme::PANEL)
                    .stroke(Stroke::new(1.0, super::theme::BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(0)),
            )
            .open(&mut open)
            .show(&ctx, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    let tab_button = |ui: &mut Ui, label: &str, active: bool| -> bool {
                        let color = if active { super::theme::TEXT } else { super::theme::MUTED };
                        let text = RichText::new(label).size(13.5).strong().color(color);
                        let resp = ui.add(egui::Button::new(text).frame(false).min_size(egui::vec2(0.0, 30.0)));
                        if active {
                            let rect = resp.rect;
                            ui.painter().line_segment(
                                [egui::pos2(rect.left(), rect.bottom() + 2.0), egui::pos2(rect.right(), rect.bottom() + 2.0)],
                                Stroke::new(2.0, super::theme::PINK),
                            );
                        }
                        resp.clicked()
                    };
                    if tab_button(ui, "Sources", self.features_popup_tab == SourcesTab::Sources) {
                        self.features_popup_tab = SourcesTab::Sources;
                    }
                    ui.add_space(18.0);
                    if tab_button(ui, "Stremio", self.features_popup_tab == SourcesTab::Stremio) {
                        self.features_popup_tab = SourcesTab::Stremio;
                    }
                });
                ui.add_space(2.0);
                ui.separator();

                egui::Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| {
                    let features = webtor_features();
                    let shown: Vec<&FeatureCard> = features
                        .iter()
                        .filter(|f| match self.features_popup_tab {
                            SourcesTab::Stremio => f.title == "Stremio Integration",
                            SourcesTab::Sources => f.title != "Stremio Integration",
                        })
                        .collect();
                    egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                        for f in shown {
                            Self::render_feature_card(ui, f);
                            ui.add_space(10.0);
                        }
                    });
                });
            });
        self.features_popup_open = open;
    }

    fn render_addon_store_popup(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.addon_store_catalog.is_empty() && self.addon_store_rx.is_none() && !self.addon_store_loading && self.addon_store_error.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            tokio::spawn(async move {
                let _ = tx.send(fetch_addon_store().await);
            });
            self.addon_store_rx = Some(rx);
            self.addon_store_loading = true;
        }
        if let Some(rx) = &self.addon_store_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(catalog) => self.addon_store_catalog = catalog,
                    Err(e) => self.addon_store_error = Some(e),
                }
                self.addon_store_loading = false;
                self.addon_store_rx = None;
            }
        }

        let installed_urls: std::collections::HashSet<String> = {
            let settings = self.settings.lock().unwrap();
            settings
                .discover_addons
                .iter()
                .chain(settings.stream_addons.iter())
                .map(|a| a.base_url.trim_end_matches('/').to_string())
                .collect()
        };

        let mut open = self.addon_store_open;
        let mut to_install: Option<usize> = None;
        egui::Window::new("Add-Ons / Source Store")
            .collapsible(false)
            .resizable(true)
            .default_size([600.0, 520.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(super::theme::PANEL)
                    .stroke(Stroke::new(1.0, super::theme::BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(16)),
            )
            .open(&mut open)
            .show(&ctx, |ui| {
                ui.label(
                    RichText::new("Stremio's real community addon catalog - the same one the official Stremio app uses")
                        .size(11.5)
                        .color(super::theme::MUTED),
                );
                ui.add_space(8.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.addon_store_search)
                        .desired_width(ui.available_width())
                        .text_color(super::theme::TEXT)
                        .hint_text(format!("{} Search addons by name...", egui_phosphor::regular::MAGNIFYING_GLASS)),
                );
                ui.add_space(10.0);

                if self.addon_store_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.add_space(6.0);
                        ui.label(RichText::new("Loading addon catalog...").color(super::theme::MUTED));
                    });
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    return;
                }
                if let Some(err) = &self.addon_store_error {
                    ui.label(RichText::new(err).color(super::theme::ERROR));
                    return;
                }

                let query = self.addon_store_search.to_lowercase();
                let matches: Vec<(usize, &StoreAddon)> = self
                    .addon_store_catalog
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| query.is_empty() || a.name.to_lowercase().contains(&query) || a.description.to_lowercase().contains(&query))
                    .collect();

                ui.label(RichText::new(format!("{} addons", matches.len())).size(11.0).color(super::theme::MUTED));
                ui.add_space(8.0);

                egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                    for (i, a) in matches {
                        let is_stream = a.resources.iter().any(|r| r == "stream");
                        let base_url = a.transport_url.trim_end_matches("/manifest.json").trim_end_matches('/').to_string();
                        let already_installed = installed_urls.contains(base_url.trim_end_matches('/'));

                        addon_row_frame(ui, |ui| {
                            ui.horizontal(|ui| {
                                if !a.logo.is_empty() {
                                    ui.add(
                                        egui::Image::new(&a.logo)
                                            .max_size(egui::vec2(36.0, 36.0))
                                            .corner_radius(CornerRadius::same(4))
                                            .show_loading_spinner(false),
                                    );
                                    ui.add_space(8.0);
                                }
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(&a.name).size(13.5).strong().color(super::theme::TEXT));
                                        stream_badge(ui, if is_stream { "Stream" } else { "Catalog" }, if is_stream { super::theme::PINK } else { super::theme::CYAN });
                                    });
                                    if !a.description.is_empty() {
                                        ui.label(RichText::new(&a.description).size(11.0).color(super::theme::MUTED));
                                    }
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if already_installed {
                                        ui.add_enabled(false, egui::Button::new(format!("{} Installed", egui_phosphor::regular::CHECK_CIRCLE)));
                                    } else if ui.button("Install").clicked() {
                                        to_install = Some(i);
                                    }
                                });
                            });
                        });
                        ui.add_space(8.0);
                    }
                });
            });
        self.addon_store_open = open;

        if let Some(i) = to_install {
            let a = &self.addon_store_catalog[i];
            let base_url = a.transport_url.trim_end_matches("/manifest.json").trim_end_matches('/').to_string();
            let is_stream = a.resources.iter().any(|r| r == "stream");
            let source = AddonSource { name: a.name.clone(), base_url, built_in: false };
            let mut settings = self.settings.lock().unwrap();
            if is_stream {
                settings.stream_addons.push(source);
            } else {
                settings.discover_addons.push(source);
            }
            let _ = save_settings(&settings);
        }
    }

    fn render_sources_popup(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut open = self.sources_popup_open;
        egui::Window::new("Add-on Sources")
            .collapsible(false)
            .resizable(true)
            .default_size([520.0, 460.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(super::theme::PANEL)
                    .stroke(Stroke::new(1.0, super::theme::BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(0)),
            )
            .open(&mut open)
            .show(&ctx, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    let tab_button = |ui: &mut Ui, label: &str, active: bool| -> bool {
                        let color = if active { super::theme::TEXT } else { super::theme::MUTED };
                        let text = RichText::new(label).size(13.5).strong().color(color);
                        let resp = ui.add(egui::Button::new(text).frame(false).min_size(egui::vec2(0.0, 30.0)));
                        if active {
                            let rect = resp.rect;
                            ui.painter().line_segment(
                                [egui::pos2(rect.left(), rect.bottom() + 2.0), egui::pos2(rect.right(), rect.bottom() + 2.0)],
                                Stroke::new(2.0, super::theme::PINK),
                            );
                        }
                        resp.clicked()
                    };
                    if tab_button(ui, "Sources", self.sources_popup_tab == SourcesTab::Sources) {
                        self.sources_popup_tab = SourcesTab::Sources;
                    }
                    ui.add_space(18.0);
                    if tab_button(ui, "Stremio", self.sources_popup_tab == SourcesTab::Stremio) {
                        self.sources_popup_tab = SourcesTab::Stremio;
                    }
                });
                ui.add_space(2.0);
                ui.separator();

                egui::Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| match self.sources_popup_tab {
                    SourcesTab::Sources => self.render_discover_sources_tab(ui),
                    SourcesTab::Stremio => self.render_stream_sources_tab(ui),
                });
            });
        self.sources_popup_open = open;
    }

    fn render_discover_sources_tab(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Stremio-compatible addons Discover can browse - same protocol webtor.io's Discover uses")
                .size(12.0)
                .color(super::theme::MUTED),
        );
        ui.add_space(12.0);

        let addons = self.settings.lock().unwrap().discover_addons.clone();
        let mut to_remove: Option<usize> = None;
        let mut to_activate: Option<usize> = None;
        for (i, a) in addons.iter().enumerate() {
            addon_row_frame(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&a.name).size(14.0).strong().color(super::theme::TEXT));
                        ui.label(RichText::new(&a.base_url).size(11.0).color(super::theme::MUTED));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !a.built_in && ui.button(egui_phosphor::regular::TRASH).clicked() {
                            to_remove = Some(i);
                        }
                        if i == self.discover_addon_index {
                            ui.add_enabled(false, egui::Button::new(format!("{} Active", egui_phosphor::regular::CHECK_CIRCLE)));
                        } else if ui.button("Use this source").clicked() {
                            to_activate = Some(i);
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }
        if let Some(i) = to_activate {
            self.discover_addon_index = i;
            self.discover_manifest = None;
            self.discover_catalog.clear();
        }
        if let Some(i) = to_remove {
            let mut settings = self.settings.lock().unwrap();
            settings.discover_addons.remove(i);
            let _ = save_settings(&settings);
            drop(settings);
            self.discover_addon_index = 0;
            self.discover_manifest = None;
            self.discover_catalog.clear();
        }

        ui.add_space(4.0);
        ui.separator();
        ui.add_space(10.0);

        ui.label(RichText::new("Add a Discover source").size(12.5).strong().color(super::theme::TEXT));
        ui.add_space(6.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.new_addon_url)
                .desired_width(ui.available_width())
                .text_color(super::theme::TEXT)
                .hint_text("https://v3-cinemeta.strem.io (any Stremio addon's base URL or manifest.json link)"),
        );
        ui.add_space(8.0);
        if ui.add_enabled(!self.addon_install_loading, egui::Button::new("Validate & Add")).clicked() {
            let trimmed = self.new_addon_url.trim();
            if !trimmed.is_empty() {
                let base_url = trimmed.trim_end_matches("/manifest.json").trim_end_matches('/').to_string();
                let (tx, rx) = std::sync::mpsc::channel();
                tokio::spawn(async move {
                    let result = fetch_addon_manifest(&base_url).await.map(|m| (base_url.clone(), m));
                    let _ = tx.send(result);
                });
                self.addon_install_rx = Some(rx);
                self.addon_install_loading = true;
                self.addon_install_error = None;
            }
        }
        if self.addon_install_loading {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Validating addon...").size(12.0).color(super::theme::MUTED));
            });
        }
        if let Some(err) = &self.addon_install_error {
            ui.add_space(6.0);
            ui.label(RichText::new(err).size(12.0).color(super::theme::ERROR));
        }
    }

    fn render_stream_sources_tab(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Addons that resolve real torrent sources for a title (e.g. Torrentio) - powers Discover's Find Sources button")
                .size(12.0)
                .color(super::theme::MUTED),
        );
        ui.add_space(12.0);

        let stream_addons = self.settings.lock().unwrap().stream_addons.clone();
        if stream_addons.is_empty() {
            ui.label(RichText::new("None installed - Discover's Find Sources button stays disabled until you add one below.").size(12.5).color(super::theme::MUTED));
            ui.add_space(10.0);
        } else {
            let mut to_remove: Option<usize> = None;
            for (i, a) in stream_addons.iter().enumerate() {
                addon_row_frame(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&a.name).size(14.0).strong().color(super::theme::TEXT));
                            ui.label(RichText::new(&a.base_url).size(11.0).color(super::theme::MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(egui_phosphor::regular::TRASH).clicked() {
                                to_remove = Some(i);
                            }
                        });
                    });
                });
                ui.add_space(8.0);
            }
            if let Some(i) = to_remove {
                let mut settings = self.settings.lock().unwrap();
                settings.stream_addons.remove(i);
                let _ = save_settings(&settings);
            }
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(10.0);
        }

        ui.label(RichText::new("Add a Stream source").size(12.5).strong().color(super::theme::TEXT));
        ui.add_space(6.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.new_stream_addon_url)
                .desired_width(ui.available_width())
                .text_color(super::theme::TEXT)
                .hint_text("https://torrentio.strem.fun (any Stremio stream addon's base URL or manifest.json link)"),
        );
        ui.add_space(8.0);
        if ui.add_enabled(!self.stream_addon_install_loading, egui::Button::new("Validate & Add")).clicked() {
            let trimmed = self.new_stream_addon_url.trim();
            if !trimmed.is_empty() {
                let base_url = trimmed.trim_end_matches("/manifest.json").trim_end_matches('/').to_string();
                let (tx, rx) = std::sync::mpsc::channel();
                tokio::spawn(async move {
                    let result = fetch_addon_manifest(&base_url).await.map(|m| (base_url.clone(), m));
                    let _ = tx.send(result);
                });
                self.stream_addon_install_rx = Some(rx);
                self.stream_addon_install_loading = true;
                self.stream_addon_install_error = None;
            }
        }
        if self.stream_addon_install_loading {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Validating addon...").size(12.0).color(super::theme::MUTED));
            });
        }
        if let Some(err) = &self.stream_addon_install_error {
            ui.add_space(6.0);
            ui.label(RichText::new(err).size(12.0).color(super::theme::ERROR));
        }
    }

    // ------------------------------------------------------------ settings

    fn settings_page(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("settings_save_bar")
            .frame(egui::Frame::NONE.inner_margin(Margin::symmetric(0, 12)))
            .show_inside(ui, |ui| {
                ui.separator();
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Settings take effect immediately.").size(12.0).color(super::theme::MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let clicked_save = ui
                            .add(
                                egui::Button::new(RichText::new("Save Settings").size(14.0).strong().color(Color32::from_rgb(20, 8, 14)))
                                    .fill(super::theme::PINK)
                                    .min_size(egui::vec2(160.0, 40.0)),
                            )
                            .clicked();
                        if clicked_save {
                            let s = self.settings.lock().unwrap();
                            self.settings_saved = save_settings(&s).is_ok();
                        }
                        if self.settings_saved {
                            ui.label(RichText::new(format!("{} Saved", egui_phosphor::regular::CHECK_CIRCLE)).color(super::theme::CYAN).size(13.0));
                        }
                    });
                });
            });

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Settings").size(26.0).strong().color(super::theme::TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::Frame::new()
                        .fill(super::theme::CARD)
                        .stroke(Stroke::new(1.0, super::theme::BORDER))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.label(RichText::new(format!("v{}", env!("WEBTORAPP_VERSION"))).size(10.5).color(super::theme::MUTED));
                        });
                });
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Configure download behavior").size(14.0).color(super::theme::MUTED));
            ui.add_space(20.0);

            self.render_download_progress_pill(ui);

            let mut s = self.settings.lock().unwrap();

            full_card(ui, |ui| {
                ui.label(RichText::new("THREADS / DOWNLOAD").size(11.0).color(super::theme::MUTED).strong());
                ui.add_space(4.0);
                ui.style_mut().spacing.slider_width = (ui.available_width() - 60.0).max(60.0);
                if ui.add(egui::Slider::new(&mut s.threads_per_download, 1..=16)).changed() {
                    self.settings_saved = false;
                }
                ui.add_space(10.0);
                ui.label(RichText::new("MAX CONCURRENT DOWNLOADS").size(11.0).color(super::theme::MUTED).strong());
                ui.add_space(4.0);
                if ui.add(egui::Slider::new(&mut s.max_concurrent_downloads, 1..=10)).changed() {
                    self.settings_saved = false;
                }
            });
            ui.add_space(12.0);

            full_card(ui, |ui| {
                ui.label(RichText::new("DOWNLOAD FOLDER").size(11.0).color(super::theme::MUTED).strong());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::TextEdit::singleline(&mut s.download_dir).desired_width(ui.available_width() - 90.0).font(egui::TextStyle::Monospace))
                        .changed()
                    {
                        self.settings_saved = false;
                    }
                    if ui.button("Browse").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            s.download_dir = dir.to_string_lossy().to_string();
                            self.settings_saved = false;
                        }
                    }
                });
            });
            ui.add_space(12.0);

            full_card(ui, |ui| {
                ui.label(RichText::new("FILE TYPE ROUTING").size(11.0).color(super::theme::MUTED).strong());
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Optional per-type overrides - leave blank to use the default download folder above.")
                        .size(11.0)
                        .color(super::theme::MUTED),
                );
                ui.add_space(8.0);

                const LABEL_W: f32 = 90.0;
                const BTN_W: f32 = 64.0;
                let fr = &mut s.folder_rules;
                let rows: [(&str, &mut Option<String>); 4] = [
                    ("Video", &mut fr.video),
                    ("Audio", &mut fr.audio),
                    ("Archives", &mut fr.archive),
                    ("Programs", &mut fr.programs),
                ];
                let mut changed = false;
                for (label, slot) in rows {
                    ui.horizontal(|ui| {
                        ui.add_sized([LABEL_W, 20.0], egui::Label::new(RichText::new(label).size(12.0)));
                        let mut val = slot.clone().unwrap_or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut val)
                                .desired_width(ui.available_width() - BTN_W - 32.0)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("(use default)"),
                        );
                        if resp.changed() {
                            *slot = if val.is_empty() { None } else { Some(val) };
                            changed = true;
                        }
                        if ui.button("Browse").clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                *slot = Some(dir.to_string_lossy().to_string());
                                changed = true;
                            }
                        }
                        if slot.is_some() && ui.button("x").clicked() {
                            *slot = None;
                            changed = true;
                        }
                    });
                }
                if changed {
                    self.settings_saved = false;
                }
            });
        });
    }

    /// The window's close button asks whether to minimize to tray or quit
    /// for real, every time, unless the user checked "Never ask again" -
    /// then `remembered_close_action` just does that from now on.
    fn handle_tray_and_close(&mut self, ctx: &egui::Context) {
        // Minimize-to-tray only makes sense where there's an actual tray
        // icon to bring it back from - Linux only for now (see crate::tray).
        // Elsewhere, closing the window really closes the app.
        #[cfg(not(target_os = "linux"))]
        {
            let _ = ctx;
            return;
        }
        #[cfg(target_os = "linux")]
        {
            if self.tray_notice_open {
                return;
            }
            if ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);

                let remembered = self.settings.lock().unwrap().remembered_close_action;
                match remembered {
                    Some(settings::CloseAction::MinimizeToTray) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    }
                    Some(settings::CloseAction::Quit) => {
                        std::process::exit(0);
                    }
                    None => {
                        self.tray_notice_open = true;
                    }
                }
            }
        }
    }

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

    fn render_tray_notice(&mut self, ctx: &egui::Context) {
        // mpv's embedded window is a real X11 child, not something egui
        // draws - it always paints over this popup's screen region
        // otherwise, so hide it while the popup is up (see
        // EmbeddedPlayer::set_hidden).
        if let Some(player) = self.embedded.as_mut() {
            player.set_hidden(self.tray_notice_open);
        }
        if !self.tray_notice_open {
            return;
        }
        let content_w = 380.0_f32;
        let mut chosen: Option<settings::CloseAction> = None;
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
                    ui.label(RichText::new(egui_phosphor::regular::BELL).size(32.0).color(super::theme::PINK));
                    ui.add_space(10.0);
                    ui.label(RichText::new("Close Webtor Desktop?").size(15.0).strong().color(super::theme::TEXT));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Keep it running in the system tray so downloads keep going, or quit for real.")
                            .size(13.0)
                            .color(super::theme::MUTED),
                    );
                });
                ui.add_space(14.0);
                ui.checkbox(&mut self.never_ask_again_checked, "Never ask again");
                ui.add_space(14.0);
                ui.columns(2, |cols| {
                    // `ui.columns` gives each column a left-aligned layout,
                    // which Button inherits for its own text alignment -
                    // recenter explicitly so labels aren't pinned left of a
                    // button stretched to the full column width.
                    cols[0].with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Close to Tray").color(Color32::from_rgb(20, 8, 14)))
                                    .fill(super::theme::PINK)
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                            )
                            .clicked()
                        {
                            chosen = Some(settings::CloseAction::MinimizeToTray);
                        }
                    });
                    cols[1].with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Quit App").color(super::theme::TEXT))
                                    .fill(Color32::from_gray(45))
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                            )
                            .clicked()
                        {
                            chosen = Some(settings::CloseAction::Quit);
                        }
                    });
                });
            });

        if let Some(action) = chosen {
            if self.never_ask_again_checked {
                let mut settings = self.settings.lock().unwrap();
                settings.remembered_close_action = Some(action);
                let _ = save_settings(&settings);
            }
            self.tray_notice_open = false;
            match action {
                settings::CloseAction::MinimizeToTray => ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)),
                settings::CloseAction::Quit => std::process::exit(0),
            }
        }
    }
}

impl eframe::App for WebtorApp {
    // `logic` runs every frame regardless of window visibility, unlike
    // `ui` (which eframe skips whenever the window is occluded/minimized) -
    // the close-request race that used to let a close slip through while
    // hidden happened because the cancel-close call lived in `ui` and
    // never got the chance to run for that frame.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray_and_close(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render_tray_notice(&ui.ctx().clone());
        #[cfg(target_os = "windows")]
        self.render_player_choice_popup(&ui.ctx().clone());
        self.drain_download_events();

        if !self.logged_in {
            self.login_page(ui);
            return;
        }

        if self.page != Page::Streaming && self.embedded.is_some() {
            self.stop_embedded();
        }

        let ctx = ui.ctx().clone();
        egui::Panel::left("sidebar")
            .exact_size(96.0)
            .frame(egui::Frame::new().fill(super::theme::PANEL))
            .show_inside(ui, |ui| {
                self.sidebar(ui, &ctx);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(super::theme::BG).inner_margin(Margin::same(24)))
            .show_inside(ui, |ui| match self.page {
                Page::Dashboard => self.dashboard_page(ui),
                Page::Discover => self.discover_page(ui),
                Page::Streaming => self.streaming_page(ui),
                Page::Downloads => self.downloads_page(ui),
                Page::AddOns => self.addons_page(ui),
                Page::Settings => self.settings_page(ui),
            });
    }
}

#[cfg(test)]
mod tests {
    use super::parse_stream_meta;

    #[test]
    fn parses_torrentio_single_line_convention() {
        let (seeders, size, uploader) = parse_stream_meta("👤 16 💾 43.69 GB ⚙️ ilCorSaRoNeRo");
        assert_eq!(seeders, "16");
        assert_eq!(size, "43.69 GB");
        assert_eq!(uploader, "ilCorSaRoNeRo");
    }

    #[test]
    fn parses_multi_line_markers_with_uneven_spacing() {
        // Real response from an installed addon: markers spread across
        // several lines, "👥" instead of "👤" for seeders, extra spaces.
        let text = "📺 2160p | BluRay | HEVC\n🔊 5.1\n💾 7.5 GB   👥 215 seeders\n🔗 Torrentcsv";
        let (seeders, size, _uploader) = parse_stream_meta(text);
        assert_eq!(size, "7.5 GB");
        assert_eq!(seeders, "215 seeders");
    }

    #[test]
    fn returns_empty_when_no_markers_present() {
        let (seeders, size, uploader) = parse_stream_meta("2160p | BluRay | HEVC");
        assert!(seeders.is_empty());
        assert!(size.is_empty());
        assert!(uploader.is_empty());
    }
}
