//! Modal popup types and terminal input / wizard routing for [`App`].
//!
//! The `impl App` block here handles all keyboard, paste, mouse, focus and
//! resize events, wizard state transitions and host connection callbacks,
//! and the daemon's events and request replies.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use std::collections::HashSet;
use std::io;
use std::time::Instant;

use crate::client::Incoming;
use crate::proto::{Msg, SessionEvent, SessionId, SessionKind, SessionState};
use crate::term::keys::{encode_focus, encode_key, encode_mouse, encode_paste};
use crate::term::screen::{MouseMode, Screen};

use super::app::{App, Effect, Mode, ReplyTo};
use super::confirm::ConfirmPrompt;
use super::git_view::GitView;
use super::keymap::Action;
use super::selection::{self, Pos, Selection, Unit};
use super::sessions::SessionView;
use super::stats_view::StatsView;
use super::ui;
use super::wizard::{Outcome, Wizard};

// ── Modal ─────────────────────────────────────────────────────────────────────

/// A popup that captures the keyboard.
pub enum Modal {
    Rename {
        id: SessionId,
        input: String,
    },
    /// The new-session wizard. Boxed like the history viewer: both are
    /// several hundred bytes, the other modals a few dozen.
    Wizard(Box<Wizard>),
    /// Proxy stats popup (see `stats_view.rs`).
    ProxyStats(StatsView),
    /// Overview / "mission control": all sessions at a glance.
    Overview {
        selected: usize,
        /// Live filter string; empty = show all.
        filter: String,
    },
    /// Help popup: all key bindings.
    Help,
    /// A multiple-choice question: kill, reset, update claude (see
    /// [`super::confirm`]).
    Confirm(ConfirmPrompt),
    /// The commit-history viewer (full pane).
    Git(Box<GitView>),
}

impl Modal {
    /// Read-only views give way to a manager action: its key closes the view
    /// and runs the action. Text-input modals swallow it instead.
    pub fn yields_to_actions(&self) -> bool {
        matches!(
            self,
            Modal::Git(_) | Modal::Overview { .. } | Modal::Help | Modal::ProxyStats { .. }
        )
    }

    /// Whether `action` is the one that opens this modal; pressing its key
    /// again closes the modal instead of reopening it.
    fn opened_by(&self, action: Action) -> bool {
        matches!(
            (self, action),
            (Modal::Git(_), Action::GitLog)
                | (Modal::Overview { .. }, Action::Overview)
                | (Modal::Help, Action::Help)
                | (Modal::ProxyStats { .. }, Action::ProxyStats)
        )
    }

    /// Whether the modal takes mouse wheel and clicks.
    fn wants_mouse(&self) -> bool {
        matches!(self, Modal::Git(_))
    }
}

// ── Terminal input (impl App) ─────────────────────────────────────────────────

impl App {
    pub fn on_terminal(&mut self, ev: crossterm::event::Event) {
        match ev {
            crossterm::event::Event::Key(key) => self.on_key(key),
            crossterm::event::Event::Paste(text) => self.on_paste(&text),
            crossterm::event::Event::Mouse(m) => self.on_mouse(m),
            crossterm::event::Event::FocusGained => self.on_focus(true),
            crossterm::event::Event::FocusLost => self.on_focus(false),
            crossterm::event::Event::Resize(w, h) => self.on_resize(w, h),
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.clear_selection();
        let action = self.keymap.lookup(&key);
        if action == Some(Action::Quit) {
            self.on_action(Action::Quit);
            return;
        }
        if let Some(modal) = &self.modal {
            match action.filter(|_| modal.yields_to_actions()) {
                Some(action) => {
                    let toggled_off = modal.opened_by(action);
                    self.modal = None;
                    if toggled_off {
                        self.redraw = true;
                    } else {
                        self.on_action(action);
                    }
                }
                None => self.modal_key(key),
            }
            return;
        }
        if let Some(action) = action {
            self.on_action(action);
            return;
        }
        if let Some(v) = self.active_view().filter(|v| v.attached) {
            let bytes = encode_key(&key, &v.mirror.modes());
            if !bytes.is_empty() {
                self.emit(Effect::Input(v.id, bytes));
            }
        }
    }

    fn on_action(&mut self, action: Action) {
        let n = self.sessions.len();
        match action {
            Action::PrevSession | Action::NextSession if n > 0 => {
                let cur = self.active.unwrap_or(0);
                let next = if action == Action::NextSession {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                self.activate(next);
            }
            Action::NextAttention if n > 0 => {
                let cur = self.active.unwrap_or(0);
                let found = (1..n)
                    .map(|k| (cur + k) % n)
                    .find(|&i| self.sessions[i].state.wants_attention());
                match found {
                    Some(i) => self.activate(i),
                    None => self.notify("no session needs attention"),
                }
            }
            Action::GotoSession(digit) => {
                // Tabs are numbered from 1; 0 is tab 10.
                let tab = if digit == 0 { 10 } else { usize::from(digit) };
                if tab <= n {
                    self.activate(tab - 1);
                } else {
                    self.notify(format!("no session {tab}"));
                }
            }
            Action::NewSession => self.open_wizard(),
            Action::Terminal => self.open_terminal(),
            Action::Reset => self.ask_reset(),
            Action::Rename => {
                if let Some(v) = self.active_view() {
                    self.modal = Some(Modal::Rename {
                        id: v.id,
                        input: v.label(),
                    });
                }
            }
            Action::Close => self.ask_kill(),
            Action::Quit => {
                self.save();
                self.quit = true;
            }
            Action::ProxyStats => self.open_proxy_stats(),
            Action::Overview => self.open_overview(),
            Action::Help => self.open_help(),
            Action::GitLog => self.open_git_log(),
            _ => {}
        }
        self.redraw = true;
    }

    fn open_overview(&mut self) {
        let selected = self.active.unwrap_or(0);
        self.modal = Some(Modal::Overview { selected, filter: String::new() });
        self.redraw = true;
    }

    fn open_help(&mut self) {
        self.modal = Some(Modal::Help);
        self.redraw = true;
    }

    pub fn modal_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.redraw = true;
        let rows = self.pane_size().0 as usize;
        let Some(modal) = &mut self.modal else { return };
        match modal {
            Modal::Wizard(w) => {
                let outcome = if self.keymap.lookup_wizard(&key) == Some(Action::ToggleHidden) {
                    w.toggle_hidden()
                } else {
                    w.on_key(&key)
                };
                self.wizard_outcome(outcome);
            }
            Modal::Rename { id, input } => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => {
                    let (id, name) = (*id, input.trim().to_owned());
                    let name = (!name.is_empty()).then_some(name);
                    self.modal = None;
                    if let Some(i) = self.index_of(id) {
                        let host = self.sessions[i].host.clone();
                        self.sessions[i].name = name.clone();
                        // Persist to daemon journal so the name survives reconnects.
                        self.request(
                            &host,
                            Msg::Rename { id, name },
                            ReplyTo::Ack("rename"),
                        );
                        self.save();
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => input.clear(),
                KeyCode::Char(c) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    input.push(c)
                }
                _ => {}
            },
            Modal::ProxyStats(_) => self.stats_key(key),
            Modal::Overview { selected, filter } => {
                let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
                match key.code {
                    KeyCode::Esc => {
                        if filter.is_empty() {
                            self.modal = None;
                        } else {
                            *filter = String::new();
                            *selected = 0;
                        }
                    }
                    KeyCode::Backspace => {
                        filter.pop();
                        *selected = 0;
                    }
                    KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                        filter.clear();
                        *selected = 0;
                    }
                    KeyCode::Up if *selected > 0 => *selected -= 1,
                    KeyCode::Down => {
                        // Max is the filtered session count.
                        let f = filter.clone();
                        let matches = self.sessions.iter()
                            .filter(|v| overview_matches(v, &f))
                            .count();
                        if *selected + 1 < matches {
                            *selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        let f = filter.clone();
                        let sel = *selected;
                        // Find the actual index in all sessions.
                        let real_idx = self.sessions.iter().enumerate()
                            .filter(|(_, v)| overview_matches(v, &f))
                            .nth(sel)
                            .map(|(i, _)| i);
                        self.modal = None;
                        if let Some(i) = real_idx {
                            self.activate(i);
                        }
                    }
                    KeyCode::Char(c) if plain => {
                        filter.push(c);
                        *selected = 0;
                    }
                    _ => {}
                }
            }
            Modal::Help => self.modal = None,
            Modal::Confirm(_) => self.confirm_key(key),
            Modal::Git(view) => {
                let outcome = view.on_key(&key, rows);
                self.git_outcome(outcome);
            }
        }
    }

    /// A mouse event while a modal that wants the mouse is open.
    fn modal_mouse(&mut self, m: MouseEvent) {
        let rows = self.pane_size().0 as usize;
        let Some(Modal::Git(view)) = &mut self.modal else { return };
        // Pane rows start below the tab bar; the status bar is not the pane.
        let above = self.mode.rows_above() as usize;
        let Some(row) = (m.row as usize).checked_sub(above).filter(|&r| r < rows) else {
            return;
        };
        let outcome = view.on_mouse(m.kind, row, rows);
        self.git_outcome(outcome);
    }

    fn on_paste(&mut self, text: &str) {
        self.clear_selection();
        match &mut self.modal {
            Some(Modal::Wizard(w)) => {
                let outcome = w.on_paste(text);
                self.wizard_outcome(outcome);
            }
            Some(Modal::Rename { input, .. }) => {
                input.push_str(text.lines().next().unwrap_or(""));
                self.redraw = true;
            }
            Some(Modal::Git(view)) => {
                view.on_paste(text);
                self.redraw = true;
            }
            Some(Modal::ProxyStats(_) | Modal::Overview { .. } | Modal::Help | Modal::Confirm(_)) => {}
            None => {
                if let Some(v) = self.active_view().filter(|v| v.attached) {
                    let bytes = encode_paste(text, &v.mirror.modes());
                    self.emit(Effect::Input(v.id, bytes));
                }
            }
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if let Some(modal) = &self.modal {
            if modal.wants_mouse() {
                self.modal_mouse(m);
            }
            return;
        }
        if self.select_mouse(&m) {
            return;
        }
        if self.mode == Mode::Manager && m.row == 0 {
            if m.kind == MouseEventKind::Down(MouseButton::Left) {
                let titles = ui::tab_titles(&self.sessions, self.active, self.width, self.now);
                if let Some(i) = ui::tab_at(&titles, m.column) {
                    self.activate(i);
                }
            }
            return;
        }
        let (rows, cols) = self.pane_size();
        if let Some(v) = self.active_view().filter(|v| v.attached) {
            let origin = (0, self.mode.rows_above());
            if let Some(bytes) = encode_mouse(&m, origin, (cols, rows), &v.mirror.modes()) {
                self.emit(Effect::Input(v.id, bytes));
            }
        }
    }

    // ── Copy on select ────────────────────────────────────────────────────────

    /// Copy-on-select (see `selection.rs`): a left-button press in the pane
    /// starts a selection, a drag extends it, and the release copies it.
    /// Only when `[ui] copy_on_select` is on and the session has not turned
    /// on mouse reporting (those events are its own). Returns whether the
    /// event was the selection's.
    fn select_mouse(&mut self, m: &MouseEvent) -> bool {
        let (rows, cols) = self.pane_size();
        let above = self.mode.rows_above();
        // The pane cell under the mouse, clamped into the pane: a drag may
        // leave it.
        let at = Pos::new(
            m.row.saturating_sub(above).min(rows - 1),
            m.column.min(cols - 1),
        );
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let in_pane = m.row >= above && m.row - above < rows;
                let Some(v) = self.active_view().filter(|v| {
                    in_pane
                        && self.copy_on_select
                        && v.attached
                        && v.mirror.modes().mouse == MouseMode::Off
                }) else {
                    // Any other click still drops an old selection.
                    self.clear_selection();
                    return false;
                };
                let (id, line) = (v.id, v.mirror.row_chars(at.row));
                let unit = Unit::from_clicks(self.clicks.press(at, Instant::now()));
                self.selection = Some(Selection::new(id, unit, unit.span(at, &line)));
                self.redraw = true;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(line) = self.dragged_row(at.row) else { return false };
                if let Some(sel) = &mut self.selection {
                    sel.extend(sel.unit().span(at, &line));
                }
                self.redraw = true;
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.dragged_row(at.row).is_none() {
                    return false;
                }
                self.finish_selection();
                true
            }
            _ => false,
        }
    }

    /// The characters of pane row `row` while a selection on the active
    /// session is being dragged; `None` when there is no such drag.
    fn dragged_row(&self, row: u16) -> Option<Vec<char>> {
        let sel = self.selection.as_ref().filter(|s| s.dragging())?;
        let v = self.active_view().filter(|v| v.id == sel.session)?;
        Some(v.mirror.row_chars(row))
    }

    /// The button was released: copy the selection, or drop it when it is
    /// a plain click (or only blanks). A copied selection stays highlighted
    /// until the next click, key, paste, tab switch or resize.
    fn finish_selection(&mut self) {
        self.redraw = true;
        let Some(sel) = &mut self.selection else { return };
        sel.finish();
        let (start, end) = sel.bounds();
        let text = (!sel.is_empty())
            .then(|| self.active_view())
            .flatten()
            .map(|v| v.mirror.text((start.row, start.col), (end.row, end.col)))
            .unwrap_or_default();
        if text.trim().is_empty() {
            self.selection = None;
            return;
        }
        let text = selection::clip(text);
        let n = text.chars().count();
        self.emit(Effect::CopyToClipboard(text));
        self.notify(format!("copied {n} char{}", if n == 1 { "" } else { "s" }));
    }

    /// Drop the selection (its highlight goes with it).
    pub(super) fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.redraw = true;
        }
    }

    fn on_focus(&mut self, gained: bool) {
        if let Some(v) = self.active_view().filter(|v| v.attached) {
            if let Some(bytes) = encode_focus(gained, &v.mirror.modes()) {
                self.emit(Effect::Input(v.id, bytes));
            }
        }
    }

    fn on_resize(&mut self, width: u16, height: u16) {
        self.clear_selection();
        self.width = width;
        self.height = height;
        self.redraw = true;
        let (rows, cols) = self.pane_size();
        if let Some(i) = self.active.filter(|&i| self.sessions[i].attached) {
            let v = &mut self.sessions[i];
            v.mirror.resize(rows, cols);
            let id = v.id;
            let host = v.host.clone();
            self.request(
                &host,
                Msg::Resize { id, rows, cols },
                ReplyTo::Ack("resize"),
            );
        }
    }

    // ── Wizard routing ────────────────────────────────────────────────────────

    pub(super) fn wizard_outcome(&mut self, outcome: Outcome) {
        let wizard_host = self.wizard_host();
        let wizard_gen = self.wizard_generation();
        match outcome {
            Outcome::None => {}
            Outcome::Cancel => self.modal = None,
            // The local daemon is always connected.
            Outcome::ConnectHost(host) if host == "local" => {
                let home = self.home.clone();
                self.on_host_connected("local", &home);
            }
            Outcome::ConnectHost(host) => self.emit(Effect::Connect(host)),
            Outcome::ListDir(path) => {
                self.request(&wizard_host, Msg::ListDir { path }, ReplyTo::DirEntries)
            }
            Outcome::ChooseDir(cwd) => {
                // A directory picked from the first screen's LOCAL section:
                // move on to the local directory step, waiting for this one.
                let in_host_step = matches!(&self.modal,
                    Some(Modal::Wizard(w)) if w.host_step.is_some());
                if in_host_step {
                    let home = self.home.clone();
                    if let Some(w) = self.wizard_enter_host("local", &home) {
                        w.pending = Some(cwd.clone());
                    }
                    self.request_projects("local");
                }
                let to = ReplyTo::ClaudeSessions {
                    host: wizard_host.clone(),
                    cwd: cwd.clone(),
                    gen: wizard_gen,
                };
                self.request(&wizard_host, Msg::ListClaudeSessions { cwd }, to)
            }
            Outcome::Spawn { cwd, resume, proxy } => {
                self.modal = None;
                if let Some(ref pname) = proxy {
                    self.maybe_fetch_proxy_config(pname);
                }
                self.spawn(wizard_host, cwd, resume, proxy);
            }
        }
        self.redraw = true;
    }

    /// `host` (whose home is `home`) is connected: the wizard goes on to its
    /// directory step, browsing the home directory.
    pub fn on_host_connected(&mut self, host: &str, home: &str) {
        if let Some(w) = self.wizard_enter_host(host, home) {
            let list_home = w.browse_home();
            self.request_projects(host);
            self.wizard_outcome(list_home);
        }
        self.redraw = true;
    }

    /// Move the open wizard on to `host`'s directory step, seeded for it.
    fn wizard_enter_host(&mut self, host: &str, home: &str) -> Option<&mut Wizard> {
        let seeds = self.wizard_seeds(host);
        let w = self.wizard_mut()?;
        w.on_host_connected(host, home, seeds);
        Some(w)
    }

    /// The wizard's directory candidates on `host`: the active session's
    /// directory when it runs there, then the host's recently used ones.
    /// Recent claude projects follow when they arrive.
    pub(super) fn wizard_seeds(&self, host: &str) -> Vec<String> {
        let active_cwd = self
            .active_view()
            .filter(|v| v.host == host)
            .map(|v| v.cwd.as_str());
        let recent = super::state::recent_for_host(&self.recent_dirs, host);
        super::wizard::assemble(active_cwd, recent, &[])
    }

    /// The first screen's LOCAL list: recently used local directories, then
    /// those with recent claude activity, minus any already open in a tab.
    pub(super) fn local_recent(&self) -> Vec<String> {
        let open: HashSet<&str> = self
            .sessions
            .iter()
            .filter(|v| v.host == "local")
            .map(|v| v.cwd.as_str())
            .collect();
        let mut seen = HashSet::new();
        super::state::recent_for_host(&self.recent_dirs, "local")
            .iter()
            .chain(&self.projects)
            .filter(|d| !open.contains(d.as_str()) && seen.insert(d.as_str()))
            .take(LOCAL_RECENT_LIMIT)
            .cloned()
            .collect()
    }

    pub fn on_host_error(&mut self, host: &str, error: &str) {
        if let Some(Modal::Wizard(w)) = &mut self.modal {
            if let Some(hs) = &mut w.host_step {
                hs.connecting = None;
            }
        }
        self.notify(format!("cannot connect to {host}: {error}"));
    }

    pub(super) fn wizard_mut(&mut self) -> Option<&mut Wizard> {
        match &mut self.modal {
            Some(Modal::Wizard(w)) => Some(w.as_mut()),
            _ => None,
        }
    }
}

// ── Daemon events (impl App) ──────────────────────────────────────────────────

impl App {
    /// Handle an incoming event from the daemon, tagged with the originating host.
    pub fn on_incoming_from(&mut self, host: &str, inc: Incoming) {
        match inc {
            Incoming::Data { id, bytes } => {
                if let Some(i) = self.index_of(id).filter(|&i| self.sessions[i].attached) {
                    self.sessions[i].mirror.feed(&bytes);
                    if self.active == Some(i) {
                        self.redraw = true;
                    }
                }
            }
            Incoming::Attached { id, rows, cols } => {
                if let Some(i) = self.index_of(id).filter(|&i| self.sessions[i].attached) {
                    if self.selection.as_ref().is_some_and(|s| s.session == id) {
                        self.selection = None;
                    }
                    self.sessions[i].mirror = Screen::new(rows, cols);
                    self.redraw = true;
                }
            }
            Incoming::Event { id, event } => self.on_event(host, id, event),
            Incoming::HostStats { .. } => {
                // Handled in the event loop before on_incoming_from is called.
            }
            Incoming::Disconnected => {
                if host == "local" {
                    self.on_disconnected_local();
                } else {
                    self.on_disconnected_remote(host);
                }
            }
        }
    }

    fn on_event(&mut self, host: &str, id: SessionId, event: SessionEvent) {
        self.redraw = true;
        if self.mode == Mode::Plain && self.plain_event(id, &event) {
            return;
        }
        // Another client's new session becomes a tab, unless it is not one
        // for the manager (`--plain`'s).
        if let SessionEvent::Created { info } = &event {
            if self.index_of(id).is_none() {
                if !info.ephemeral {
                    let view = SessionView::from_info(host, info, self.pane_size());
                    self.sessions.push(view);
                    self.save();
                }
                return;
            }
        }
        let Some(i) = self.index_of(id) else { return };
        let v = &mut self.sessions[i];
        match event {
            SessionEvent::Created { info } => {
                v.cwd = info.cwd;
                v.state = info.state;
                v.title = info.title.or(v.title.take());
                v.created_at = info.created_at;
                if info.claude_session_id.is_some() {
                    v.claude_session_id = info.claude_session_id;
                }
                self.save();
            }
            // Our own closes are gone already; this is a clean exit or
            // another client's kill.
            SessionEvent::Removed => {
                let label = v.label();
                self.remove(i);
                self.notify(format!("{label}: session ended"));
            }
            SessionEvent::State { state } => v.state = state,
            SessionEvent::ClaudeSession { claude_session_id } => {
                v.claude_session_id = Some(claude_session_id);
                self.save();
                self.refresh_session_cred(super::proxy_state::SESSION_CRED_MIN_GAP_SECS);
            }
            // A fresh reset: recovery must not resume the old conversation
            // from what we saved.
            SessionEvent::ClaudeSessionCleared => {
                v.claude_session_id = None;
                self.save();
            }
            SessionEvent::Title { title } => v.title = Some(title),
            SessionEvent::Exited { .. } => v.state = SessionState::Exited,
            SessionEvent::Notice { text } => self.notify(text),
            SessionEvent::Renamed { name } => {
                v.name = name;
                self.save();
            }
            // `None` is "no news", never "cleared".
            SessionEvent::Meta {
                branch,
                model,
                context_tokens,
                git,
                output_tokens,
                turns,
            } => {
                if branch.is_some() {
                    v.branch = branch;
                }
                if model.is_some() {
                    v.model = model;
                }
                if context_tokens.is_some() {
                    v.context_tokens = context_tokens;
                }
                if git.is_some() {
                    v.git = git;
                }
                if output_tokens.is_some() {
                    v.output_tokens = output_tokens;
                }
                if turns.is_some() {
                    v.turns = turns;
                }
            }
            SessionEvent::Unknown => {}
        }
    }

    /// A reply (or failure) for a request made through [`Effect::Request`].
    pub fn on_reply(&mut self, to: ReplyTo, reply: io::Result<Msg>) {
        self.redraw = true;
        match (to, reply) {
            (ReplyTo::HostStats, _) => {}
            // Delivered (a session that is already gone answers "no such
            // session"): the tombstone can go.
            (ReplyTo::Kill(id), Ok(_)) => {
                self.killed.retain(|t| t.id != id);
                self.save();
            }
            // The tombstone stays, so the next recovery sends the Kill again.
            (ReplyTo::Kill(_), Err(e)) => self.notify(format!("kill failed (will retry): {e}")),
            (ReplyTo::Projects { host, gen }, Ok(Msg::Projects { dirs })) => {
                let paths: Vec<String> = dirs.iter().map(|d| d.path.clone()).collect();
                if host == "local" {
                    // Seeds the next wizard at once.
                    self.projects = paths.clone();
                }
                if gen != self.wizard_generation() {
                    return;
                }
                let local_recent = (host == "local").then(|| self.local_recent());
                if let Some(w) = self.wizard_mut() {
                    // Ignored unless the wizard is still on `host`.
                    w.add_seeds_for_host(&host, &paths);
                    w.add_project_meta(&dirs);
                    if let Some(dirs) = &local_recent {
                        w.set_local_dirs(dirs);
                    }
                }
            }
            (ReplyTo::DirEntries, Ok(Msg::DirEntries { path, entries, .. })) => {
                if let Some(w) = self.wizard_mut() {
                    w.set_dir_entries(&path, &entries);
                }
            }
            (ReplyTo::ClaudeSessions { host, cwd, gen }, reply) => {
                if gen != self.wizard_generation() {
                    return;
                }
                match reply {
                    Ok(Msg::ClaudeSessions { sessions, .. }) => {
                        if let Some(w) = self.wizard_mut() {
                            let outcome = w.set_claude_sessions(&cwd, sessions);
                            self.wizard_outcome(outcome);
                        }
                    }
                    Ok(_) => {}
                    // An error is not "no sessions": say so and let the user
                    // retry.
                    Err(e) => {
                        let place = if host == "local" { "" } else { "remote " };
                        self.notify(format!("could not list {place}claude sessions: {e}"));
                        if let Some(w) = self.wizard_mut() {
                            w.set_claude_sessions_error(&cwd);
                        }
                    }
                }
            }
            (ReplyTo::Git { gen, seq }, reply) => self.on_git_reply(gen, seq, reply),
            (ReplyTo::Respawned(id), reply) => self.on_respawned(id, reply),
            (ReplyTo::SpawnedDeferred(id), Ok(_)) => {
                // Re-attach the active view: its first Attach raced the Spawn.
                if let Some(i) = self.index_of(id).filter(|&i| self.active == Some(i)) {
                    self.sessions[i].attached = false;
                    self.activate(i);
                }
            }
            (ReplyTo::Spawned(_) | ReplyTo::SpawnedDeferred(_), Err(e))
                if self.mode == Mode::Plain =>
            {
                self.plain_failed(format!("could not start claude: {e}"));
            }
            (ReplyTo::Spawned(id) | ReplyTo::SpawnedDeferred(id), Err(e)) => {
                let Some(i) = self.index_of(id) else { return };
                let (kind, host) = (self.sessions[i].kind, self.sessions[i].host.clone());
                match (kind, App::older_daemon_notice(&host, &e)) {
                    // An old daemon cannot run terminals: no tab to keep.
                    (SessionKind::Shell, Some(notice)) => {
                        self.remove(i);
                        self.notify(notice);
                    }
                    (SessionKind::Shell, None) => {
                        self.sessions[i].state = SessionState::Exited;
                        self.notify(format!("could not start terminal: {e}"));
                    }
                    (SessionKind::Claude, _) => {
                        self.sessions[i].state = SessionState::Exited;
                        self.notify(format!("could not start claude: {e}"));
                    }
                }
            }
            (ReplyTo::ClaudeUpdate(host), reply) => self.on_claude_updated(&host, reply),
            // Typing a path that doesn't exist (yet) is not an error.
            (ReplyTo::DirEntries, Err(_)) => {}
            (to, Err(e)) if self.connected => {
                let what = match to {
                    ReplyTo::Ack(what) => what,
                    ReplyTo::Projects { .. } => "recent projects",
                    _ => "request",
                };
                self.notify(format!("{what} failed: {e}"));
            }
            _ => {}
        }
    }
}

/// How many recent directories the wizard's first screen offers.
const LOCAL_RECENT_LIMIT: usize = 8;

// ── Overview filter helper ─────────────────────────────────────────────────────

/// Whether a session matches the overview filter string (case-insensitive
/// substring match against label, cwd, host, or state name).
pub fn overview_matches(v: &SessionView, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let f = filter.to_lowercase();
    let label = v.label().to_lowercase();
    let cwd = v.cwd.to_lowercase();
    let host = v.host.to_lowercase();
    let state = v.state.name();
    label.contains(&f) || cwd.contains(&f) || host.contains(&f) || state.contains(&f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::AppConfig;
    use crate::tui::test_support::{self, app_with, info, press};
    use crossterm::event::Event;
    use ratatui::{backend::TestBackend, Terminal};

    /// A mouse event at screen `col`, `row` (row 0 is the tab bar).
    fn mouse(kind: MouseEventKind, col: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE })
    }

    const DOWN: MouseEventKind = MouseEventKind::Down(MouseButton::Left);
    const DRAG: MouseEventKind = MouseEventKind::Drag(MouseButton::Left);
    const UP: MouseEventKind = MouseEventKind::Up(MouseButton::Left);

    /// Press at `from`, drag to `to`, release there; screen `(col, row)`.
    fn drag(app: &mut App, from: (u16, u16), to: (u16, u16)) {
        app.on_terminal(mouse(DOWN, from.0, from.1));
        app.on_terminal(mouse(DRAG, to.0, to.1));
        app.on_terminal(mouse(UP, to.0, to.1));
    }

    fn click(app: &mut App, col: u16, row: u16) {
        app.on_terminal(mouse(DOWN, col, row));
        app.on_terminal(mouse(UP, col, row));
    }

    /// The texts the queued effects copy.
    fn copied(app: &mut App) -> Vec<String> {
        app.take_effects()
            .into_iter()
            .filter_map(|e| match e {
                Effect::CopyToClipboard(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    /// One idle session whose screen reads "hello world" / "second line".
    fn app_with_text() -> App {
        let mut app = app_with(&[info(Some(1), None)]);
        app.sessions[0].mirror.feed(b"hello world\r\nsecond line");
        app
    }

    #[test]
    fn a_drag_copies_the_selected_text() {
        let mut app = app_with_text();
        // Pane row 0 is screen row 1, below the tab bar.
        drag(&mut app, (0, 1), (4, 1));
        assert_eq!(copied(&mut app), ["hello"]);
        assert_eq!(app.notice.as_ref().unwrap().text, "copied 5 chars");
        let sel = app.selection.as_ref().expect("stays highlighted");
        assert!(!sel.dragging() && sel.contains(0, 4) && !sel.contains(0, 5));

        drag(&mut app, (6, 1), (5, 2));
        assert_eq!(copied(&mut app), ["world\nsecond"], "across rows");
        drag(&mut app, (5, 2), (6, 1));
        assert_eq!(copied(&mut app), ["world\nsecond"], "dragged backwards");
    }

    #[test]
    fn a_drag_past_the_pane_stops_at_its_edge() {
        let mut app = app_with_text();
        // The status bar is below the pane, the tab bar above it.
        drag(&mut app, (7, 2), (500, 29));
        assert_eq!(copied(&mut app), ["line"]);
        drag(&mut app, (3, 2), (1, 0));
        assert_eq!(copied(&mut app), ["ello world\nseco"]);
    }

    #[test]
    fn double_and_triple_clicks_take_a_word_and_a_row() {
        let mut app = app_with_text();
        click(&mut app, 8, 1);
        assert!(copied(&mut app).is_empty(), "a single click copies nothing");
        assert!(app.selection.is_none());
        click(&mut app, 8, 1);
        assert_eq!(copied(&mut app), ["world"]);
        click(&mut app, 8, 1);
        assert_eq!(copied(&mut app), ["hello world"]);
    }

    #[test]
    fn a_click_without_a_drag_clears_the_selection() {
        let mut app = app_with_text();
        drag(&mut app, (0, 1), (4, 1));
        app.take_effects();
        click(&mut app, 40, 5);
        assert!(app.selection.is_none());
        assert!(app.take_effects().is_empty());
        // So does a click outside the pane (the status bar).
        drag(&mut app, (0, 1), (4, 1));
        app.take_effects();
        click(&mut app, 3, 29);
        assert!(app.selection.is_none());
    }

    #[test]
    fn blanks_alone_copy_nothing() {
        let mut app = app_with_text();
        drag(&mut app, (30, 5), (50, 6));
        assert!(copied(&mut app).is_empty());
        assert!(app.selection.is_none());
    }

    #[test]
    fn a_session_with_mouse_reporting_gets_the_mouse_instead() {
        let mut app = app_with_text();
        app.sessions[0].mirror.feed(b"\x1b[?1002h\x1b[?1006h");
        let id = app.sessions[0].id;
        drag(&mut app, (0, 1), (4, 1));
        let effects = app.take_effects();
        let inputs: Vec<&[u8]> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Input(i, b) if *i == id => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(
            inputs,
            [&b"\x1b[<0;1;1M"[..], b"\x1b[<32;5;1M", b"\x1b[<0;5;1m"],
            "{effects:?}"
        );
        assert!(app.selection.is_none());
    }

    #[test]
    fn copy_on_select_off_leaves_the_mouse_alone() {
        let mut app = App::new(AppConfig {
            copy_on_select: false,
            ..test_support::config()
        });
        app.recover(&Default::default(), &[info(Some(1), None)]);
        app.take_effects();
        app.sessions[0].mirror.feed(b"hello world");
        drag(&mut app, (0, 1), (4, 1));
        click(&mut app, 2, 1);
        click(&mut app, 2, 1);
        assert!(app.take_effects().is_empty());
        assert!(app.selection.is_none() && app.notice.is_none());
    }

    #[test]
    fn a_key_or_a_resize_clears_the_selection() {
        let mut app = app_with_text();
        drag(&mut app, (0, 1), (4, 1));
        app.take_effects();
        app.on_terminal(press(KeyCode::Char('h')));
        assert!(app.selection.is_none());
        let id = app.sessions[0].id;
        assert!(
            matches!(&app.take_effects()[..], [Effect::Input(i, b)] if *i == id && b == b"h"),
            "the key still reaches the session"
        );
        drag(&mut app, (0, 1), (4, 1));
        assert!(app.selection.is_some());
        app.on_terminal(Event::Resize(90, 30));
        assert!(app.selection.is_none());
    }

    #[test]
    fn the_tab_bar_still_switches_tabs_and_drops_the_selection() {
        let mut app = app_with(&[info(Some(1), None), info(Some(2), None)]);
        app.activate(0);
        app.sessions[0].mirror.feed(b"hello world");
        drag(&mut app, (0, 1), (4, 1));
        app.take_effects();
        let titles = ui::tab_titles(&app.sessions, app.active, app.width, app.now);
        let col = (0..app.width).find(|&c| ui::tab_at(&titles, c) == Some(1)).unwrap();
        app.on_terminal(mouse(DOWN, col, 0));
        assert_eq!(app.active, Some(1));
        assert!(app.selection.is_none());
        assert!(copied(&mut app).is_empty());
    }

    #[test]
    fn plain_mode_selects_from_the_top_row() {
        let mut app = App::new(AppConfig {
            mode: Mode::Plain,
            ..test_support::config()
        });
        app.recover(&Default::default(), &[info(Some(1), None)]);
        app.take_effects();
        app.activate(0);
        app.sessions[0].mirror.feed(b"hello world");
        drag(&mut app, (6, 0), (10, 0));
        assert_eq!(copied(&mut app), ["world"]);
    }

    #[test]
    fn the_selection_is_drawn_reversed() {
        let mut app = app_with_text();
        drag(&mut app, (6, 1), (10, 1));
        let mut terminal = Terminal::new(TestBackend::new(app.width, app.height)).unwrap();
        terminal.draw(|f| ui::draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let reversed = |x: u16| buf[(x, 1)].modifier.contains(ratatui::style::Modifier::REVERSED);
        assert!((6..=10).all(reversed), "world is highlighted");
        assert!(!reversed(5) && !reversed(11) && !reversed(0));
    }
}
