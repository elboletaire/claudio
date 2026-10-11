//! Session view type, sanitization helpers, and session lifecycle methods.
//!
//! `SessionView` and `sanitize_label` are the shared types. The `impl App`
//! block adds the session lifecycle to `App`: spawning, activation, reset,
//! kill and recovery.

use uuid::Uuid;

use crate::proto::{
    GitStatus, Msg, RespawnSpec, SessionId, SessionInfo, SessionKind, SessionState, ShellSpec,
    SpawnSpec,
};
use crate::term::screen::Screen;

use super::app::{App, Effect, Mode, ReplyTo, PROJECTS_LIMIT};
use super::confirm::{Choice, ConfirmAction, ConfirmPrompt};
use super::interaction::Modal;
use super::state::{self, ClientState, KillTombstone, SavedSession};
use super::wizard::{self, Wizard};

// ── SessionView ───────────────────────────────────────────────────────────────

/// Strip C0 controls (0x00–0x1F), DEL (0x7F), and C1 controls (0x80–0x9F)
/// from `s`, and cap the result at `max_chars` characters. Used for any
/// text that leaves the process boundary (OSC notifications, tab labels
/// written to the outer terminal).
pub fn sanitize_label(s: &str, max_chars: usize) -> String {
    s.chars()
        .filter(|&c| {
            let n = c as u32;
            // Keep printable ASCII and non-C1 Unicode.
            !(n < 0x20 || n == 0x7f || (0x80..=0x9f).contains(&n))
        })
        .take(max_chars)
        .collect()
}

/// The client-side view of one session.
pub struct SessionView {
    pub id: SessionId,
    pub name: Option<String>,
    pub cwd: String,
    pub host: String,
    pub state: SessionState,
    pub title: Option<String>,
    pub claude_session_id: Option<String>,
    pub created_at: u64,
    /// Mirror of the daemon's screen; fed only while attached.
    pub mirror: Screen,
    pub attached: bool,
    /// Proxy profile name (None = no proxy). Only the name, never the token.
    pub proxy: Option<String>,
    /// Git branch of the session's cwd, if any.
    pub branch: Option<String>,
    /// Short human-readable model name from the last assistant turn.
    pub model: Option<String>,
    /// Total input+cache tokens of the last assistant turn.
    pub context_tokens: Option<u64>,
    /// The working tree's state (ahead/behind, dirty counts), if known.
    pub git: Option<GitStatus>,
    /// Output tokens of the whole conversation so far.
    pub output_tokens: Option<u64>,
    /// Prompts sent in the conversation so far.
    pub turns: Option<u32>,
    /// Claude, or a plain terminal tab.
    pub kind: SessionKind,
}

impl SessionView {
    /// A detached view of a session that nothing is known about yet but where
    /// and what it runs. `size` is the pane's `(rows, cols)`.
    pub fn new(
        id: SessionId,
        host: impl Into<String>,
        cwd: impl Into<String>,
        kind: SessionKind,
        size: (u16, u16),
    ) -> SessionView {
        SessionView {
            id,
            name: None,
            cwd: cwd.into(),
            host: host.into(),
            // A shell has no hooks to report its state: it is simply idle.
            state: match kind {
                SessionKind::Claude => SessionState::Starting,
                SessionKind::Shell => SessionState::Idle,
            },
            title: None,
            claude_session_id: None,
            created_at: 0,
            mirror: Screen::new(size.0, size.1),
            attached: false,
            proxy: None,
            branch: None,
            model: None,
            context_tokens: None,
            git: None,
            output_tokens: None,
            turns: None,
            kind,
        }
    }

    /// The view of a session `host`'s daemon reports.
    pub fn from_info(host: &str, info: &SessionInfo, size: (u16, u16)) -> SessionView {
        SessionView {
            name: info.name.clone(),
            state: info.state,
            title: info.title.clone(),
            claude_session_id: info.claude_session_id.clone(),
            created_at: info.created_at,
            branch: info.branch.clone(),
            model: info.model.clone(),
            context_tokens: info.context_tokens,
            ..SessionView::new(info.id, host, info.cwd.clone(), info.kind, size)
        }
    }

    /// The view of a session as state.json remembers it.
    pub fn from_saved(saved: SavedSession, size: (u16, u16)) -> SessionView {
        SessionView {
            name: saved.name,
            claude_session_id: saved.claude_session_id,
            created_at: saved.created_at,
            proxy: saved.proxy,
            ..SessionView::new(saved.id, saved.host, saved.cwd, saved.kind, size)
        }
    }

    /// The tab label: the user's name, else a meaningful claude title (not
    /// "Claude Code"), else the cwd's basename. A terminal is its name, else
    /// `term`; its title is ignored so a shell prompt cannot rename the tab.
    /// The renderer adds the `$` glyph and `@host`.
    /// Sanitized so it is safe to embed in escape sequences.
    pub fn label(&self) -> String {
        let title = match self.kind {
            SessionKind::Claude => self.title.as_ref(),
            SessionKind::Shell => None,
        };
        let raw = self
            .name
            .clone()
            .or_else(|| {
                title
                    .filter(|t| *t != "Claude Code" && !t.is_empty())
                    .cloned()
            })
            .unwrap_or_else(|| match self.kind {
                SessionKind::Claude => self
                    .cwd
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or("/")
                    .to_owned(),
                SessionKind::Shell => "term".to_owned(),
            });
        sanitize_label(&raw, 200)
    }

    pub fn saved(&self) -> SavedSession {
        SavedSession {
            id: self.id,
            name: self.name.clone(),
            cwd: self.cwd.clone(),
            host: self.host.clone(),
            claude_session_id: self.claude_session_id.clone(),
            created_at: self.created_at,
            proxy: self.proxy.clone(),
            kind: self.kind,
        }
    }
}

/// What to start: the part of a spawn that does not depend on the proxy.
pub(super) struct SpawnRequest {
    pub id: SessionId,
    pub kind: SessionKind,
    pub cwd: String,
    pub name: Option<String>,
    /// Extra claude arguments (`--resume …`); ignored for a terminal.
    pub args: Vec<String>,
}

// ── Session lifecycle (impl App) ──────────────────────────────────────────────

impl App {
    // ── Accessors ─────────────────────────────────────────────────────────────

    pub(super) fn index_of(&self, id: SessionId) -> Option<usize> {
        self.sessions.iter().position(|v| v.id == id)
    }

    /// The host of the active session (or "local" when none is active).
    pub(super) fn active_host(&self) -> String {
        self.active_view()
            .map(|v| v.host.clone())
            .unwrap_or_else(|| "local".to_owned())
    }

    /// The host the wizard is currently targeting (or "local").
    pub(super) fn wizard_host(&self) -> String {
        match &self.modal {
            Some(Modal::Wizard(w)) => w.host.clone(),
            _ => "local".to_owned(),
        }
    }

    /// The open wizard's generation (0 if no wizard is open).
    pub(super) fn wizard_generation(&self) -> u64 {
        match &self.modal {
            Some(Modal::Wizard(_)) => self.modal_gen,
            _ => 0,
        }
    }

    /// Ask `host` for its recent projects, to seed the open wizard.
    pub(super) fn request_projects(&mut self, host: &str) {
        let to = ReplyTo::Projects {
            host: host.to_owned(),
            gen: self.wizard_generation(),
        };
        let msg = Msg::RecentProjects {
            limit: PROJECTS_LIMIT,
        };
        self.request(host, msg, to);
    }

    // ── Activate / remove ─────────────────────────────────────────────────────

    /// Make session `i` the active one: detach the old, attach the new.
    pub(super) fn activate(&mut self, i: usize) {
        if i >= self.sessions.len() {
            return;
        }
        if self.active == Some(i) && self.sessions[i].attached {
            return;
        }
        if let Some(old) = self.active.filter(|&old| old != i) {
            if let Some(v) = self.sessions.get_mut(old).filter(|v| v.attached) {
                v.attached = false;
                let id = v.id;
                let host = v.host.clone();
                self.request(&host, Msg::Detach { id }, ReplyTo::Ack("detach"));
            }
        }
        self.active = Some(i);
        self.clear_selection();
        let (rows, cols) = self.pane_size();
        let view = &mut self.sessions[i];
        view.attached = true;
        // Blank until the daemon's `Attached` + snapshot arrive.
        view.mirror = Screen::new(rows, cols);
        let id = view.id;
        let host = view.host.clone();
        self.request(
            &host,
            Msg::Attach { id, rows, cols },
            ReplyTo::Ack("attach"),
        );
        self.redraw = true;
        self.save();
        self.refresh_session_cred(super::proxy_state::SESSION_CRED_MIN_GAP_SECS);
    }

    /// Forget session `i` and activate a neighbour.
    pub(super) fn remove(&mut self, i: usize) {
        if i >= self.sessions.len() {
            return;
        }
        let removed = self.sessions.remove(i);
        self.session_creds.remove(&removed.id);
        match self.active {
            Some(a) if a == i => {
                self.active = None;
                if !self.sessions.is_empty() {
                    self.activate(i.min(self.sessions.len() - 1));
                }
            }
            Some(a) if a > i => self.active = Some(a - 1),
            _ => {}
        }
        if self.sessions.is_empty() && self.modal.is_none() && self.mode == Mode::Manager {
            self.open_wizard();
        }
        self.redraw = true;
        self.save();
    }

    // ── Spawn / wizard ────────────────────────────────────────────────────────

    /// Spawn claude in `cwd` on `host` (optionally resuming) and switch to it.
    pub(super) fn spawn(
        &mut self,
        host: String,
        cwd: String,
        resume: Option<String>,
        proxy: Option<String>,
    ) {
        let args = resume
            .map(|r| vec!["--resume".to_owned(), r])
            .unwrap_or_default();
        state::push_recent(&mut self.recent_dirs, &host, &cwd);
        let at = self.sessions.len();
        self.start(Uuid::new_v4(), host, cwd, SessionKind::Claude, args, proxy, at)
            .unwrap_or_else(|e| self.notify(format!("cannot spawn: {e}")));
    }

    /// Open a terminal on the active session's host, in its launch directory
    /// (the local home when nothing is active), right after the active tab.
    pub(super) fn open_terminal(&mut self) {
        let (host, cwd) = match self.active_view() {
            Some(v) => (v.host.clone(), v.cwd.clone()),
            None => ("local".to_owned(), self.home.clone()),
        };
        let at = self.active.map_or(self.sessions.len(), |a| a + 1);
        self.start(Uuid::new_v4(), host, cwd, SessionKind::Shell, Vec::new(), None, at)
            .unwrap_or_else(|e| self.notify(format!("cannot spawn: {e}")));
    }

    /// Send the spawn of session `id`, add its tab at `at` (never before the
    /// active tab) and switch to it. Fails when the spawn cannot be built
    /// (an unresolvable proxy profile); nothing is added then.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start(
        &mut self,
        id: SessionId,
        host: String,
        cwd: String,
        kind: SessionKind,
        args: Vec<String>,
        proxy: Option<String>,
        at: usize,
    ) -> Result<(), String> {
        let req = SpawnRequest {
            id,
            kind,
            cwd: cwd.clone(),
            name: None,
            args,
        };
        self.send_spawn(&host, req, proxy.as_deref())?;
        let view = SessionView {
            created_at: self.now,
            proxy,
            ..SessionView::new(id, host, cwd, kind, self.pane_size())
        };
        self.sessions.insert(at, view);
        self.activate(at);
        Ok(())
    }

    /// Queue the spawn request for `req` on `host`. The one place that knows
    /// how a spawn is built:
    ///
    /// - a terminal is a `SpawnShell` with no `--resume` and no proxy env;
    /// - claude with a proxy builds its env at once when the profile's config
    ///   is cached. When the cache is cold (first spawn), the effect runner
    ///   fetches it (5 s timeout) first, so the proxy's model/rate-limit
    ///   overrides are applied even on the first session.
    ///
    /// Fails when the proxy profile cannot be resolved.
    pub(super) fn send_spawn(
        &mut self,
        host: &str,
        req: SpawnRequest,
        proxy: Option<&str>,
    ) -> Result<(), String> {
        let (rows, cols) = self.pane_size();
        let SpawnRequest {
            id,
            kind,
            cwd,
            name,
            args,
        } = req;
        if kind == SessionKind::Shell {
            let shell = ShellSpec {
                id,
                cwd,
                name,
                rows,
                cols,
            };
            self.request(host, Msg::SpawnShell(shell), ReplyTo::Spawned(id));
            return Ok(());
        }
        let spec = SpawnSpec {
            id,
            cwd,
            name,
            args,
            env: vec![], // filled by `send_with_proxy`
            rows,
            cols,
            // `--plain`'s session is not one for a manager to show.
            ephemeral: self.mode == Mode::Plain,
        };
        self.send_with_proxy(host, Msg::Spawn(spec), proxy, ReplyTo::Spawned(id))
    }

    /// Send `msg` (a `Spawn` or `Respawn` with an empty env) to `host` with
    /// the env of `proxy`. A cold proxy-config cache goes through the effect
    /// runner, which fetches the config first and answers with
    /// [`ReplyTo::deferred`]. Fails when the profile cannot be resolved.
    fn send_with_proxy(
        &mut self,
        host: &str,
        msg: Msg,
        proxy: Option<&str>,
        to: ReplyTo,
    ) -> Result<(), String> {
        match proxy {
            Some(name) if self.proxy_config_cached(name).is_none() => {
                self.emit(Effect::SpawnWithProxy {
                    host: host.to_owned(),
                    msg,
                    proxy_name: name.to_owned(),
                    to: to.deferred(),
                });
            }
            _ => {
                let msg = msg.with_env(self.proxy_env_for(proxy)?);
                self.request(host, msg, to);
            }
        }
        Ok(())
    }

    // ── Close (Alt+x) ─────────────────────────────────────────────────────────

    /// Ask before killing the active session.
    pub(super) fn ask_kill(&mut self) {
        let Some(v) = self.active_view() else { return };
        let prompt = ConfirmPrompt::new(
            "Close session",
            format!("Kill session {}?", v.label()),
            vec![
                Choice::new('y', "kill", Some(ConfirmAction::Kill(v.id))),
                Choice::new('n', "cancel", None),
            ],
        );
        self.queue_confirm(prompt.ready());
    }

    /// Kill session `id` and close its tab. The tombstone is saved before the
    /// Kill is sent, so a Kill lost on the way is sent again on recovery
    /// instead of the session coming back.
    pub(super) fn kill_session(&mut self, id: SessionId) {
        let Some(i) = self.index_of(id) else { return };
        let host = self.sessions[i].host.clone();
        self.killed.push(KillTombstone {
            host: host.clone(),
            id,
        });
        // Saves: state.json has the tombstone (`to_state` includes `killed`)
        // and no longer the session.
        self.remove(i);
        self.request(&host, Msg::Kill { id }, ReplyTo::Kill(id));
    }

    // ── Reset (Alt+e) ─────────────────────────────────────────────────────────

    /// Ask how to restart the active session.
    pub(super) fn ask_reset(&mut self) {
        let Some(v) = self.active_view() else {
            self.notify("no session to reset");
            return;
        };
        let reset = |fresh| Some(ConfirmAction::Reset { id: v.id, fresh });
        let prompt = match v.kind {
            SessionKind::Claude => ConfirmPrompt::new(
                "Restart session",
                format!("Restart claude in {}?", v.label()),
                vec![
                    Choice::new('r', "restart & resume this conversation", reset(false)),
                    Choice::new('n', "new conversation", reset(true)),
                    Choice::esc("cancel"),
                ],
            ),
            SessionKind::Shell => ConfirmPrompt::new(
                "Restart terminal",
                format!("Restart the shell in {}?", v.label()),
                vec![
                    Choice::new('r', "restart shell", reset(false)),
                    Choice::esc("cancel"),
                ],
            ),
        };
        self.queue_confirm(prompt.ready());
    }

    /// Restart session `id` in place: the daemon kills its process and starts
    /// the same session again (see [`Msg::Respawn`]); the tab stays.
    pub(super) fn reset_session(&mut self, id: SessionId, fresh: bool) {
        let Some(i) = self.index_of(id) else { return };
        let (host, kind, proxy) = {
            let v = &self.sessions[i];
            (v.host.clone(), v.kind, v.proxy.clone())
        };
        let (rows, cols) = self.pane_size();
        let msg = Msg::Respawn(RespawnSpec {
            id,
            fresh,
            env: vec![], // filled by `send_with_proxy`
            rows,
            cols,
        });
        // A terminal never carries the proxy env.
        let proxy = proxy.filter(|_| kind == SessionKind::Claude);
        if let Err(e) = self.send_with_proxy(&host, msg, proxy.as_deref(), ReplyTo::Respawned(id)) {
            self.notify(format!("cannot restart: {e}"));
        }
    }

    /// The daemon's answer to [`App::reset_session`].
    pub(super) fn on_respawned(&mut self, id: SessionId, reply: std::io::Result<Msg>) {
        let Some(i) = self.index_of(id) else { return };
        match reply {
            Ok(_) => {
                // The old process's output is gone: attach to the new one.
                if self.active == Some(i) {
                    self.sessions[i].attached = false;
                    self.activate(i);
                }
            }
            Err(e) => {
                let notice = App::older_daemon_notice(&self.sessions[i].host, &e);
                self.notify(notice.unwrap_or_else(|| format!("could not restart: {e}")));
            }
        }
    }

    pub(super) fn open_wizard(&mut self) {
        let active_proxy = self.active_view().and_then(|v| v.proxy.clone());
        let active_host = self.active_host();
        // The wizard starts on the active host; picking another host seeds
        // it again (see `on_host_connected`).
        let seeds = wizard::assemble(None, &self.wizard_seeds(&active_host), &self.projects);
        // Effective proxy default for the wizard:
        // active session's proxy > startup override > config default.
        let proxy_default = active_proxy
            .as_deref()
            .or_else(|| self.proxy_override.pick(self.proxy_default.as_deref()));
        self.modal_gen += 1;
        let wizard = Wizard::new(
            seeds,
            &self.local_recent(),
            self.home.clone(),
            &active_host,
            &self.ssh_hosts,
            &self.proxy_profiles,
            proxy_default,
        );
        self.modal = Some(Modal::Wizard(Box::new(wizard)));
        // `~/.ssh/config` may have changed since the list was read.
        self.emit(Effect::LoadSshHosts);
        self.request_projects("local");
        self.redraw = true;
    }

    // ── Recovery ─────────────────────────────────────────────────────────────

    /// Begin as the manager: recover the tabs, and subscribe to the local
    /// host's CPU and memory samples for the status bar (an old daemon
    /// refuses; the sparklines then stay empty).
    pub fn start_manager(&mut self, saved: &ClientState, live: &[SessionInfo]) {
        self.recover(saved, live);
        self.subscribe_host_stats("local");
    }

    /// Subscribe to `host`'s CPU and memory samples, for the status bar.
    /// Needed on every new connection: the subscription is per connection.
    pub fn subscribe_host_stats(&mut self, host: &str) {
        if self.mode == Mode::Manager {
            self.request(host, Msg::SubscribeHostStats, ReplyTo::HostStats);
        }
    }

    /// Full recovery: rebuild the entire session list from `saved` and the
    /// daemon's `live` sessions (initial local startup).
    ///
    /// Only call once at startup. For reconnects, use [`recover_host`].
    pub fn recover(&mut self, saved: &ClientState, live: &[SessionInfo]) {
        // Merge in tombstones from saved state.
        self.killed = saved.killed.clone();
        self.recover_host_inner("local", saved, live, true);
    }

    /// Host-scoped recovery: reconcile only the given host's
    /// sessions against the current live list from that host's daemon.
    ///
    /// - Does NOT touch sessions belonging to other hosts.
    /// - Assigns unknown live sessions the correct originating host (not "local").
    /// - Preserves offline hosts' tabs, names and proxy associations.
    /// - Re-sends Kill for any tombstone that is still in the live list.
    pub fn recover_host(&mut self, host: &str, live: &[SessionInfo]) {
        // Build a synthetic ClientState from current in-memory sessions for this host.
        let saved_for_host = ClientState {
            sessions: self
                .sessions
                .iter()
                .filter(|v| v.host == host)
                .map(SessionView::saved)
                .collect(),
            active: self.active_view().filter(|v| v.host == host).map(|v| v.id),
            recent_dirs: self.recent_dirs.clone(),
            killed: self.killed.clone(),
            ..Default::default()
        };
        // Remove existing sessions for this host (they'll be re-added after merge).
        let active_id = self.active_view().map(|v| v.id);
        self.sessions.retain(|v| v.host != host);
        // Adjust active index (may now point into a shrunk vec).
        self.active = active_id.and_then(|id| self.sessions.iter().position(|v| v.id == id));

        self.recover_host_inner(host, &saved_for_host, live, false);
    }

    /// Internal: merge `saved` + `live` for `host` and add the resulting
    /// sessions to `self.sessions`. When `is_full` is true (initial startup),
    /// sessions.clear() is called first and active/wizard logic runs.
    fn recover_host_inner(
        &mut self,
        host: &str,
        saved: &ClientState,
        live: &[SessionInfo],
        is_full: bool,
    ) {
        let size = self.pane_size();
        if is_full {
            self.connected = true;
            self.sessions.clear();
            self.active = None;
        }

        let merged = state::merge_for_host(host, saved, live);

        // Re-send Kill for tombstoned sessions that are still live.
        for id in live.iter().map(|info| info.id) {
            if self.killed.iter().any(|t| t.id == id && t.host == host) {
                self.request(host, Msg::Kill { id }, ReplyTo::Kill(id));
            }
        }

        for r in merged {
            if let Some(args) = r.respawn {
                let req = SpawnRequest {
                    id: r.saved.id,
                    kind: r.saved.kind,
                    cwd: r.saved.cwd.clone(),
                    name: r.saved.name.clone(),
                    args,
                };
                if let Err(e) = self.send_spawn(&r.saved.host, req, r.saved.proxy.as_deref()) {
                    self.notify(format!("cannot respawn '{}': {e}", r.saved.cwd));
                }
            }
            self.sessions.push(SessionView {
                state: r.state,
                title: r.title,
                ..SessionView::from_saved(r.saved, size)
            });
        }

        if is_full {
            let restore = saved.active.and_then(|id| self.index_of(id));
            match restore.or((!self.sessions.is_empty()).then_some(0)) {
                Some(i) => self.activate(i),
                None if self.modal.is_none() => self.open_wizard(),
                None => {}
            }
        } else {
            // After a per-host recovery, activate the first session for this
            // host if nothing is currently active.
            if self.active.is_none() && !self.sessions.is_empty() {
                self.activate(0);
            }
        }
        self.save();
    }

    /// Re-run recovery after the local daemon reconnects.
    pub fn on_reconnected_local(&mut self, live: &[SessionInfo], home: String) {
        self.home = home;
        self.notice = None;
        self.connected = true;
        if self.mode == Mode::Plain {
            // Never adopt the manager's sessions: just re-attach our own.
            self.plain_reconnected(live);
            return;
        }
        self.recover_host("local", live);
        self.subscribe_host_stats("local");
    }

    /// The local daemon connection dropped; only local sessions are affected.
    pub fn on_disconnected_local(&mut self) {
        self.connected = false;
        for v in &mut self.sessions {
            if v.host == "local" {
                v.attached = false;
            }
        }
        self.redraw = true;
    }

    /// A remote daemon connection dropped (only affects that host's sessions).
    pub fn on_disconnected_remote(&mut self, host: &str) {
        for v in &mut self.sessions {
            if v.host == host {
                v.state = SessionState::Unknown;
                v.attached = false;
            }
        }
        self.redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_c0_controls() {
        assert_eq!(sanitize_label("hello\x07world", 200), "helloworld");
        // ESC (0x1B) is stripped; the remaining "[31m" chars are printable ASCII.
        assert_eq!(sanitize_label("a\x1b[31mb", 200), "a[31mb");
    }

    #[test]
    fn sanitize_strips_del() {
        assert_eq!(sanitize_label("a\x7fb", 200), "ab");
    }

    #[test]
    fn sanitize_strips_c1_controls() {
        // C1 control: 0x9B (CSI in Latin-1).
        let s = "\u{009B}test";
        assert_eq!(sanitize_label(s, 200), "test");
    }

    #[test]
    fn sanitize_caps_length() {
        let long: String = "a".repeat(300);
        assert_eq!(sanitize_label(&long, 200).len(), 200);
    }

    #[test]
    fn sanitize_keeps_normal_unicode() {
        assert_eq!(sanitize_label("héllo wörld", 200), "héllo wörld");
    }

    #[test]
    fn sanitize_malicious_label() {
        // Injection attempt: ESC (stripped), ] (printable), BEL (stripped), [31m (printable).
        // ESC is the dangerous byte; stripping it neutralises the OSC sequence.
        let evil = "\x1b]0;injected\x07\x1b[31m";
        assert_eq!(sanitize_label(evil, 200), "]0;injected[31m");
    }
}
