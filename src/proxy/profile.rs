//! Proxy profiles: URL + token pairs stored in `~/.config/claudio/config.toml`.
//!
//! **Tokens are never in argv, never in logs, never in state.json.**
//! state.json stores only the profile *name*. `Debug` impls redact the token.
//!
//! Config file format:
//! ```toml
//! [proxy]
//! default = "work"
//! [proxy.profiles.work]
//! url   = "https://proxy.example.com"
//! token = "…"
//! first_party = true  # optional: see `Profile::first_party`
//! ```
//!
//! The env var `CLAUDIO_PROXY_URL=<token>@<host>` creates an ephemeral profile
//! named `"env"` and makes it the default for this run.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::paths;

// ── Data model ────────────────────────────────────────────────────────────────

/// A single proxy profile: a base URL and a bearer token.
#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    /// `https://claude.example.net` (or `http://localhost:PORT` for local dev).
    pub url: String,
    /// Bearer token, never logged.
    pub token: String,
    /// Opt into first-party mode (`first_party = true`). Without it a session
    /// uses the gateway env, which turns off Claude Code's feature flags
    /// (Monitor, /advisor, WebSearch). With it the session is first-party
    /// against the proxy URL, which needs the proxy to pass Anthropic's
    /// server-side tools through.
    #[serde(default, skip_serializing_if = "is_false")]
    pub first_party: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("first_party", &self.first_party)
            .finish()
    }
}

impl Profile {
    /// Masked token for display: first 4 + last 4 chars, or all `*` when short.
    ///
    /// Uses `chars()` so multi-byte Unicode tokens don't cause a panic.
    pub fn masked_token(&self) -> String {
        let chars: Vec<char> = self.token.chars().collect();
        if chars.len() > 8 {
            let head: String = chars[..4].iter().collect();
            let tail: String = chars[chars.len() - 4..].iter().collect();
            format!("{head}…{tail}")
        } else {
            "*".repeat(chars.len())
        }
    }
}

/// The `[proxy]` section of `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxySection {
    /// The default profile name used by new sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Named profiles.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
}

/// The whole `config.toml` file (other sections are preserved via `extra`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy: Option<ProxySection>,
    /// All other TOML keys — preserved on write so we don't clobber them.
    #[serde(flatten)]
    extra: toml::Table,
}

// ── TOML error sanitization ────────────────────────────────────────────────────

/// Extract a safe (non-secret) diagnostic from a TOML parse error.
///
/// The full `toml::de::Error` display includes the offending source line, which
/// may contain a token. We keep only the first line (location) and discard the
/// source excerpt.
fn sanitize_toml_error(e: &toml::de::Error) -> String {
    e.to_string()
        .lines()
        .next()
        .unwrap_or("TOML parse error")
        .to_owned()
}

// ── Loading and saving ────────────────────────────────────────────────────────

/// Load the `[proxy]` section from the default `config.toml`.
/// Missing file → empty section. Parse errors do NOT include source excerpts.
pub fn load() -> io::Result<ProxySection> {
    load_from(&paths::config_file())
}

/// Load the `[proxy]` section from an explicit path (used in tests to avoid
/// mutating the global `XDG_CONFIG_HOME`).
pub fn load_from(path: &Path) -> io::Result<ProxySection> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ProxySection::default()),
        Err(e) => return Err(e),
    };
    let cfg: ConfigFile = toml::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| {
        // Report only the location line; never embed the source (may contain token).
        io::Error::other(format!(
            "config.toml: {} (token content redacted)",
            sanitize_toml_error(&e)
        ))
    })?;
    Ok(cfg.proxy.unwrap_or_default())
}

/// Save an updated `[proxy]` section back to the default `config.toml`,
/// preserving any other keys that may already be present.
pub fn save(proxy: &ProxySection) -> io::Result<()> {
    save_to(&paths::config_file(), proxy)
}

/// Save an updated `[proxy]` section to an explicit path (used in tests).
pub fn save_to(path: &Path, proxy: &ProxySection) -> io::Result<()> {
    // Load the current file (or start with an empty one) so we keep other sections.
    let bytes = std::fs::read(path).unwrap_or_default();
    let mut cfg: ConfigFile = if bytes.is_empty() {
        ConfigFile::default()
    } else {
        toml::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| {
            io::Error::other(format!(
                "config.toml: {} (token content redacted)",
                sanitize_toml_error(&e)
            ))
        })?
    };
    cfg.proxy = Some(proxy.clone());
    let out = toml::to_string_pretty(&cfg)
        .map_err(|e| io::Error::other(format!("config.toml serialize error: {e}")))?;
    paths::write_atomic(path, out.as_bytes())
}

// ── URL / env-var parsing ─────────────────────────────────────────────────────

/// Parse a `CLAUDIO_PROXY_URL` value into `(url, token)`.
///
/// Format: `<token>@<host>`, where `<host>` may be:
/// - `claude.example.net`              → `https://claude.example.net`
/// - `https://claude.example.net`      → verbatim
/// - `http://127.0.0.1:PORT`           → allowed (localhost only)
/// - `127.0.0.1:PORT`                  → `http://127.0.0.1:PORT` (localhost)
/// - `localhost:PORT`                  → `http://localhost:PORT`
///
/// Returns `Err` when:
/// - No `@` separator (missing token)
/// - The scheme is `http` but the host is not `127.0.0.1` / `localhost`
pub fn parse_proxy_url(raw: &str) -> Result<(String, String), String> {
    // Split on the LAST `@` so tokens themselves may contain `@`.
    let at = raw.rfind('@').ok_or_else(|| {
        "CLAUDIO_PROXY_URL must be <token>@<host> (missing token or '@' separator)".to_owned()
    })?;
    let token = raw[..at].trim().to_owned();
    if token.is_empty() {
        return Err("CLAUDIO_PROXY_URL: token before '@' is empty".to_owned());
    }
    let host_part = raw[at + 1..].trim().to_owned();
    if host_part.is_empty() {
        return Err("CLAUDIO_PROXY_URL: host after '@' is empty".to_owned());
    }

    let url = normalize_host(&host_part)?;
    Ok((url, token))
}

/// Turn a raw host string into a canonical `https://…` or `http://…` URL.
fn normalize_host(host: &str) -> Result<String, String> {
    // Already has a scheme.
    if let Some(rest) = host.strip_prefix("https://") {
        return Ok(format!("https://{}", rest.trim_end_matches('/')));
    }
    if let Some(rest) = host.strip_prefix("http://") {
        let trimmed = rest.trim_end_matches('/');
        let bare_host = trimmed.split(':').next().unwrap_or(trimmed);
        if !is_localhost(bare_host) {
            return Err(format!(
                "http:// is only allowed for 127.0.0.1/localhost, got '{bare_host}'"
            ));
        }
        return Ok(format!("http://{trimmed}"));
    }

    // No scheme: detect bare localhost patterns.
    let bare_host = host.split(':').next().unwrap_or(host);
    if is_localhost(bare_host) {
        Ok(format!("http://{}", host.trim_end_matches('/')))
    } else {
        Ok(format!("https://{}", host.trim_end_matches('/')))
    }
}

fn is_localhost(host: &str) -> bool {
    host == "127.0.0.1" || host == "localhost"
}

/// Return `Some(("env", Profile))` when `CLAUDIO_PROXY_URL` is set and valid.
/// Prints a warning to stderr and returns `None` on parse error.
pub fn from_env() -> Option<(&'static str, Profile)> {
    let raw = std::env::var("CLAUDIO_PROXY_URL").ok()?;
    match parse_proxy_url(&raw) {
        Ok((url, token)) => Some((
            "env",
            Profile {
                url,
                token,
                first_party: false,
            },
        )),
        Err(e) => {
            eprintln!("claudio: CLAUDIO_PROXY_URL: {e}");
            None
        }
    }
}

/// Normalize a raw host/URL string to a canonical URL (no token).
/// Same logic as the URL part of [`parse_proxy_url`] but without the token.
pub fn normalize_url(host: &str) -> Result<String, String> {
    normalize_host(host)
}

/// Derive a default profile name from a URL (the first DNS label).
///
/// `https://proxy.example.com` → `"proxy"`
/// `http://127.0.0.1:8080`      → `"localhost"`
pub fn name_from_url(url: &str) -> String {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .split(':')
        .next()
        .unwrap_or(url);
    if is_localhost(host) {
        return "localhost".to_owned();
    }
    host.splitn(2, '.').next().unwrap_or(host).to_owned()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    // ── parse_proxy_url ───────────────────────────────────────────────────────

    #[test]
    fn parse_full_https() {
        let (url, tok) = parse_proxy_url("mytoken@https://proxy.example.com").unwrap();
        assert_eq!(url, "https://proxy.example.com");
        assert_eq!(tok, "mytoken");
    }

    #[test]
    fn parse_bare_host_defaults_https() {
        let (url, tok) = parse_proxy_url("tok@claude.example.net").unwrap();
        assert_eq!(url, "https://claude.example.net");
        assert_eq!(tok, "tok");
    }

    #[test]
    fn parse_localhost_port_defaults_http() {
        let (url, tok) = parse_proxy_url("tok@127.0.0.1:8080").unwrap();
        assert_eq!(url, "http://127.0.0.1:8080");
        assert_eq!(tok, "tok");
    }

    #[test]
    fn parse_localhost_name() {
        let (url, tok) = parse_proxy_url("tok@localhost:9000").unwrap();
        assert_eq!(url, "http://localhost:9000");
        assert_eq!(tok, "tok");
    }

    #[test]
    fn parse_http_non_localhost_rejected() {
        assert!(parse_proxy_url("tok@http://claude.example.net").is_err());
    }

    #[test]
    fn parse_missing_token_rejected() {
        assert!(parse_proxy_url("@claude.example.net").is_err());
        assert!(parse_proxy_url("claude.example.net").is_err());
    }

    #[test]
    fn parse_token_with_at_sign() {
        // Token itself contains '@'; we split on the last '@'.
        let (url, tok) = parse_proxy_url("user@example.com@proxy.example.com").unwrap();
        assert_eq!(url, "https://proxy.example.com");
        assert_eq!(tok, "user@example.com");
    }

    // ── name_from_url ─────────────────────────────────────────────────────────

    #[test]
    fn name_from_url_extracts_first_label() {
        assert_eq!(name_from_url("https://proxy.example.com"), "proxy");
        assert_eq!(name_from_url("https://proxy.example.com"), "proxy");
        assert_eq!(name_from_url("http://127.0.0.1:8080"), "localhost");
    }

    // ── Debug redaction ───────────────────────────────────────────────────────

    #[test]
    fn debug_redacts_token() {
        let p = Profile {
            url: "https://x.net".into(),
            token: "supersecret".into(),
            first_party: false,
        };
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("supersecret"));
        assert!(dbg.contains("[redacted]"));
    }

    // ── TOML round-trip and file mode 0600 ───────────────────────────────────
    // Uses load_from/save_to with an explicit temp path — no global env mutation.

    #[test]
    fn round_trip_and_file_mode() {
        let dir = std::env::temp_dir().join(format!("claudio-proxy-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let mut sec = ProxySection::default();
        sec.default = Some("work".into());
        sec.profiles.insert(
            "work".into(),
            Profile {
                url: "https://proxy.example.com".into(),
                token: "topsecret".into(),
                first_party: false,
            },
        );
        save_to(&path, &sec).unwrap();

        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "config.toml must be mode 0600");

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.default.as_deref(), Some("work"));
        let p = loaded.profiles.get("work").unwrap();
        assert_eq!(p.url, "https://proxy.example.com");
        assert_eq!(p.token, "topsecret");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── first_party ───────────────────────────────────────────────────────────

    #[test]
    fn first_party_round_trips_and_defaults_to_false() {
        let dir =
            std::env::temp_dir().join(format!("claudio-proxy-gw-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // A profile without the key loads as false and saves without the key.
        std::fs::write(
            &path,
            "[proxy.profiles.plain]\nurl = \"https://a.net\"\ntoken = \"t1\"\n",
        )
        .unwrap();
        let loaded = load_from(&path).unwrap();
        assert!(!loaded.profiles["plain"].first_party);
        save_to(&path, &loaded).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("first_party"), "saved: {saved}");

        // first_party = true round-trips.
        let mut sec = ProxySection::default();
        sec.profiles.insert(
            "fp".into(),
            Profile {
                url: "https://b.net".into(),
                token: "t2".into(),
                first_party: true,
            },
        );
        save_to(&path, &sec).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("first_party = true"), "saved: {saved}");
        let loaded = load_from(&path).unwrap();
        assert!(loaded.profiles["fp"].first_party);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── masked_token ──────────────────────────────────────────────────────────

    #[test]
    fn masked_token_long_and_short() {
        let p = |t: &str| Profile {
            url: "u".into(),
            token: t.into(),
            first_party: false,
        };
        assert_eq!(p("abcdefghwxyz").masked_token(), "abcd…wxyz");
        assert_eq!(p("ab").masked_token(), "**");
    }

    /// masked_token must not panic on Unicode tokens.
    #[test]
    fn masked_token_unicode_no_panic() {
        // Token with multi-byte characters (e.g. emoji = 4 bytes each).
        let p = Profile {
            url: "u".into(),
            token: "🔑🔑🔑🔑🔑🔑🔑🔑🔑🔑".into(),
            first_party: false,
        };
        // Should not panic; result length varies by char count.
        let masked = p.masked_token();
        assert!(!masked.is_empty());
        // Short token: all stars.
        let p2 = Profile {
            url: "u".into(),
            token: "🔑🔑".into(),
            first_party: false,
        };
        assert_eq!(p2.masked_token(), "**");
    }

    // ── TOML error sanitization ───────────────────────────────────────────────

    /// A TOML parse error on a token line must NOT include the token
    /// value in the error message.
    #[test]
    fn toml_error_does_not_leak_token() {
        let dir =
            std::env::temp_dir().join(format!("claudio-proxy-toml-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Write a malformed TOML where the token line is syntactically invalid.
        let bad_toml =
            "[proxy.profiles.x]\nurl = \"https://x.net\"\ntoken = SUPERSECRETTOKEN_NO_QUOTES\n";
        std::fs::write(&path, bad_toml).unwrap();

        let err = load_from(&path).unwrap_err();
        let msg = err.to_string();
        // The error message must not contain the secret value.
        assert!(
            !msg.contains("SUPERSECRETTOKEN_NO_QUOTES"),
            "error leaked token: {msg}"
        );
        // But it should still say something useful (location).
        assert!(
            msg.contains("config.toml") || msg.contains("TOML"),
            "error not useful: {msg}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
