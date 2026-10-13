//! Choosing and resolving the proxy profile for a new claude session.
//!
//! Shared by the manager (new-session wizard, spawn) and `claudio --plain`.

use super::api::ConfigResponse;
use super::profile::{self, Profile};

/// How a launcher should apply a proxy to new sessions.
///
/// Resolved once at startup from `--proxy`/`--no-proxy` and used as the
/// pre-selected proxy.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ProxyChoice {
    /// Use the profile named in `config.toml [proxy] default` (the default).
    #[default]
    Default,
    /// Explicitly no proxy, even when a default is configured (--no-proxy).
    Direct,
    /// Use a specific named profile (--proxy <name>).
    Profile(String),
}

impl ProxyChoice {
    /// The profile name this choice selects, given the configured `default`.
    pub fn pick<'a>(&'a self, default: Option<&'a str>) -> Option<&'a str> {
        match self {
            ProxyChoice::Default => default,
            ProxyChoice::Direct => None,
            ProxyChoice::Profile(name) => Some(name),
        }
    }
}

/// The launch flags that the session manager and `--plain` take before
/// anything else: the proxy choice and `--first-party`.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchFlags<'a> {
    pub choice: ProxyChoice,
    /// `--first-party`: every session of this run is first-party.
    pub first_party: bool,
    /// The arguments after the flags.
    pub rest: &'a [String],
}

/// Consume the leading `--proxy NAME`, `--no-proxy` and `--first-party` flags
/// (in any order). Stops at the first other argument.
pub fn split_launch_flags(args: &[String]) -> Result<LaunchFlags<'_>, String> {
    let mut flags = LaunchFlags {
        choice: ProxyChoice::Default,
        first_party: false,
        rest: args,
    };
    loop {
        match flags.rest {
            [flag, ..] if flag == "--first-party" => flags.first_party = true,
            [flag] if flag == "--proxy" => return Err("--proxy requires a profile name".into()),
            [flag, name, ..] if flag == "--proxy" => {
                flags.choice = ProxyChoice::Profile(name.clone());
                flags.rest = &flags.rest[1..];
            }
            [flag, ..] if flag == "--no-proxy" => flags.choice = ProxyChoice::Direct,
            _ => return Ok(flags),
        }
        flags.rest = &flags.rest[1..];
    }
}

/// Load proxy profiles from config.toml and CLAUDIO_PROXY_URL. Returns
/// `(profile_names, default_name)`. Never panics; returns empty on any error.
pub fn load_proxy_profiles() -> (Vec<String>, Option<String>) {
    // Check env var first — when set it overrides any saved default.
    if let Some((name, _)) = profile::from_env() {
        // Return just the "env" profile, which is always the default.
        return (vec![name.to_owned()], Some(name.to_owned()));
    }
    match profile::load() {
        Ok(sec) => {
            let names: Vec<String> = sec.profiles.keys().cloned().collect();
            (names, sec.default)
        }
        Err(_) => (Vec::new(), None),
    }
}

/// Look up a profile by name. Handles `"env"` (CLAUDIO_PROXY_URL) and named
/// profiles; `None` if it cannot be found.
pub fn find_profile(name: &str) -> Option<Profile> {
    if name == "env" {
        profile::from_env().map(|(_, p)| p)
    } else {
        profile::load()
            .ok()
            .and_then(|sec| sec.profiles.get(name).cloned())
    }
}

/// Resolve a proxy profile by name to `(url, token)`.
pub fn resolve_profile(name: &str) -> Option<(String, String)> {
    find_profile(name).map(|p| (p.url, p.token))
}

/// Build the `SpawnSpec.env` for a proxy profile name.
///
/// Returns `Err(msg)` if a profile name is given but cannot be resolved:
/// - deleted profile
/// - `"env"` profile but `CLAUDIO_PROXY_URL` is unset
///
/// Returns `Ok(vec![])` when `proxy_name` is `None`. `first_party` (the
/// `--first-party` flag) makes the session first-party whatever the profile
/// says.
pub fn proxy_env_for(
    proxy_name: Option<&str>,
    proxy_config: Option<&ConfigResponse>,
    first_party: bool,
) -> Result<Vec<(String, String)>, String> {
    let Some(name) = proxy_name else {
        return Ok(Vec::new());
    };
    match find_profile(name) {
        Some(mut p) => {
            p.first_party |= first_party;
            Ok(super::env::session_env(&p, proxy_config))
        }
        None if name == "env" => {
            Err("proxy profile 'env' requires CLAUDIO_PROXY_URL to be set".to_owned())
        }
        None => Err(format!(
            "proxy profile '{name}' not found (was it deleted?)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_picks_default_none_or_named() {
        assert_eq!(ProxyChoice::Default.pick(Some("work")), Some("work"));
        assert_eq!(ProxyChoice::Default.pick(None), None);
        assert_eq!(ProxyChoice::Direct.pick(Some("work")), None);
        assert_eq!(
            ProxyChoice::Profile("lab".into()).pick(Some("work")),
            Some("lab")
        );
    }

    #[test]
    fn no_proxy_name_means_empty_env() {
        assert_eq!(proxy_env_for(None, None, false), Ok(Vec::new()));
        assert_eq!(proxy_env_for(None, None, true), Ok(Vec::new()));
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn launch_flags_are_consumed_in_any_order() {
        let a = args(&["--first-party", "--proxy", "work", "-c"]);
        let flags = split_launch_flags(&a).unwrap();
        assert_eq!(flags.choice, ProxyChoice::Profile("work".into()));
        assert!(flags.first_party);
        assert_eq!(flags.rest, &args(&["-c"])[..]);

        let a = args(&["--no-proxy", "--first-party"]);
        let flags = split_launch_flags(&a).unwrap();
        assert_eq!(flags.choice, ProxyChoice::Direct);
        assert!(flags.first_party);
        assert!(flags.rest.is_empty());
    }

    #[test]
    fn launch_flags_default_when_absent() {
        let a = args(&["--resume", "abc"]);
        let flags = split_launch_flags(&a).unwrap();
        assert_eq!(flags.choice, ProxyChoice::Default);
        assert!(!flags.first_party);
        assert_eq!(flags.rest, &a[..]);
    }

    #[test]
    fn proxy_flag_needs_a_name() {
        assert!(split_launch_flags(&args(&["--proxy"])).is_err());
        assert!(split_launch_flags(&args(&["--first-party", "--proxy"])).is_err());
    }
}
