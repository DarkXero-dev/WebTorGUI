use anyhow::{anyhow, Result};

/// Opens the user's default system browser at webtor.io's login page. Used
/// only by the win7 build in place of an embedded WebView2 window (see
/// src/lib.rs for why). The user completes the email-code login there,
/// themselves - this app has no way to observe that browser's session.
pub fn open_system_browser() -> Result<()> {
    open::that("https://webtor.io/login").map_err(|e| anyhow!("could not open browser: {e}"))
}

/// Parses a raw `Cookie:` request header value (what a browser's DevTools
/// Network tab shows for any request to webtor.io after logging in -
/// `name1=value1; name2=value2; ...`) into the same `(name, value)` pairs
/// `WebtorAuth::import_cookies` expects from the embedded-browser flow.
///
/// The Network tab is the only place a user can get at `sAccessToken`
/// without special tooling - it's HttpOnly, so `document.cookie` in the
/// console never shows it, but DevTools still puts it in the outgoing
/// request header.
pub fn parse_cookie_header(input: &str) -> Result<Vec<(String, String)>> {
    let cookies: Vec<(String, String)> = input
        .split(';')
        .filter_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            let (name, value) = (name.trim(), value.trim());
            (!name.is_empty() && !value.is_empty()).then(|| (name.to_string(), value.to_string()))
        })
        .collect();

    if !cookies.iter().any(|(name, _)| name == "sAccessToken") {
        return Err(anyhow!(
            "No sAccessToken cookie found in that - make sure you copied the full Cookie request header after signing in."
        ));
    }

    Ok(cookies)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_looking_cookie_header() {
        let cookies = parse_cookie_header("sAccessToken=abc123; front-token=eyJ...; sRefreshToken=def456").unwrap();
        assert_eq!(cookies.len(), 3);
        assert!(cookies.contains(&("sAccessToken".to_string(), "abc123".to_string())));
        assert!(cookies.contains(&("front-token".to_string(), "eyJ...".to_string())));
    }

    #[test]
    fn rejects_input_missing_the_access_token() {
        let err = parse_cookie_header("front-token=eyJ...; sRefreshToken=def456").unwrap_err();
        assert!(err.to_string().contains("sAccessToken"));
    }

    #[test]
    fn rejects_empty_input() {
        assert!(parse_cookie_header("").is_err());
    }
}
