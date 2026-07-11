use anyhow::{anyhow, Result};
use base64::Engine;
use cookie_store::CookieStore;
use reqwest_cookie_store::CookieStoreMutex;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

const BASE: &str = "https://webtor.io";

/// SuperTokens' `sFrontToken` cookie (see `front_token_json`/`find_email`
/// below) turns out to carry only session bookkeeping (session handle,
/// refresh-token hashes, empty role/permission arrays) - no email, verified
/// against a real account's decoded token. So the real email comes from
/// `set_profile_scrape`, fed by the login browser navigating to
/// webtor.io/profile and scanning its rendered text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScrapedProfile {
    email: Option<String>,
}

/// A logged-in webtor.io session. webtor.io sits behind a Cloudflare bot
/// challenge that a plain HTTP client cannot pass (verified: identical
/// headers, curl gets through and reqwest doesn't - it comes down to
/// TLS/HTTP client fingerprinting, not anything we send). So the actual
/// email-code login happens in a real embedded browser window
/// (`crate::browser_login`), and this struct's cookie jar is populated
/// afterward via `import_cookies`.
#[derive(Clone)]
pub struct WebtorAuth {
    cookie_store: Arc<CookieStoreMutex>,
    scraped: Arc<Mutex<ScrapedProfile>>,
}

impl WebtorAuth {
    pub fn new() -> Result<Self> {
        Self::from_cookie_store(CookieStore::default())
    }

    fn from_cookie_store(store: CookieStore) -> Result<Self> {
        let cookie_store = Arc::new(CookieStoreMutex::new(store));
        Ok(Self { cookie_store, scraped: Arc::new(Mutex::new(ScrapedProfile::default())) })
    }

    /// Feeds the login browser's scrape of webtor.io/profile's whole
    /// rendered text - only overwrites the email when this pass actually
    /// found one, so a failed/partial scrape doesn't clobber it back to
    /// unknown.
    pub fn set_profile_scrape(&self, full_text: Option<&str>) {
        let Some(email) = full_text.and_then(find_email_in_text) else { return };
        self.scraped.lock().unwrap().email = Some(email);
    }

    /// Imports cookies captured from a real browser session (the
    /// browser-login flow, since webtor.io's Cloudflare challenge blocks a
    /// plain HTTP client outright). `name`/`value` pairs only - domain/path
    /// are fixed to webtor.io since that's the only site this ever visits.
    pub fn import_cookies(&self, cookies: Vec<(String, String)>) -> Result<()> {
        let url = reqwest::Url::parse(BASE)?;
        let mut store = self.cookie_store.lock().unwrap();
        for (name, value) in cookies {
            let raw = cookie_store::RawCookie::build((name, value)).domain("webtor.io").path("/").build();
            let _ = store.insert_raw(&raw, &url);
        }
        Ok(())
    }

    /// Whether the cookie jar actually holds a live SuperTokens session
    /// cookie.
    pub fn has_session(&self) -> bool {
        let store = self.cookie_store.lock().unwrap();
        let found = store.iter_any().any(|c| c.name() == "sAccessToken");
        found
    }

    /// Decodes SuperTokens' `sFrontToken` cookie (a base64 JSON blob it sets
    /// specifically for frontends to read - not the HttpOnly access token).
    /// The cookie is `sFrontToken` - "front-token" is only the name of the
    /// *header* SuperTokens sends it in, not the cookie itself (verified
    /// against a live session).
    fn front_token_json(&self) -> Option<serde_json::Value> {
        let store = self.cookie_store.lock().unwrap();
        let front_token = store.iter_any().find(|c| c.name() == "sFrontToken")?.value().to_string();
        drop(store);
        let decoded = base64::engine::general_purpose::STANDARD.decode(front_token).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Best-effort account label for display: the real email scraped from
    /// webtor.io/profile after login, falling back to whatever (likely
    /// nothing) the sFrontToken cookie happens to carry.
    pub fn account_label(&self) -> Option<String> {
        self.scraped.lock().unwrap().email.clone().or_else(|| find_email(&self.front_token_json()?))
    }

    fn serialize(&self) -> Result<String> {
        let store = self.cookie_store.lock().unwrap();
        let mut buf = Vec::new();
        // `save` (persistent-only) silently drops any cookie with no
        // Expires/Max-Age - which SuperTokens' session cookies are, so the
        // saved session ended up empty every time. Need the non-persistent
        // variant to actually keep them.
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut buf).map_err(|e| anyhow!("{e}"))?;
        drop(store);
        let persisted = PersistedSession {
            cookies: String::from_utf8(buf)?,
            scraped: self.scraped.lock().unwrap().clone(),
        };
        Ok(serde_json::to_string(&persisted)?)
    }

    fn deserialize(json: &str) -> Result<Self> {
        // Current format wraps the cookie-jar JSON alongside the scraped
        // email. A session saved before scraping existed is just the raw
        // cookie-jar JSON with no wrapper - fall back to reading it
        // directly so an existing login isn't forced to happen again.
        if let Ok(persisted) = serde_json::from_str::<PersistedSession>(json) {
            let store = cookie_store::serde::json::load_all(persisted.cookies.as_bytes()).map_err(|e| anyhow!("{e}"))?;
            let auth = Self::from_cookie_store(store)?;
            *auth.scraped.lock().unwrap() = persisted.scraped;
            Ok(auth)
        } else {
            let store = cookie_store::serde::json::load_all(json.as_bytes()).map_err(|e| anyhow!("{e}"))?;
            Self::from_cookie_store(store)
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedSession {
    cookies: String,
    #[serde(default)]
    scraped: ScrapedProfile,
}

fn session_path() -> Result<std::path::PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
    let dir = base.join("webtorapp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("webtor_session.enc"))
}

/// Persists the real SuperTokens session cookies, encrypted at rest with the
/// same machine-bound AES-256-GCM scheme as everything else this app saves
/// (see `crate::auth`) - a session cookie is as sensitive as a password, so
/// it never touches disk in plaintext.
pub fn save_session(auth: &WebtorAuth) -> Result<()> {
    let json = auth.serialize()?;
    crate::auth::encrypt_and_save(&json, &session_path()?)
}

pub fn load_session() -> Option<WebtorAuth> {
    let path = session_path().ok()?;
    let json = crate::auth::load_and_decrypt(&path)?;
    let auth = WebtorAuth::deserialize(&json).ok()?;
    if auth.has_session() {
        Some(auth)
    } else {
        None
    }
}

pub fn clear_session() -> Result<()> {
    let path = session_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Scans the profile page's plain rendered text (`document.body.innerText`,
/// no HTML/JSON structure to lean on) for something email-shaped. Manual
/// scan rather than a regex crate - just walk out from each `@` to the
/// nearest whitespace/quote/bracket on either side.
fn find_email_in_text(text: &str) -> Option<String> {
    for (i, _) in text.match_indices('@') {
        let before = &text[..i];
        let after = &text[i..];
        let is_boundary = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | ',' | '<' | '>');
        let start = before.rfind(is_boundary).map(|p| p + before[p..].chars().next().unwrap().len_utf8()).unwrap_or(0);
        let end = after.find(is_boundary).unwrap_or(after.len());
        let candidate = &text[start..i + end];
        if candidate.len() > 5 && candidate.matches('@').count() == 1 && candidate.contains('.') && !candidate.starts_with('@') && !candidate.ends_with('@') {
            return Some(candidate.trim_matches('.').to_string());
        }
    }
    None
}

fn find_email(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if s.contains('@') && s.contains('.') => Some(s.clone()),
        serde_json::Value::Object(map) => map.values().find_map(find_email),
        serde_json::Value::Array(items) => items.iter().find_map(find_email),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SuperTokens' session cookies have no Expires/Max-Age, so a
    // persistent-only serializer silently saves them as "[]" - this is
    // exactly the bug that made login not survive a restart.
    #[test]
    fn session_cookie_with_no_expiry_survives_a_round_trip() {
        let auth = WebtorAuth::new().unwrap();
        auth.import_cookies(vec![("sAccessToken".to_string(), "test-token".to_string())]).unwrap();
        assert!(auth.has_session());

        let json = auth.serialize().unwrap();
        let restored = WebtorAuth::deserialize(&json).unwrap();
        assert!(restored.has_session());
    }
}
