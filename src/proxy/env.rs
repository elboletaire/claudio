//! Build the environment for a proxy-backed `claude` session.
//!
//! [`session_env`] is a pure function: it takes the profile and an optional
//! config response from the proxy and returns the `(key, value)` pairs that
//! [`crate::proto::SpawnSpec::env`] should carry. A profile with
//! `first_party = true` gets the first-party env instead of the gateway one
//! (see [`session_env`]).
//!
//! Built-in fallback model defaults, used when the proxy is unreachable or
//! does not provide `/v1/claudio/config`:
//!
//! | Family  | Fallback model                   |
//! |---------|----------------------------------|
//! | fable   | `claude-fable-5-1[1m]`           |
//! | opus    | `claude-opus-5-5[1m]`            |
//! | sonnet  | `claude-sonnet-5-5[1m]`          |
//! | haiku   | `claude-haiku-5-5[1m]`           |

use super::api::ConfigResponse;
use super::profile::Profile;

/// Built-in fallback 1M model defaults (used when proxy is unavailable).
pub const DEFAULT_FABLE: &str = "claude-fable-5-1[1m]";
pub const DEFAULT_OPUS: &str = "claude-opus-5-5[1m]";
pub const DEFAULT_SONNET: &str = "claude-sonnet-5-5[1m]";
pub const DEFAULT_HAIKU: &str = "claude-haiku-5-5[1m]";

/// Build the `SpawnSpec.env` entries for a session that uses `profile`.
///
/// If `config` is `Some`, its `env` map overrides the fallbacks key by key.
/// The `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` are always added from
/// the profile itself, not from the proxy config (the proxy cannot know our
/// token's URL).
///
/// The default is gateway mode: `CLAUDE_CODE_USE_GATEWAY` and
/// `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` are set. Claude Code turns its
/// feature flags off in that mode (Monitor, /advisor, WebSearch).
///
/// With `profile.first_party` the session is first-party against the proxy
/// URL instead: both gateway variables are omitted, so the feature flags stay
/// on. Two more variables are set in that mode:
/// - `DISABLE_AUTOUPDATER=1`: claude must not update itself behind claudio's
///   `[claude] update_check`.
/// - `ENABLE_TOOL_SEARCH=true`: Claude Code turns tool search off for a
///   non-Anthropic base URL, in case the proxy drops `tool_reference` blocks.
///   claude-proxy passes them through, so it stays on and every request
///   carries only the tools it needs.
pub fn session_env(profile: &Profile, config: Option<&ConfigResponse>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();

    // Always set: base URL and auth token.
    env.push(("ANTHROPIC_BASE_URL".into(), profile.url.clone()));
    env.push(("ANTHROPIC_AUTH_TOKEN".into(), profile.token.clone()));

    // Gateway mode only: first-party sessions keep Claude Code's feature flags.
    if !profile.first_party {
        env.push(("CLAUDE_CODE_USE_GATEWAY".into(), "1".into()));
    }
    env.push((
        "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".into(),
        "1".into(),
    ));
    env.push(("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "1000000".into()));
    env.push(("CLAUDE_CODE_GATEWAY_HINT_HEADERS".into(), "1".into()));
    if !profile.first_party {
        env.push((
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        ));
    }
    env.push(("API_TIMEOUT_MS".into(), "30000".into()));
    if profile.first_party {
        env.push(("DISABLE_AUTOUPDATER".into(), "1".into()));
        env.push(("ENABLE_TOOL_SEARCH".into(), "true".into()));
    }

    // Model defaults: start from built-in fallbacks, then let the proxy
    // config override key by key.
    let fable = config
        .and_then(|c| c.env.get("ANTHROPIC_DEFAULT_FABLE_MODEL"))
        .map(String::as_str)
        .unwrap_or(DEFAULT_FABLE);
    let opus = config
        .and_then(|c| c.env.get("ANTHROPIC_DEFAULT_OPUS_MODEL"))
        .map(String::as_str)
        .unwrap_or(DEFAULT_OPUS);
    let sonnet = config
        .and_then(|c| c.env.get("ANTHROPIC_DEFAULT_SONNET_MODEL"))
        .map(String::as_str)
        .unwrap_or(DEFAULT_SONNET);
    let haiku = config
        .and_then(|c| c.env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL"))
        .map(String::as_str)
        .unwrap_or(DEFAULT_HAIKU);

    env.push(("ANTHROPIC_DEFAULT_FABLE_MODEL".into(), fable.into()));
    env.push(("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), opus.into()));
    env.push(("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), sonnet.into()));
    env.push(("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), haiku.into()));

    env
}

/// How to alter the environment a `claude` process inherits.
///
/// Returned by [`env_diff`] so every launcher (the daemon's PTY builder,
/// `--plain`'s `std::process::Command`) applies the same rules with its own
/// builder type.
#[derive(Debug, PartialEq)]
pub struct EnvDiff<'a> {
    /// Variables to remove from the inherited environment.
    pub remove: Vec<&'static str>,
    /// Variables to set.
    pub set: &'a [(String, String)],
}

/// The env changes for a claude session that carries `env` (a proxy's
/// [`session_env`], or empty).
///
/// Scrubs the markers of a parent claude session (see
/// [`crate::claude::SESSION_MARKERS`]). A proxy token replaces any
/// `ANTHROPIC_API_KEY`, which would otherwise win over it.
///
/// A first-party proxy env (no `CLAUDE_CODE_USE_GATEWAY`, see
/// [`session_env`]) also drops an inherited `CLAUDE_CODE_USE_GATEWAY`: it
/// would turn the feature flags off again. An inherited
/// `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is kept, since it may be set on
/// purpose; it turns the feature flags off too, so it has to be unset by hand
/// for first-party sessions.
pub fn env_diff(env: &[(String, String)]) -> EnvDiff<'_> {
    let mut remove = crate::claude::SESSION_MARKERS.to_vec();
    if env.iter().any(|(k, _)| k == "ANTHROPIC_AUTH_TOKEN") {
        remove.push("ANTHROPIC_API_KEY");
        if !env.iter().any(|(k, _)| k == "CLAUDE_CODE_USE_GATEWAY") {
            remove.push("CLAUDE_CODE_USE_GATEWAY");
        }
    }
    EnvDiff { remove, set: env }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::api::ConfigResponse;

    fn profile(url: &str, token: &str) -> Profile {
        Profile {
            url: url.into(),
            token: token.into(),
            first_party: false,
        }
    }

    fn first_party_profile(url: &str, token: &str) -> Profile {
        Profile {
            first_party: true,
            ..profile(url, token)
        }
    }

    fn env_get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn without_config_uses_fallbacks() {
        let p = profile("https://claude.example.net", "tok");
        let env = session_env(&p, None);
        assert_eq!(
            env_get(&env, "ANTHROPIC_BASE_URL"),
            Some("https://claude.example.net")
        );
        assert_eq!(env_get(&env, "ANTHROPIC_AUTH_TOKEN"), Some("tok"));
        assert_eq!(env_get(&env, "CLAUDE_CODE_USE_GATEWAY"), Some("1"));
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_FABLE_MODEL"),
            Some(DEFAULT_FABLE)
        );
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some(DEFAULT_OPUS)
        );
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some(DEFAULT_SONNET)
        );
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some(DEFAULT_HAIKU)
        );
    }

    #[test]
    fn with_config_overrides_model_defaults() {
        let p = profile("https://x.net", "tok");
        let mut cfg = ConfigResponse::default();
        cfg.env.insert(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".into(),
            "claude-sonnet-99[1m]".into(),
        );
        cfg.env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
            "claude-haiku-99[1m]".into(),
        );
        let env = session_env(&p, Some(&cfg));
        // Overridden.
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some("claude-sonnet-99[1m]")
        );
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("claude-haiku-99[1m]")
        );
        // Not overridden: falls back.
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_FABLE_MODEL"),
            Some(DEFAULT_FABLE)
        );
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some(DEFAULT_OPUS)
        );
    }

    #[test]
    fn config_other_vars_ignored() {
        // The proxy config `env` map may contain many keys; only the four
        // model-default keys are used; the rest are silently ignored.
        let p = profile("https://x.net", "tok");
        let mut cfg = ConfigResponse::default();
        cfg.env.insert("SOME_OTHER_KEY".into(), "value".into());
        let env = session_env(&p, Some(&cfg));
        assert!(env_get(&env, "SOME_OTHER_KEY").is_none());
    }

    #[test]
    fn no_anthropic_api_key_in_output() {
        let p = profile("https://x.net", "tok");
        let env = session_env(&p, None);
        // ANTHROPIC_API_KEY must not appear (the daemon removes it separately).
        assert!(env_get(&env, "ANTHROPIC_API_KEY").is_none());
    }

    #[test]
    fn env_diff_scrubs_markers_and_keeps_api_key_without_token() {
        let diff = env_diff(&[]);
        assert_eq!(diff.remove, crate::claude::SESSION_MARKERS);
        assert!(diff.set.is_empty());
    }

    #[test]
    fn env_diff_drops_api_key_only_with_auth_token() {
        let env = session_env(&profile("https://x.net", "tok"), None);
        let diff = env_diff(&env);
        assert!(diff.remove.contains(&"CLAUDECODE"));
        assert!(diff.remove.contains(&"ANTHROPIC_API_KEY"));
        assert_eq!(diff.set, env.as_slice());
        // Other entries alone don't count as a token.
        let other = [("API_TIMEOUT_MS".to_owned(), "1".to_owned())];
        assert!(!env_diff(&other).remove.contains(&"ANTHROPIC_API_KEY"));
    }

    #[test]
    fn gateway_output_is_unchanged() {
        // Must match what origin/main produces, pair for pair and in order.
        let env = session_env(&profile("https://x.net", "tok"), None);
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(
            env,
            [
                pair("ANTHROPIC_BASE_URL", "https://x.net"),
                pair("ANTHROPIC_AUTH_TOKEN", "tok"),
                pair("CLAUDE_CODE_USE_GATEWAY", "1"),
                pair("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1"),
                pair("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000"),
                pair("CLAUDE_CODE_GATEWAY_HINT_HEADERS", "1"),
                pair("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
                pair("API_TIMEOUT_MS", "30000"),
                pair("ANTHROPIC_DEFAULT_FABLE_MODEL", DEFAULT_FABLE),
                pair("ANTHROPIC_DEFAULT_OPUS_MODEL", DEFAULT_OPUS),
                pair("ANTHROPIC_DEFAULT_SONNET_MODEL", DEFAULT_SONNET),
                pair("ANTHROPIC_DEFAULT_HAIKU_MODEL", DEFAULT_HAIKU),
            ]
        );
    }

    #[test]
    fn first_party_env_omits_gateway_knobs_and_keeps_the_rest() {
        let env = session_env(&first_party_profile("https://x.net", "tok"), None);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
                "CLAUDE_CODE_GATEWAY_HINT_HEADERS",
                "API_TIMEOUT_MS",
                "DISABLE_AUTOUPDATER",
                "ENABLE_TOOL_SEARCH",
                "ANTHROPIC_DEFAULT_FABLE_MODEL",
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "ANTHROPIC_DEFAULT_SONNET_MODEL",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            ]
        );
        assert_eq!(env_get(&env, "ANTHROPIC_BASE_URL"), Some("https://x.net"));
        assert_eq!(env_get(&env, "ANTHROPIC_AUTH_TOKEN"), Some("tok"));
        assert_eq!(env_get(&env, "DISABLE_AUTOUPDATER"), Some("1"));
        assert_eq!(env_get(&env, "ENABLE_TOOL_SEARCH"), Some("true"));
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some(DEFAULT_OPUS)
        );
    }

    #[test]
    fn first_party_env_with_config_still_overrides_models() {
        let mut cfg = ConfigResponse::default();
        cfg.env.insert(
            "ANTHROPIC_DEFAULT_OPUS_MODEL".into(),
            "claude-opus-99[1m]".into(),
        );
        let env = session_env(&first_party_profile("https://x.net", "tok"), Some(&cfg));
        assert_eq!(
            env_get(&env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some("claude-opus-99[1m]")
        );
    }

    #[test]
    fn env_diff_removes_inherited_gateway_use_for_first_party_only() {
        let env = session_env(&first_party_profile("https://x.net", "tok"), None);
        let diff = env_diff(&env);
        assert!(diff.remove.contains(&"CLAUDE_CODE_USE_GATEWAY"));
        assert!(diff.remove.contains(&"ANTHROPIC_API_KEY"));
        // A user's own NONESSENTIAL_TRAFFIC setting is not touched.
        assert!(!diff
            .remove
            .contains(&"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"));
    }

    #[test]
    fn env_diff_keeps_gateway_vars_for_gateway_envs() {
        let env = session_env(&profile("https://x.net", "tok"), None);
        let diff = env_diff(&env);
        assert!(!diff.remove.contains(&"CLAUDE_CODE_USE_GATEWAY"));
        assert!(!diff
            .remove
            .contains(&"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"));
    }
}
