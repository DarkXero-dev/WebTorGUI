use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct FolderRules {
    #[serde(default)]
    pub video: Option<String>,
    #[serde(default)]
    pub audio: Option<String>,
    #[serde(default)]
    pub archive: Option<String>,
    #[serde(default)]
    pub programs: Option<String>,
}

/// A Stremio-compatible addon installed as a Discover catalog source.
/// `base_url` has no trailing slash and no `/manifest.json` suffix.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct AddonSource {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub built_in: bool,
}

fn default_discover_addons() -> Vec<AddonSource> {
    vec![AddonSource {
        name: "Cinemeta".to_string(),
        base_url: "https://v3-cinemeta.strem.io".to_string(),
        built_in: true,
    }]
}

/// Without a stream source installed, Discover's "Find Sources" button stays
/// permanently disabled - there's nothing to actually find a torrent with.
/// Torrentio is the one every Stremio-alike ships with for exactly that
/// reason, so it's built in rather than something the user has to know to
/// go add themselves.
fn default_stream_addons() -> Vec<AddonSource> {
    vec![AddonSource {
        name: "Torrentio".to_string(),
        base_url: "https://torrentio.strem.fun".to_string(),
        built_in: true,
    }]
}

/// What the window's close button should do, remembered only once the user
/// checks "Never ask again" on the confirmation dialog - otherwise it's
/// asked every time.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    MinimizeToTray,
    Quit,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AppSettings {
    pub download_dir: String,
    pub threads_per_download: u8,
    pub max_concurrent_downloads: u8,
    pub quiet_hours_enabled: bool,
    pub quiet_hours_start: Option<String>,
    pub quiet_hours_end: Option<String>,
    #[serde(default)]
    pub folder_rules: FolderRules,
    #[serde(default = "default_discover_addons")]
    pub discover_addons: Vec<AddonSource>,
    /// Stremio-compatible addons that resolve real torrent/stream sources
    /// for a title (e.g. Torrentio) - a different addon category than
    /// `discover_addons`, which only provide browsable catalogs.
    #[serde(default = "default_stream_addons")]
    pub stream_addons: Vec<AddonSource>,
    /// What the close button does, if the user has told it to stop asking.
    /// `None` means always show the confirmation dialog.
    #[serde(default)]
    pub remembered_close_action: Option<CloseAction>,
}

impl Default for AppSettings {
    fn default() -> Self {
        let download_dir = dirs::download_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string();
        Self {
            download_dir,
            threads_per_download: 4,
            max_concurrent_downloads: 3,
            quiet_hours_enabled: false,
            quiet_hours_start: None,
            quiet_hours_end: None,
            folder_rules: FolderRules::default(),
            discover_addons: default_discover_addons(),
            stream_addons: default_stream_addons(),
            remembered_close_action: None,
        }
    }
}

pub enum FileCategory {
    Video,
    Audio,
    Archive,
    Programs,
    Other,
}

pub fn detect_file_category(filename: &str) -> FileCategory {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mkv" | "mp4" | "avi" | "mov" | "wmv" | "flv" | "m2ts" | "ts" | "m4v" | "webm" | "vob"
        | "mpg" | "mpeg" => FileCategory::Video,
        "mp3" | "flac" | "aac" | "ogg" | "wav" | "m4a" | "opus" | "wma" | "alac" | "ape" => {
            FileCategory::Audio
        }
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "cbz" | "cbr" | "iso"
        | "tgz" | "tbz2" => FileCategory::Archive,
        "exe" | "msi" | "deb" | "rpm" | "appimage" | "pkg" | "dmg" | "flatpak" | "snap" => {
            FileCategory::Programs
        }
        _ => FileCategory::Other,
    }
}

/// Resolve the destination directory a file should land in, applying the
/// per-category override if one is set, falling back to `download_dir`.
pub fn resolve_dest_dir(settings: &AppSettings, filename: &str) -> String {
    let rule = match detect_file_category(filename) {
        FileCategory::Video => &settings.folder_rules.video,
        FileCategory::Audio => &settings.folder_rules.audio,
        FileCategory::Archive => &settings.folder_rules.archive,
        FileCategory::Programs => &settings.folder_rules.programs,
        FileCategory::Other => &None,
    };
    rule.clone().unwrap_or_else(|| settings.download_dir.clone())
}

fn settings_path() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    let dir = base.join("webtorapp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("settings.json"))
}

pub fn load_settings() -> AppSettings {
    let mut settings: AppSettings = settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // `#[serde(default = "default_stream_addons")]` only fires for a
    // *missing* key - an existing settings.json saved before Torrentio
    // became built in has an explicit `"stream_addons": []` on disk, which
    // deserializes straight past the default. Enforce it here instead, so
    // it's there for every install regardless of when settings.json was
    // first written, and reappears even if a user manually edits it out.
    for addon in default_stream_addons() {
        if !settings.stream_addons.iter().any(|a| a.base_url == addon.base_url) {
            settings.stream_addons.push(addon);
        }
    }

    settings
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
            threads_per_download: 4,
            max_concurrent_downloads: 3,
            quiet_hours_enabled: true,
            quiet_hours_start: Some("22:00".to_string()),
            quiet_hours_end: Some("07:00".to_string()),
            folder_rules: FolderRules {
                video: Some("/home/test/Videos".to_string()),
                audio: None,
                archive: None,
                programs: None,
            },
            discover_addons: default_discover_addons(),
            stream_addons: Vec::new(),
            remembered_close_action: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(original.download_dir, restored.download_dir);
        assert_eq!(original.threads_per_download, restored.threads_per_download);
        assert_eq!(original.quiet_hours_start, restored.quiet_hours_start);
        assert_eq!(original.folder_rules.video, restored.folder_rules.video);
    }

    #[test]
    fn detects_categories_by_extension() {
        assert!(matches!(detect_file_category("movie.mkv"), FileCategory::Video));
        assert!(matches!(detect_file_category("song.mp3"), FileCategory::Audio));
        assert!(matches!(detect_file_category("archive.zip"), FileCategory::Archive));
        assert!(matches!(detect_file_category("setup.exe"), FileCategory::Programs));
        assert!(matches!(detect_file_category("readme.txt"), FileCategory::Other));
    }

    #[test]
    fn resolve_dest_dir_uses_category_override_when_set() {
        let mut settings = AppSettings::default();
        settings.download_dir = "/home/test/Downloads".to_string();
        settings.folder_rules.video = Some("/home/test/Videos".to_string());

        assert_eq!(resolve_dest_dir(&settings, "movie.mkv"), "/home/test/Videos");
        assert_eq!(resolve_dest_dir(&settings, "song.mp3"), "/home/test/Downloads");
    }
}
