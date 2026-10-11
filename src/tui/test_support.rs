//! Fixtures shared by the TUI's unit tests.

use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

use super::app::{App, AppConfig, Mode};
use super::keymap::Keymap;
use super::state::ClientState;
use crate::proto::{SessionInfo, SessionKind, SessionState};

/// A manager on a 100×30 terminal with home `/home/u`, the default keys, and
/// nothing from the machine it runs on: no proxy profiles, no SSH hosts, no
/// saved state, no local claude.
pub fn config() -> AppConfig {
    AppConfig {
        mode: Mode::Manager,
        size: (100, 30),
        home: "/home/u".to_owned(),
        keymap: Keymap::default(),
        notify: true,
        copy_on_select: true,
        proxy_override: Default::default(),
        proxy_profiles: Vec::new(),
        proxy_default: None,
        claude: Default::default(),
        local_claude: None,
        recent_dirs: HashMap::new(),
        claude_skipped: HashMap::new(),
        ssh_hosts: Vec::new(),
    }
}

/// An app made from [`config`].
pub fn app() -> App {
    App::new(config())
}

/// An app that recovered `live` from the local daemon (and nothing from
/// state.json), with the effects of that drained. Without sessions, the
/// wizard is open.
pub fn app_with(live: &[SessionInfo]) -> App {
    let mut app = app();
    app.recover(&ClientState::default(), live);
    app.take_effects();
    app
}

/// An idle claude session in `/srv/app`, as the local daemon lists it:
/// running as `pid` (`None`: dormant), in conversation `csid`.
pub fn info(pid: Option<u32>, csid: Option<&str>) -> SessionInfo {
    SessionInfo {
        id: Uuid::new_v4(),
        cwd: "/srv/app".into(),
        name: None,
        state: SessionState::Idle,
        claude_session_id: csid.map(Into::into),
        title: None,
        pid,
        created_at: 1,
        branch: None,
        model: None,
        context_tokens: None,
        kind: SessionKind::Claude,
        ephemeral: false,
    }
}

/// A terminal tab in `/srv/app`.
pub fn shell_info(pid: Option<u32>) -> SessionInfo {
    SessionInfo {
        kind: SessionKind::Shell,
        ..info(pid, None)
    }
}

/// A key press with `mods`, as the terminal reports it.
pub fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, mods))
}

/// A key press without modifiers.
pub fn press(code: KeyCode) -> Event {
    key(code, KeyModifiers::NONE)
}

/// Alt+`c`.
pub fn alt(c: char) -> Event {
    key(KeyCode::Char(c), KeyModifiers::ALT)
}
