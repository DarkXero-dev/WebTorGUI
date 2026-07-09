use anyhow::{anyhow, Result};
use base64::Engine;
use cookie_store::CookieStore;
use reqwest_cookie_store::CookieStoreMutex;
use std::sync::Arc;

const BASE: &str = "https://webtor.io";

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
}

impl WebtorAuth {
    pub fn new() -> Result<Self> {
        Self::from_cookie_store(CookieStore::default())
    }

    fn from_cookie_store(store: CookieStore) -> Result<Self> {
        let cookie_store = Arc::new(CookieStoreMutex::new(store));
        Ok(Self { cookie_store })
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

    /// Decodes SuperTokens' `front-token` cookie (a base64 JSON blob it sets
    /// specifically for frontends to read - not the HttpOnly access token).
    fn front_token_json(&self) -> Option<serde_json::Value> {
        let store = self.cookie_store.lock().unwrap();
        let front_token = store.iter_any().find(|c| c.name() == "front-token")?.value().to_string();
        drop(store);
        let decoded = base64::engine::general_purpose::STANDARD.decode(front_token).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Best-effort account label for display. Returns `None` if the
    /// front-token cookie is missing or doesn't contain anything
    /// email-shaped.
    pub fn account_label(&self) -> Option<String> {
        find_email(&self.front_token_json()?)
    }

    /// Best-effort subscription tier, read from whatever plan/subscription
    /// claim (if any) webtor.io's access token happens to carry. Returns
    /// `None` rather than guessing - callers should show a neutral "signed
    /// in" state instead of fabricating a tier we can't actually verify.
    pub fn plan_label(&self) -> Option<String> {
        find_plan(&self.front_token_json()?)
    }

    /// Best-effort storage usage (used_bytes, total_bytes), read from
    /// whatever quota claim (if any) the access token carries. `None` if no
    /// such claim exists - callers should say usage isn't available rather
    /// than fabricating numbers.
    pub fn storage_usage(&self) -> Option<(u64, u64)> {
        find_storage(&self.front_token_json()?)
    }

    fn serialize(&self) -> Result<String> {
        let store = self.cookie_store.lock().unwrap();
        let mut buf = Vec::new();
        // `save` (persistent-only) silently drops any cookie with no
        // Expires/Max-Age - which SuperTokens' session cookies are, so the
        // saved session ended up empty every time. Need the non-persistent
        // variant to actually keep them.
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut buf).map_err(|e| anyhow!("{e}"))?;
        Ok(String::from_utf8(buf)?)
    }

    fn deserialize(json: &str) -> Result<Self> {
        let store = cookie_store::serde::json::load_all(json.as_bytes()).map_err(|e| anyhow!("{e}"))?;
        Self::from_cookie_store(store)
    }
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

fn find_email(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if s.contains('@') && s.contains('.') => Some(s.clone()),
        serde_json::Value::Object(map) => map.values().find_map(find_email),
        serde_json::Value::Array(items) => items.iter().find_map(find_email),
        _ => None,
    }
}

const PLAN_KEYS: &[&str] = &["plan", "subscription", "tier", "subscriptionTier", "subscriptionStatus", "accountType"];

fn find_plan(value: &serde_json::Value) -> Option<String> {
    if let serde_json::Value::Object(map) = value {
        for key in PLAN_KEYS {
            if let Some(v) = map.get(*key) {
                match v {
                    serde_json::Value::String(s) if !s.is_empty() => return Some(s.clone()),
                    serde_json::Value::Bool(b) if *key == "premium" || key.to_lowercase().contains("premium") => {
                        return Some(if *b { "Premium".to_string() } else { "Free".to_string() });
                    }
                    _ => {}
                }
            }
        }
        return map.values().find_map(find_plan);
    }
    if let serde_json::Value::Array(items) = value {
        return items.iter().find_map(find_plan);
    }
    None
}

const STORAGE_USED_KEYS: &[&str] = &["storageUsed", "usedBytes", "storageUsage", "bytesUsed"];
const STORAGE_TOTAL_KEYS: &[&str] = &["storageLimit", "storageQuota", "totalBytes", "quota", "storageTotal"];

fn find_storage(value: &serde_json::Value) -> Option<(u64, u64)> {
    if let serde_json::Value::Object(map) = value {
        let used = STORAGE_USED_KEYS.iter().find_map(|k| map.get(*k)).and_then(|v| v.as_u64());
        let total = STORAGE_TOTAL_KEYS.iter().find_map(|k| map.get(*k)).and_then(|v| v.as_u64());
        if let (Some(u), Some(t)) = (used, total) {
            return Some((u, t));
        }
        return map.values().find_map(find_storage);
    }
    if let serde_json::Value::Array(items) = value {
        return items.iter().find_map(find_storage);
    }
    None
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
