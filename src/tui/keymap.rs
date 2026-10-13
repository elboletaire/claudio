//! Manager key bindings.
//!
//! Every key the manager intercepts (instead of forwarding to claude) is
//! defined in [`DEFAULT_BINDINGS`], which is the single source of truth for
//! both dispatch (`lookup`) and the generated help popup.
//!
//! Overrides from `config.toml` `[keys]` are applied at startup by
//! [`init`], which replaces the action's binding. Unknown action names,
//! unparseable key specs, and collisions produce a notice and are ignored.
//!
//! The effective table is stored in `App` as a `Keymap` value, so status-bar
//! hints always reflect overrides.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Something the manager does instead of forwarding a key to claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PrevSession,
    NextSession,
    NewSession,
    Rename,
    Close,
    /// Jump to the next session that wants attention (cycles).
    NextAttention,
    /// Leave the UI; sessions keep running in the daemon.
    Quit,
    /// Open the proxy stats popup for the active session.
    ProxyStats,
    /// Open the overview ("mission control") popup.
    Overview,
    /// Open the help popup.
    Help,
    /// Open a terminal tab next to the active session.
    Terminal,
    /// Browse the commit history of the active session's directory.
    GitLog,
    /// Restart the active session in its tab (asks how).
    Reset,
    /// Show/hide dot-directories in the new-session wizard (wizard scope).
    ToggleHidden,
    /// Jump to tab `n` (`Alt+Shift+1`…`9` → 1…9, `Alt+Shift+0` → 10).
    /// Fixed: not in [`DEFAULT_BINDINGS`], so `[keys]` can't override it.
    GotoSession(u8),
}

impl Action {
    /// The action's canonical name (matches `config.toml` `[keys]` action names).
    pub fn name(self) -> &'static str {
        match self {
            Action::PrevSession => "prev_session",
            Action::NextSession => "next_session",
            Action::NewSession => "new_session",
            Action::Rename => "rename",
            Action::Close => "close",
            Action::NextAttention => "next_attention",
            Action::Quit => "quit",
            Action::ProxyStats => "proxy_stats",
            Action::Overview => "overview",
            Action::Help => "help",
            Action::Terminal => "terminal",
            Action::GitLog => "git_log",
            Action::Reset => "reset",
            Action::ToggleHidden => "toggle_hidden",
            Action::GotoSession(_) => "goto_session",
        }
    }
}

/// Where a binding is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Intercepted anywhere in the manager (never forwarded to claude).
    Global,
    /// Only consulted while the new-session wizard is open.
    Wizard,
}

/// A single binding in the effective table: key + description + action.
#[derive(Debug, Clone)]
pub struct Binding {
    pub code: KeyCode,
    pub mods: KeyModifiers,
    /// Short description shown in the help popup.
    pub label: &'static str,
    pub action: Action,
    pub scope: Scope,
}

/// The default binding table. The help popup is generated from this, so it
/// can never drift from what the manager actually does.
pub const DEFAULT_BINDINGS: &[Binding] = &[
    Binding {
        code: KeyCode::Left,
        mods: KeyModifiers::ALT,
        label: "previous session",
        action: Action::PrevSession,
        scope: Scope::Global,
    },
    Binding {
        code: KeyCode::Right,
        mods: KeyModifiers::ALT,
        label: "next session",
        action: Action::NextSession,
        scope: Scope::Global,
    },
    Binding {
        code: KeyCode::Char('n'),
        mods: KeyModifiers::ALT,
        label: "new session (wizard)",
        action: Action::NewSession,
        scope: Scope::Global,
    },
    Binding {
        code: KeyCode::Char('r'),
        mods: KeyModifiers::ALT,
        label: "rename session",
        action: Action::Rename,
        scope: Scope::Global,
    },
    Binding {
        code: KeyCode::Char('x'),
        mods: KeyModifiers::ALT,
        label: "close / kill session",
        action: Action::Close,
        scope: Scope::Global,
    },
    Binding {
        code: KeyCode::Char('a'),
        mods: KeyModifiers::ALT,
        label: "jump to next session needing attention",
        action: Action::NextAttention,
        scope: Scope::Global,
    },
    // Alt+s: proxy stats. Verified not used by claude (bundle grep: empty).
    Binding {
        code: KeyCode::Char('s'),
        mods: KeyModifiers::ALT,
        label: "proxy stats popup",
        action: Action::ProxyStats,
        scope: Scope::Global,
    },
    // Alt+g: "glance" / overview. Verified free: not in claude bundle,
    // not a readline binding, not a Ghostty default.
    Binding {
        code: KeyCode::Char('g'),
        mods: KeyModifiers::ALT,
        label: "overview of all sessions",
        action: Action::Overview,
        scope: Scope::Global,
    },
    // Alt+h: help. Verified free: not in claude bundle, not in Ghostty defaults.
    Binding {
        code: KeyCode::Char('h'),
        mods: KeyModifiers::ALT,
        label: "this help popup",
        action: Action::Help,
        scope: Scope::Global,
    },
    // Alt+c: terminal tab. Verified free in Claude Code 2.1.280 (Alt+t is its
    // thinking toggle, hence not t). Inside terminal tabs this shadows
    // readline's Alt+c (capitalize-word), like every other manager key.
    Binding {
        code: KeyCode::Char('c'),
        mods: KeyModifiers::ALT,
        label: "new terminal (shell) next to this tab",
        action: Action::Terminal,
        scope: Scope::Global,
    },
    // Alt+l: commit history. Verified free in Claude Code 2.1.280; readline's
    // Alt+l (downcase-word) is shadowed in terminal tabs.
    Binding {
        code: KeyCode::Char('l'),
        mods: KeyModifiers::ALT,
        label: "git history of the session's directory",
        action: Action::GitLog,
        scope: Scope::Global,
    },
    // Alt+e: restart ("e" for "restart"; r is rename). Verified free in Claude
    // Code 2.1.280: it uses alt+b/d/f/y for readline word ops and o/p/t/w/j/m/k/v.
    Binding {
        code: KeyCode::Char('e'),
        mods: KeyModifiers::ALT,
        label: "restart / reset this session",
        action: Action::Reset,
        scope: Scope::Global,
    },
    // Alt+.: show/hide dot-directories in the wizard's directory step. Wizard
    // scoped: only consulted while the wizard is open, so it is never swallowed
    // from claude. Plain letters can't be used: they type into the search box.
    Binding {
        code: KeyCode::Char('.'),
        mods: KeyModifiers::ALT,
        label: "wizard: show/hide hidden dirs",
        action: Action::ToggleHidden,
        scope: Scope::Wizard,
    },
    // Quit is last so it's always visible in the status bar even when truncated.
    Binding {
        code: KeyCode::Char('q'),
        mods: KeyModifiers::ALT,
        label: "quit",
        action: Action::Quit,
        scope: Scope::Global,
    },
];

/// Help-popup row for the fixed `Alt+Shift+<digit>` bindings (see
/// [`goto_session`]); these are not in [`DEFAULT_BINDINGS`].
const GOTO_SESSION_HELP: (&str, &str) = ("Alt+Shift+1…9,0", "go to session 1…10");

/// `Alt+Shift+<digit>` → [`Action::GotoSession`].
///
/// With the kitty keyboard protocol (enabled in `tui/mod.rs`), terminals send
/// the base key plus modifiers, so crossterm reports `Char('1')` with
/// `ALT | SHIFT` on any layout. Without the protocol the terminal sends the
/// layout-dependent shifted symbol (`!`, `"`…), which we deliberately don't map.
fn goto_session(key: &KeyEvent) -> Option<Action> {
    if key.modifiers != (KeyModifiers::ALT | KeyModifiers::SHIFT) {
        return None;
    }
    match key.code {
        KeyCode::Char(c @ '0'..='9') => Some(Action::GotoSession(c as u8 - b'0')),
        _ => None,
    }
}

/// The only actions `claudio --plain` keeps, in help order. Everything else
/// (tabs, wizard, rename, quit, ...) belongs to the manager and goes to claude.
pub const PLAIN_ACTIONS: [Action; 4] = [
    Action::Help,
    Action::ProxyStats,
    Action::GitLog,
    Action::Reset,
];

/// The effective keymap: defaults + config overrides.
///
/// Owned by `App`; used for dispatch, help rendering, and status-bar hints.
/// Build with [`Keymap::build`] (the manager) or [`Keymap::build_plain`].
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<Binding>,
    /// Whether `Alt+Shift+<digit>` jumps to a tab (the manager only).
    goto_tabs: bool,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::build(&std::collections::BTreeMap::new(), &mut Vec::new())
    }
}

impl Keymap {
    /// Build from defaults + overrides. Collect notices for any problem.
    pub fn build(
        overrides: &std::collections::BTreeMap<String, String>,
        notices: &mut Vec<String>,
    ) -> Keymap {
        Keymap::build_with(overrides, notices, |_| true, true)
    }

    /// The keymap of `claudio --plain`: only [`PLAIN_ACTIONS`] (in that order)
    /// act, and the rest of the keyboard goes to claude. `[keys]` overrides of
    /// those actions apply as in the manager; overrides of the manager-only
    /// actions are ignored without a notice, since they stay valid for the
    /// manager.
    pub fn build_plain(
        overrides: &std::collections::BTreeMap<String, String>,
        notices: &mut Vec<String>,
    ) -> Keymap {
        let mut km = Keymap::build_with(overrides, notices, |a| PLAIN_ACTIONS.contains(&a), false);
        km.bindings
            .sort_by_key(|b| PLAIN_ACTIONS.iter().position(|a| *a == b.action));
        km
    }

    /// Defaults restricted to the actions `keep` accepts, plus overrides.
    fn build_with(
        overrides: &std::collections::BTreeMap<String, String>,
        notices: &mut Vec<String>,
        keep: impl Fn(Action) -> bool,
        goto_tabs: bool,
    ) -> Keymap {
        // Start from defaults (clone the static slice into an owned Vec).
        let mut bindings: Vec<Binding> = DEFAULT_BINDINGS
            .iter()
            .filter(|b| keep(b.action))
            .map(|b| Binding {
                code: b.code,
                mods: b.mods,
                label: b.label,
                action: b.action,
                scope: b.scope,
            })
            .collect();

        for (name, spec) in overrides {
            // Find the action by name.
            let action = DEFAULT_BINDINGS
                .iter()
                .find(|b| b.action.name() == name)
                .map(|b| b.action);
            let Some(action) = action else {
                notices.push(format!("config.toml [keys]: unknown action '{name}'"));
                continue;
            };
            if !keep(action) {
                continue;
            }
            let (code, mods) = match parse_key_spec(spec) {
                Ok(km) => km,
                Err(e) => {
                    notices.push(format!("config.toml [keys] '{name}': {e}"));
                    continue;
                }
            };
            // Collision check: reject if another action already claims this key.
            if let Some(existing) = bindings
                .iter()
                .find(|b| b.code == code && b.mods == mods && b.action != action)
            {
                notices.push(format!(
                    "config.toml [keys] '{name}': key {} already bound to '{}', skipping",
                    key_str(code, mods),
                    existing.action.name()
                ));
                continue;
            }
            // Replace the existing binding for this action.
            if let Some(b) = bindings.iter_mut().find(|b| b.action == action) {
                b.code = code;
                b.mods = mods;
            }
        }
        Keymap { bindings, goto_tabs }
    }

    /// The global action bound to `key`, if any. Releases never trigger actions.
    pub fn lookup(&self, key: &KeyEvent) -> Option<Action> {
        self.lookup_scope(key, Scope::Global)
            .or_else(|| {
                goto_session(key).filter(|_| self.goto_tabs && key.kind != KeyEventKind::Release)
            })
    }

    /// The wizard-scoped action bound to `key`, if any.
    pub fn lookup_wizard(&self, key: &KeyEvent) -> Option<Action> {
        self.lookup_scope(key, Scope::Wizard)
    }

    fn lookup_scope(&self, key: &KeyEvent, scope: Scope) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let (code, mods) = canonical_key(key.code, key.modifiers);
        self.bindings
            .iter()
            .find(|b| b.scope == scope && b.code == code && b.mods == mods)
            .map(|b| b.action)
    }

    /// The human-readable key (`"Alt+."`) currently bound to `action`.
    pub fn key_for(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|b| b.action == action)
            .map(|b| key_str(b.code, b.mods))
    }

    /// Generate the help lines from the current effective bindings.
    /// Each entry is `(key_str, label)`.
    pub fn help_entries(&self) -> Vec<(String, &'static str)> {
        let mut entries: Vec<_> = self
            .bindings
            .iter()
            .map(|b| (key_str(b.code, b.mods), b.label))
            .collect();
        if self.goto_tabs {
            // The fixed Alt+Shift+digit row sits right after "next session".
            let at = self
                .bindings
                .iter()
                .position(|b| b.action == Action::NextSession)
                .map_or(entries.len(), |i| i + 1);
            entries.insert(at, (GOTO_SESSION_HELP.0.to_owned(), GOTO_SESSION_HELP.1));
        }
        entries
    }

    /// Generate the status-bar hints line from the current effective bindings.
    ///
    /// Reflects overrides so the hint is always accurate.
    #[allow(dead_code)]
    pub fn hints(&self) -> String {
        let parts: Vec<String> = self
            .bindings
            .iter()
            .map(|b| format!("{} {}", key_str(b.code, b.mods), b.label))
            .collect();
        parts.join(" · ")
    }
}

/// Parse a key spec string like `"alt+right"`, `"alt+g"`, `"ctrl+x"`.
///
/// Supported modifiers: `alt`, `ctrl`, `shift`. Supported keys: letter/digit
/// chars, `left`, `right`, `up`, `down`, `enter`, `esc`, `tab`, `backtab`,
/// `backspace`, `delete`, `home`, `end`, `pageup`, `pagedown`, `f1`–`f12`.
///
/// Multiple modifiers can be combined: `"alt+ctrl+x"`.
///
/// `shift+tab` is canonicalized to `KeyCode::BackTab`.
/// `alt+shift+<letter>` is canonicalized to the uppercase letter.
pub fn parse_key_spec(spec: &str) -> Result<(KeyCode, KeyModifiers), String> {
    let spec_lc = spec.trim().to_ascii_lowercase();
    let mut parts: Vec<&str> = spec_lc.split('+').collect();
    if parts.is_empty() {
        return Err("empty key spec".into());
    }
    let key_str_lc = parts.pop().unwrap();
    let mut mods = KeyModifiers::NONE;
    for m in &parts {
        match *m {
            "alt" | "meta" => mods |= KeyModifiers::ALT,
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "shift" => mods |= KeyModifiers::SHIFT,
            other => return Err(format!("unknown modifier '{other}'")),
        }
    }
    // Canonicalize shift+tab → BackTab.
    if key_str_lc == "tab" && mods.contains(KeyModifiers::SHIFT) {
        return Ok((KeyCode::BackTab, mods - KeyModifiers::SHIFT));
    }
    let code = match key_str_lc {
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        "/" => KeyCode::Char('/'),
        "." => KeyCode::Char('.'),
        "," => KeyCode::Char(','),
        ";" => KeyCode::Char(';'),
        "'" => KeyCode::Char('\''),
        s if s.len() == 1 => {
            let c = s.chars().next().unwrap();
            if c.is_ascii_alphanumeric() || c.is_ascii_punctuation() {
                // alt+shift+<letter> → uppercase letter with ALT only.
                if mods.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                    let uc = c.to_ascii_uppercase();
                    return Ok((KeyCode::Char(uc), mods - KeyModifiers::SHIFT));
                }
                KeyCode::Char(c)
            } else {
                return Err(format!("unsupported key character '{c}'"));
            }
        }
        s if s.starts_with('f') => {
            let n: u8 = s[1..].parse().map_err(|_| format!("bad F-key '{s}'"))?;
            if n == 0 || n > 12 {
                return Err(format!("F-key number must be 1-12, got {n}"));
            }
            KeyCode::F(n)
        }
        other => return Err(format!("unknown key '{other}'")),
    };
    Ok((code, mods))
}

/// Fold Shift+letter into the uppercase letter, the form [`parse_key_spec`]
/// stores. Terminals report Alt+Shift+q as `Char('Q')` with `ALT | SHIFT`
/// (legacy encoding) or `Char('q')` with `ALT | SHIFT` (kitty protocol).
fn canonical_key(code: KeyCode, mods: KeyModifiers) -> (KeyCode, KeyModifiers) {
    match code {
        KeyCode::Char(c) if mods.contains(KeyModifiers::SHIFT) && c.is_ascii_alphabetic() => {
            (KeyCode::Char(c.to_ascii_uppercase()), mods - KeyModifiers::SHIFT)
        }
        _ => (code, mods),
    }
}

/// Human-readable key string for a binding (e.g. `"Alt+←"`, `"Alt+G"`).
pub fn key_str(code: KeyCode, mods: KeyModifiers) -> String {
    let mut parts = Vec::new();
    if mods.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("Alt");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("Shift");
    }
    let key = match code {
        KeyCode::Left => "←".to_owned(),
        KeyCode::Right => "→".to_owned(),
        KeyCode::Up => "↑".to_owned(),
        KeyCode::Down => "↓".to_owned(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "Shift+Tab".to_owned(),
        KeyCode::Backspace => "Bksp".to_owned(),
        KeyCode::Delete => "Del".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PgUp".to_owned(),
        KeyCode::PageDown => "PgDn".to_owned(),
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        _ => "?".to_owned(),
    };
    parts.push(&key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn default_bindings_match_expected_keys() {
        let km = Keymap::default();
        assert_eq!(
            km.lookup(&key(KeyCode::Left, KeyModifiers::ALT)),
            Some(Action::PrevSession)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Right, KeyModifiers::ALT)),
            Some(Action::NextSession)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Char('q'), KeyModifiers::ALT)),
            Some(Action::Quit)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Char('a'), KeyModifiers::ALT)),
            Some(Action::NextAttention)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Char('g'), KeyModifiers::ALT)),
            Some(Action::Overview)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Char('h'), KeyModifiers::ALT)),
            Some(Action::Help)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Char('e'), KeyModifiers::ALT)),
            Some(Action::Reset)
        );
        assert_eq!(km.key_for(Action::Reset).as_deref(), Some("Alt+e"));
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let km = Keymap::default();
        assert_eq!(
            km.lookup(&key(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::SHIFT)),
            None
        );
        assert_eq!(km.lookup(&key(KeyCode::Left, KeyModifiers::NONE)), None);
        assert_eq!(
            km.lookup(&key(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            km.lookup(&key(
                KeyCode::Char('n'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            )),
            None
        );
    }

    #[test]
    fn releases_are_ignored() {
        let km = Keymap::default();
        let release = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(km.lookup(&release), None);
    }

    // ── parse_key_spec ────────────────────────────────────────────────────────

    #[test]
    fn parse_alt_letter() {
        let (code, mods) = parse_key_spec("alt+g").unwrap();
        assert_eq!(code, KeyCode::Char('g'));
        assert_eq!(mods, KeyModifiers::ALT);
    }

    #[test]
    fn parse_alt_arrow() {
        let (code, mods) = parse_key_spec("alt+right").unwrap();
        assert_eq!(code, KeyCode::Right);
        assert_eq!(mods, KeyModifiers::ALT);
    }

    #[test]
    fn parse_ctrl_letter() {
        let (code, mods) = parse_key_spec("ctrl+x").unwrap();
        assert_eq!(code, KeyCode::Char('x'));
        assert_eq!(mods, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_multiple_modifiers() {
        let (code, mods) = parse_key_spec("alt+shift+enter").unwrap();
        assert_eq!(code, KeyCode::Enter);
        assert_eq!(mods, KeyModifiers::ALT | KeyModifiers::SHIFT);
    }

    #[test]
    fn parse_function_keys() {
        let (code, mods) = parse_key_spec("f5").unwrap();
        assert_eq!(code, KeyCode::F(5));
        assert_eq!(mods, KeyModifiers::NONE);
        let (code, mods) = parse_key_spec("alt+f12").unwrap();
        assert_eq!(code, KeyCode::F(12));
        assert_eq!(mods, KeyModifiers::ALT);
    }

    #[test]
    fn parse_special_keys() {
        assert_eq!(
            parse_key_spec("esc").unwrap(),
            (KeyCode::Esc, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key_spec("enter").unwrap(),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key_spec("tab").unwrap(),
            (KeyCode::Tab, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key_spec("pageup").unwrap(),
            (KeyCode::PageUp, KeyModifiers::NONE)
        );
    }

    #[test]
    fn parse_shift_tab_becomes_backtab() {
        // shift+tab should canonicalize to BackTab (no SHIFT modifier).
        assert_eq!(
            parse_key_spec("shift+tab").unwrap(),
            (KeyCode::BackTab, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key_spec("alt+shift+tab").unwrap(),
            (KeyCode::BackTab, KeyModifiers::ALT)
        );
    }

    #[test]
    fn parse_alt_shift_letter_becomes_uppercase() {
        // alt+shift+g → Alt+G (uppercase, SHIFT removed)
        assert_eq!(
            parse_key_spec("alt+shift+g").unwrap(),
            (KeyCode::Char('G'), KeyModifiers::ALT)
        );
    }

    #[test]
    fn alt_shift_letter_override_matches_terminal_events() {
        let overrides = [("quit".to_owned(), "alt+shift+q".to_owned())].into();
        let mut notices = Vec::new();
        let km = Keymap::build(&overrides, &mut notices);
        assert!(notices.is_empty(), "{notices:?}");
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        // Legacy encoding (ESC Q) and kitty protocol (base key + SHIFT).
        for c in ['Q', 'q'] {
            assert_eq!(
                km.lookup(&key(KeyCode::Char(c), alt_shift)),
                Some(Action::Quit),
                "Char({c:?}) with ALT|SHIFT"
            );
        }
        assert_eq!(km.lookup(&key(KeyCode::Char('q'), KeyModifiers::ALT)), None);
    }

    #[test]
    fn parse_invalid_key_returns_error() {
        assert!(parse_key_spec("").is_err());
        assert!(parse_key_spec("badmod+x").is_err());
        assert!(parse_key_spec("alt+zzz").is_err());
        assert!(parse_key_spec("alt+f0").is_err());
        assert!(parse_key_spec("alt+f13").is_err());
    }

    #[test]
    fn parse_case_insensitive() {
        let (code, mods) = parse_key_spec("ALT+G").unwrap();
        assert_eq!(code, KeyCode::Char('g'));
        assert_eq!(mods, KeyModifiers::ALT);
    }

    // ── Keymap::build ─────────────────────────────────────────────────────────

    #[test]
    fn collision_detection_emits_notice_and_skips() {
        let mut overrides = std::collections::BTreeMap::new();
        // Try to bind "quit" to alt+n, which is already "new_session".
        overrides.insert("quit".to_owned(), "alt+n".to_owned());
        let mut notices = Vec::new();
        let km = Keymap::build(&overrides, &mut notices);
        // Notice should mention the collision.
        assert!(!notices.is_empty(), "expected a collision notice");
        assert!(
            notices[0].contains("already bound"),
            "notice should mention collision: {:?}",
            notices
        );
        // The override must be rejected: alt+q still quits, alt+n still opens wizard.
        assert_eq!(
            km.lookup(&key(KeyCode::Char('q'), KeyModifiers::ALT)),
            Some(Action::Quit)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Char('n'), KeyModifiers::ALT)),
            Some(Action::NewSession)
        );
    }

    // ── Alt+Shift+digit ───────────────────────────────────────────────────────

    #[test]
    fn alt_shift_digits_go_to_sessions() {
        let km = Keymap::default();
        let mods = KeyModifiers::ALT | KeyModifiers::SHIFT;
        for d in 0..=9u8 {
            let c = char::from(b'0' + d);
            assert_eq!(
                km.lookup(&key(KeyCode::Char(c), mods)),
                Some(Action::GotoSession(d)),
                "Alt+Shift+{c}"
            );
        }
        assert_eq!(Action::GotoSession(1).name(), "goto_session");
    }

    #[test]
    fn plain_alt_digit_and_shift_digit_are_not_intercepted() {
        let km = Keymap::default();
        let digit = KeyCode::Char('1');
        assert_eq!(km.lookup(&key(digit, KeyModifiers::ALT)), None);
        assert_eq!(km.lookup(&key(digit, KeyModifiers::SHIFT)), None);
        assert_eq!(km.lookup(&key(digit, KeyModifiers::NONE)), None);
        // Not by symbol either: the shifted glyph is layout-dependent.
        assert_eq!(
            km.lookup(&key(
                KeyCode::Char('!'),
                KeyModifiers::ALT | KeyModifiers::SHIFT
            )),
            None
        );
        // Other modifiers in the mix do not match.
        assert_eq!(
            km.lookup(&key(
                digit,
                KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL
            )),
            None
        );
    }

    #[test]
    fn goto_session_ignores_releases_and_config_overrides() {
        let km = Keymap::default();
        let release = KeyEvent {
            code: KeyCode::Char('1'),
            modifiers: KeyModifiers::ALT | KeyModifiers::SHIFT,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(km.lookup(&release), None);
        // The action is fixed: `[keys]` can't name it, and that's just a notice.
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("goto_session".to_owned(), "alt+j".to_owned());
        let mut notices = Vec::new();
        let km = Keymap::build(&overrides, &mut notices);
        assert!(notices[0].contains("unknown action 'goto_session'"));
        assert_eq!(km.lookup(&key(KeyCode::Char('j'), KeyModifiers::ALT)), None);
    }

    #[test]
    fn help_lists_goto_session_after_next_session() {
        let entries = Keymap::default().help_entries();
        let at = entries
            .iter()
            .position(|(k, desc)| k == "Alt+Shift+1…9,0" && *desc == "go to session 1…10")
            .expect("goto row in help");
        assert_eq!(entries[at - 1].0, "Alt+→");
    }

    // ── help_entries ──────────────────────────────────────────────────────────

    #[test]
    fn help_entries_covers_all_default_bindings() {
        let km = Keymap::default();
        let entries = km.help_entries();
        // Every default binding must appear in the help.
        for b in DEFAULT_BINDINGS {
            assert!(
                entries.iter().any(|(_, label)| *label == b.label),
                "missing label: {}",
                b.label
            );
        }
    }

    #[test]
    fn toggle_hidden_is_wizard_scoped() {
        let km = Keymap::default();
        let alt_dot = key(KeyCode::Char('.'), KeyModifiers::ALT);
        assert_eq!(km.lookup_wizard(&alt_dot), Some(Action::ToggleHidden));
        // Never intercepted outside the wizard, so claude still receives it.
        assert_eq!(km.lookup(&alt_dot), None);
        assert_eq!(km.key_for(Action::ToggleHidden).as_deref(), Some("Alt+."));
        assert!(km.help_entries().iter().any(|(k, _)| k == "Alt+."));
    }

    // ── Plain keymap ──────────────────────────────────────────────────────────

    fn plain_keymap(overrides: &[(&str, &str)]) -> (Keymap, Vec<String>) {
        let overrides = overrides
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut notices = Vec::new();
        (Keymap::build_plain(&overrides, &mut notices), notices)
    }

    #[test]
    fn plain_keymap_only_has_help_stats_git_and_reset() {
        let (km, notices) = plain_keymap(&[]);
        assert!(notices.is_empty());
        let alt = |c| key(KeyCode::Char(c), KeyModifiers::ALT);
        assert_eq!(km.lookup(&alt('h')), Some(Action::Help));
        assert_eq!(km.lookup(&alt('s')), Some(Action::ProxyStats));
        assert_eq!(km.lookup(&alt('l')), Some(Action::GitLog));
        assert_eq!(km.lookup(&alt('e')), Some(Action::Reset));
        // Every other manager key goes to claude.
        for c in ['n', 'x', 'r', 'g', 'c', 'a', 'q'] {
            assert_eq!(km.lookup(&alt(c)), None, "Alt+{c}");
        }
        for code in [KeyCode::Left, KeyCode::Right] {
            assert_eq!(km.lookup(&key(code, KeyModifiers::ALT)), None);
        }
        for d in '0'..='9' {
            let mods = KeyModifiers::ALT | KeyModifiers::SHIFT;
            assert_eq!(km.lookup(&key(KeyCode::Char(d), mods)), None, "Alt+Shift+{d}");
        }
        // The wizard key is not part of it either.
        assert_eq!(km.lookup_wizard(&alt('.')), None);
    }

    #[test]
    fn plain_help_lists_exactly_the_plain_keys_in_order() {
        let (km, _) = plain_keymap(&[]);
        let keys: Vec<String> = km.help_entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["Alt+h", "Alt+s", "Alt+l", "Alt+e"]);
    }

    #[test]
    fn plain_overrides_apply_and_manager_only_ones_are_ignored_quietly() {
        let (km, notices) = plain_keymap(&[
            ("proxy_stats", "alt+p"),
            ("rename", "alt+j"),
            ("bogus", "alt+k"),
        ]);
        assert_eq!(
            km.lookup(&key(KeyCode::Char('p'), KeyModifiers::ALT)),
            Some(Action::ProxyStats)
        );
        assert_eq!(km.lookup(&key(KeyCode::Char('s'), KeyModifiers::ALT)), None);
        // Valid for the manager, so no notice and no effect; unknown still says so.
        assert_eq!(km.lookup(&key(KeyCode::Char('j'), KeyModifiers::ALT)), None);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("unknown action 'bogus'"));
    }

    #[test]
    fn plain_override_cannot_steal_another_plain_key() {
        let (km, notices) = plain_keymap(&[("git_log", "alt+h")]);
        assert!(notices[0].contains("already bound"), "{notices:?}");
        assert_eq!(
            km.lookup(&key(KeyCode::Char('h'), KeyModifiers::ALT)),
            Some(Action::Help)
        );
    }

    #[test]
    fn hints_string_not_empty() {
        let km = Keymap::default();
        assert!(!km.hints().is_empty());
    }
}
