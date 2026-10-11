//! Global configuration from `~/.config/claudio/config.toml`.
//!
//! Only the sections claudio itself defines are parsed here; the proxy section
//! lives in [`crate::proxy::profile`], which reads the same file. Unknown
//! sections are preserved on write via a `flatten` map.
//!
//! ```toml
//! [ui]
//! notify = true          # desktop notifications for background sessions
//! copy_on_select = true  # drag in a pane to select and copy (OSC 52)
//!
//! [claude]
//! allow_skip_permissions = true   # pass --allow-dangerously-skip-permissions
//! update_check = "ask"            # local claude vs its release channel
//! remote_check = "ask"            # remote claude vs the local version
//!
//! [keys]
//! overview    = "alt+g"
//! help        = "alt+h"
//! next_session = "alt+right"
//! # …any action from keymap::Action
//! ```

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::paths;

// ── Data model ────────────────────────────────────────────────────────────────

/// The `[ui]` section of `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSection {
    /// Whether to emit desktop notifications (OSC 9 + BEL) when a background
    /// session enters NeedsApproval / NeedsInput / Error.
    #[serde(default = "default_notify")]
    pub notify: bool,
    /// Copy on select: a left-button drag in a session pane selects text
    /// (double-click a word, triple-click a row) and copies it to the
    /// clipboard through OSC 52. Only while the session has not turned on
    /// mouse reporting; those clicks go to the session as before.
    #[serde(default = "default_true")]
    pub copy_on_select: bool,
}

fn default_notify() -> bool {
    true
}

impl Default for UiSection {
    fn default() -> Self {
        UiSection {
            notify: true,
            copy_on_select: true,
        }
    }
}

/// The `[update]` section of `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSection {
    /// Whether to check for newer releases in the background (at most once per
    /// 24 h).  Set to `false` or export `CLAUDIO_NO_UPDATE_CHECK=1` to opt out.
    #[serde(default = "default_update_check")]
    pub check: bool,
}

fn default_update_check() -> bool {
    true
}

impl Default for UpdateSection {
    fn default() -> Self {
        UpdateSection { check: true }
    }
}

/// What to do when a claude install is out of date.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdatePolicy {
    /// Ask before updating.
    #[default]
    Ask,
    /// Update without asking (a notice says so).
    Auto,
    /// Never check.
    Off,
}

/// The `[claude]` section of `config.toml`: how claudio launches and maintains
/// the claude CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeSection {
    /// Start claude with `--allow-dangerously-skip-permissions`, so bypass
    /// mode is selectable with Shift+Tab (it does not start in it).
    #[serde(default = "default_true")]
    pub allow_skip_permissions: bool,
    /// Compare the local claude with its release channel (once a day).
    #[serde(default)]
    pub update_check: UpdatePolicy,
    /// Compare a remote host's claude with the local version on connect.
    #[serde(default)]
    pub remote_check: UpdatePolicy,
}

fn default_true() -> bool {
    true
}

impl Default for ClaudeSection {
    fn default() -> Self {
        ClaudeSection {
            allow_skip_permissions: true,
            update_check: UpdatePolicy::Ask,
            remote_check: UpdatePolicy::Ask,
        }
    }
}

/// The `[keys]` section of `config.toml`: action name → key spec string.
///
/// Each entry overrides the default binding for that action. Unknown action
/// names and invalid key specs are ignored with a notice.
pub type KeysSection = BTreeMap<String, String>;

/// The subset of `config.toml` that this module owns. Other sections (e.g.
/// `[proxy]`) are preserved via `extra` on round-trip.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    pub ui: Option<UiSection>,
    #[serde(default)]
    pub keys: Option<KeysSection>,
    #[serde(default)]
    pub update: Option<UpdateSection>,
    #[serde(default)]
    pub claude: Option<ClaudeSection>,
    /// All other TOML keys — preserved on write.
    #[serde(flatten)]
    extra: toml::Table,
}

/// Loaded claudio configuration.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub ui: UiSection,
    /// Raw key overrides (action name → key spec). Parsed into bindings by
    /// `keymap::apply_overrides`.
    pub keys: KeysSection,
    pub update: UpdateSection,
    pub claude: ClaudeSection,
}

// ── Loading ───────────────────────────────────────────────────────────────────

/// Load the configuration from the default `config.toml`. A missing file is
/// not an error; an unreadable or invalid file is logged to stderr and
/// defaults are used. Parse errors do NOT include source excerpts (which may
/// contain secrets from the `[proxy]` section).
pub fn load() -> Config {
    match try_load_from(&paths::config_file()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("claudio: config.toml: {e} (using defaults)");
            Config::default()
        }
    }
}

/// Load configuration from an explicit path (used in tests to avoid mutating
/// the global `XDG_CONFIG_HOME`).
#[allow(dead_code)]
pub fn load_from(path: &Path) -> Config {
    match try_load_from(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("claudio: config.toml: {e} (using defaults)");
            Config::default()
        }
    }
}

fn try_load_from(path: &Path) -> io::Result<Config> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e),
    };
    let file: ConfigFile = toml::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| {
        // Keep only the first line of the TOML error (location), never the source
        // excerpt which could contain a token from the [proxy] section.
        let loc = e
            .to_string()
            .lines()
            .next()
            .unwrap_or("TOML parse error")
            .to_owned();
        io::Error::other(format!("{loc} (content redacted)"))
    })?;
    Ok(Config {
        ui: file.ui.unwrap_or_default(),
        keys: file.keys.unwrap_or_default(),
        update: file.update.unwrap_or_default(),
        claude: file.claude.unwrap_or_default(),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses load_from with an explicit temp dir to avoid mutating XDG_CONFIG_HOME.
    #[test]
    fn missing_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("claudio-cfg-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml"); // does not exist
        let cfg = load_from(&path);
        assert!(cfg.ui.notify);
        assert!(cfg.ui.copy_on_select);
        assert!(cfg.keys.is_empty());
        assert!(cfg.claude.allow_skip_permissions);
        assert_eq!(cfg.claude.update_check, UpdatePolicy::Ask);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_claude_section() {
        let dir = std::env::temp_dir().join(format!("claudio-cfg-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[claude]\nallow_skip_permissions = false\nupdate_check = \"auto\"\nremote_check = \"off\"\n",
        )
        .unwrap();
        let cfg = load_from(&path);
        assert!(!cfg.claude.allow_skip_permissions);
        assert_eq!(cfg.claude.update_check, UpdatePolicy::Auto);
        assert_eq!(cfg.claude.remote_check, UpdatePolicy::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_ui_and_keys_sections() {
        let toml = r#"
[ui]
notify = false
copy_on_select = false

[keys]
overview = "alt+g"
help = "alt+h"

[proxy]
default = "x"
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        let ui = file.ui.unwrap();
        assert!(!ui.notify);
        assert!(!ui.copy_on_select);
        let keys = file.keys.unwrap();
        assert_eq!(keys.get("overview").map(String::as_str), Some("alt+g"));
        assert_eq!(keys.get("help").map(String::as_str), Some("alt+h"));
        // [proxy] survives in extra.
        assert!(file.extra.contains_key("proxy"));
    }

    /// TOML errors must not embed source content (which may contain
    /// a token from the [proxy] section).
    #[test]
    fn toml_error_does_not_include_source_content() {
        let dir =
            std::env::temp_dir().join(format!("claudio-cfg-toml-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Malformed TOML that would expose "MYVERYSECRETTOKEN" in a naive error.
        let bad = "[proxy.profiles.x]\ntoken = MYVERYSECRETTOKEN_UNQUOTED\n";
        std::fs::write(&path, bad).unwrap();

        let cfg = load_from(&path); // falls back to defaults, logs to stderr
                                    // The config returned is defaults (no panic).
        assert!(cfg.ui.notify);
        // We can't easily capture stderr here, but we verify the
        // try_load_from error message via direct call.
        let err = try_load_from(&path).unwrap_err();
        assert!(
            !err.to_string().contains("MYVERYSECRETTOKEN_UNQUOTED"),
            "error leaked secret: {}",
            err
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
