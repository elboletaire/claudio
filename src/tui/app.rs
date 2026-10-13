//! Manager state and update logic.
//!
//! [`App`] holds the session views, the open modal and the bits of
//! persistent state. It never does I/O: inputs arrive through the `on_*`
//! methods and everything it wants done (daemon requests, PTY input, saving
//! state.json) is queued as [`Effect`]s that the event loop in `mod.rs`
//! drains with [`App::take_effects`].
//!
//! Sibling modules extend `App` with one concern each:
//! - `sessions`      – SessionView; spawning, activation and recovery
//! - `interaction`   – Modal; terminal input, wizard routing, daemon events
//!                     and request replies
//! - `confirm`       – the generic multiple-choice prompt
//! - `claude_update` – keeping claude up to date on every host
//! - `stats_view`    – the proxy stats popup (Alt+s)
//! - `git_app`       – the history viewer's glue (its state is `git_view`)
//! - `plain`         – `claudio --plain`, one session full screen
//! - `proxy_state`   – ProxyStatus and session credentials
//! - `notifications` – Notice and attention notifications
//! - `selection`     – copy-on-select in the pane (its glue is in
//!                     `interaction`)

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use crate::proto::{Msg, SessionId, SessionState};
use crate::proxy::api::{
    ConfigResponse, ModelsResponse, SessionCredential, SwitchError, SwitchOutcome,
};
use crate::proxy::ProxyChoice;

use super::confirm::{Choice, ConfirmAction, ConfirmPrompt};
use super::keymap::Keymap;
use super::selection::{Clicks, Selection};
use super::proxy_state::{
    ProxyFetch, SessionCred, SESSION_CRED_MIN_GAP_SECS, SESSION_CRED_POLL_SECS,
};
use super::state::{ClientState, KillTombstone};
use super::stats_view::{StatsOutcome, StatsView, Window};

// Re-export split types so ui.rs and mod.rs can still import from `app`.
pub use super::interaction::Modal;
pub use super::notifications::Notice;
pub use super::proxy_state::ProxyStatus;
pub use super::sessions::SessionView;

/// What the UI is: the multi-session manager, or the single bare session of
/// `claudio --plain` (see `plain.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Manager,
    /// One session fills the screen: no tab bar, status bar or wizard, no
    /// state.json, and only the [`PLAIN_ACTIONS`](super::keymap::PLAIN_ACTIONS)
    /// keys are claudio's.
    Plain,
}

impl Mode {
    /// Rows above the session pane (the tab bar).
    pub fn rows_above(self) -> u16 {
        match self {
            Mode::Manager => 1,
            Mode::Plain => 0,
        }
    }

    /// Rows taken by the chrome around the pane: the tab bar and the
    /// two-line status bar.
    pub fn chrome_rows(self) -> u16 {
        match self {
            Mode::Manager => 3,
            Mode::Plain => 0,
        }
    }

    /// Whether the session list is remembered in state.json.
    pub fn persists(self) -> bool {
        self == Mode::Manager
    }
}

/// A single host-stats sample stored in the ring buffer.
#[derive(Debug, Clone, Copy)]
pub struct HostStatsSample {
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
}
/// How long a notice stays in the status bar, in ticks.
const NOTICE_TICKS: u32 = 24;
/// `RecentProjects` limit for the wizard.
pub(super) const PROJECTS_LIMIT: u32 = 50;

/// Something for the event loop to do.
#[derive(Debug)]
pub enum Effect {
    /// Send a request to this host's daemon; the reply goes to [`App::on_reply`].
    Request { host: String, msg: Msg, to: ReplyTo },
    /// Forward input bytes to a session's PTY.
    Input(SessionId, Vec<u8>),
    /// Bootstrap and connect to a remote host (bootstrap + connect_ssh).
    /// When done, the event loop calls [`App::on_host_connected`] or
    /// [`App::on_host_error`].
    Connect(String),
    /// Write state.json.
    Save,
    /// Read the SSH host candidates again; they go to [`App::on_ssh_hosts`].
    LoadSshHosts,
    /// Fetch proxy config (env) for a profile name. Result goes to
    /// [`App::on_proxy_config`].
    FetchProxyConfig { profile_name: String },
    /// Fetch proxy stats (one request per window) plus pool health, and the
    /// model catalogue when `models` is set. Result goes to
    /// [`App::on_proxy_stats`].
    FetchProxyStats {
        profile_name: String,
        windows: Vec<Window>,
        models: bool,
    },
    /// Ask the proxy which upstream credential a conversation uses. The
    /// result goes to [`App::on_session_credential`].
    FetchSessionCredential {
        profile_name: String,
        session: SessionId,
        claude_session_id: String,
    },
    /// Ask the proxy to move a conversation to another subscription. The
    /// result goes to [`App::on_switch_result`].
    SwitchSubscription {
        profile_name: String,
        session: SessionId,
        claude_session_id: String,
    },
    /// Spawn (or respawn) a session with proxy config: the effect runner
    /// fetches the proxy config (with a 5 s timeout), builds the env, then
    /// sends the request. This avoids silently using fallback model defaults
    /// when the proxy-config cache is cold.
    SpawnWithProxy {
        host: String,
        /// A `Spawn` or `Respawn` whose `env` is empty; the runner fills it.
        msg: Msg,
        proxy_name: String,
        to: ReplyTo,
    },
    /// Put this text on the outer terminal's clipboard (OSC 52).
    CopyToClipboard(String),
}

/// What a request's reply is for.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyTo {
    /// Only errors matter; they are shown as a notice prefixed with this.
    Ack(&'static str),
    /// `SubscribeHostStats`: nothing to do, even on error (an old daemon has
    /// no stats; that host's sparklines stay empty).
    HostStats,
    Spawned(SessionId),
    /// A spawn sent after an async proxy-config fetch: any `Attach` issued
    /// meanwhile reached the daemon first and failed, so attach on success.
    SpawnedDeferred(SessionId),
    /// A `Respawn`: the tab stays, and the active view re-attaches to the
    /// new process.
    Respawned(SessionId),
    /// Kill acknowledged; the `SessionId` lets us clear the tombstone.
    Kill(SessionId),
    /// A directory listing for the wizard (which ignores one for a path it
    /// no longer shows).
    DirEntries,
    /// The claude sessions in `cwd` on `host`, for the wizard of generation
    /// `gen`; a reply for a cancelled or replaced wizard is dropped.
    ClaudeSessions { host: String, cwd: String, gen: u64 },
    /// The recent projects on `host`, for the wizard of generation `gen`.
    Projects { host: String, gen: u64 },
    /// `UpdateClaude` on this host; the outcome becomes a notice.
    ClaudeUpdate(String),
    /// A request of the history viewer: the view's generation and the
    /// request's sequence number (see `git_view`).
    Git { gen: u64, seq: u64 },
}

impl ReplyTo {
    /// The reply target for a request sent after an async proxy-config fetch.
    pub(super) fn deferred(self) -> ReplyTo {
        match self {
            ReplyTo::Spawned(id) => ReplyTo::SpawnedDeferred(id),
            other => other,
        }
    }
}

/// The manager's state.
pub struct App {
    pub mode: Mode,
    /// How the UI ends in [`Mode::Plain`]; read once `quit` is set.
    pub exit: super::plain::Exit,
    pub sessions: Vec<SessionView>,
    pub active: Option<usize>,
    pub modal: Option<Modal>,
    /// Most recently used directories per host ("local" for the local machine).
    pub recent_dirs: HashMap<String, Vec<String>>,
    /// The last `RecentProjects` reply, to seed the wizard.
    pub projects: Vec<String>,
    /// Terminal size.
    pub width: u16,
    pub height: u16,
    /// Animation counter, advanced every tick.
    pub tick: usize,
    /// Unix seconds, refreshed every tick.
    pub now: u64,
    pub notice: Option<Notice>,
    pub connected: bool,
    /// The daemon host's home directory.
    pub home: String,
    /// Set when the user asked to leave.
    pub quit: bool,
    /// Set when the screen needs repainting.
    pub redraw: bool,
    /// What the event loop is to do next: queued by [`App::emit`], drained
    /// by [`App::take_effects`].
    effects: Vec<Effect>,
    /// Cached proxy config (env vars) per profile name, with fetch time.
    pub proxy_config: HashMap<String, (ConfigResponse, Instant)>,
    /// Live proxy stats per profile name.
    pub proxy_status: HashMap<String, ProxyStatus>,
    /// Profiles whose model catalogue has been requested once for the status
    /// bar's context window (a failed fetch is not retried; Alt+s fetches it
    /// again anyway).
    models_requested: HashSet<String>,
    /// The upstream credential each proxied session last used, as reported by
    /// the proxy. Only the active session is polled; the rest is a cache.
    pub session_creds: HashMap<SessionId, SessionCred>,
    /// Ordered list of available proxy profile names (empty = no proxy configured).
    pub proxy_profiles: Vec<String>,
    /// The default proxy profile from config.
    pub proxy_default: Option<String>,
    /// Startup proxy override: Direct suppresses the default; Profile forces
    /// a specific profile regardless of per-session selection in the wizard.
    pub proxy_override: ProxyChoice,
    /// Last state for which a desktop notification was sent per session.
    /// Used to debounce: only one notification per session per state change.
    pub notified: HashMap<SessionId, SessionState>,
    /// Whether to emit desktop notifications (from config.toml [ui] notify).
    pub notify_enabled: bool,
    /// Sessions that need a notification emitted after the next draw.
    /// Populated by `check_notifications`; drained by the event loop.
    pub pending_notifs: Vec<String>,
    /// Kill tombstones: sessions the user explicitly closed. Persisted before
    /// Kill is sent, and cleared only when the daemon acknowledges.
    pub killed: Vec<KillTombstone>,
    /// The effective keymap (defaults + config.toml overrides).
    /// Owned here so rendering always reflects the current bindings.
    pub keymap: Keymap,
    /// Set by the background upgrade-check task when a newer release is found.
    /// Rendered as a dim `↑ vX.Y.Z` indicator at the right of the status bar.
    pub upgrade_notice: Option<String>,
    /// Ring buffer of host stats samples, keyed by host name.
    /// Capacity capped at 30 samples per host (~1 min at 2-s interval).
    pub host_stats: HashMap<String, std::collections::VecDeque<HostStatsSample>>,
    /// `[claude]` from config.toml: the update policies.
    pub claude_policy: crate::config::ClaudeSection,
    /// This machine's `claude --version`; remote hosts are compared with it.
    pub local_claude: Option<String>,
    /// Per host, the claude version the user chose to skip (persisted).
    pub claude_skipped: HashMap<String, String>,
    /// Hosts whose claude was already checked in this run.
    pub(super) claude_checked: HashSet<String>,
    /// Prompts waiting for the open modal to close.
    pub(super) confirms: VecDeque<ConfirmPrompt>,
    /// The SSH hosts the wizard offers: recently used ones, then
    /// `~/.ssh/config` aliases.
    pub(super) ssh_hosts: Vec<String>,
    /// The generation of the open wizard or history view, raised each time
    /// one opens. Their requests carry it, so a reply meant for one that
    /// was closed (or replaced) since is recognised and dropped.
    pub(super) modal_gen: u64,
    /// `[ui] copy_on_select`: a left-button drag in the pane selects and
    /// copies text when the session does not take the mouse itself.
    pub copy_on_select: bool,
    /// The pane's text selection, if any (see `selection.rs`).
    pub selection: Option<Selection>,
    /// Press timing for double and triple clicks.
    pub(super) clicks: Clicks,
}

/// What the app starts from. The event loop gathers it (reading config.toml,
/// state.json, the SSH host lists and the local daemon's welcome), so that
/// `App` itself never touches the outside world.
pub struct AppConfig {
    pub mode: Mode,
    /// Terminal size, `(width, height)`.
    pub size: (u16, u16),
    /// The local daemon host's home directory.
    pub home: String,
    pub keymap: Keymap,
    /// Desktop notifications for background sessions (`[ui] notify`).
    pub notify: bool,
    /// Drag to select and copy in a pane (`[ui] copy_on_select`).
    pub copy_on_select: bool,
    /// The startup proxy choice (`--proxy` / `--no-proxy`).
    pub proxy_override: ProxyChoice,
    /// The configured proxy profile names, and the default one.
    pub proxy_profiles: Vec<String>,
    pub proxy_default: Option<String>,
    /// `[claude]` from config.toml: the update policies.
    pub claude: crate::config::ClaudeSection,
    /// This machine's `claude --version`.
    pub local_claude: Option<String>,
    /// From state.json: recently used directories per host, and the claude
    /// versions the user chose to skip.
    pub recent_dirs: HashMap<String, Vec<String>>,
    pub claude_skipped: HashMap<String, String>,
    /// The SSH hosts the wizard offers (refreshed through
    /// [`Effect::LoadSshHosts`]).
    pub ssh_hosts: Vec<String>,
}

impl App {
    pub fn new(cfg: AppConfig) -> App {
        let (width, height) = cfg.size;
        App {
            mode: cfg.mode,
            exit: Default::default(),
            sessions: Vec::new(),
            active: None,
            modal: None,
            recent_dirs: cfg.recent_dirs,
            projects: Vec::new(),
            width,
            height,
            tick: 0,
            now: crate::paths::unix_now(),
            notice: None,
            connected: true,
            home: cfg.home,
            quit: false,
            redraw: true,
            effects: Vec::new(),
            proxy_config: HashMap::new(),
            proxy_status: HashMap::new(),
            models_requested: HashSet::new(),
            session_creds: HashMap::new(),
            proxy_profiles: cfg.proxy_profiles,
            proxy_default: cfg.proxy_default,
            proxy_override: cfg.proxy_override,
            notified: HashMap::new(),
            notify_enabled: cfg.notify,
            pending_notifs: Vec::new(),
            killed: Vec::new(),
            keymap: cfg.keymap,
            upgrade_notice: None,
            host_stats: HashMap::new(),
            claude_policy: cfg.claude,
            local_claude: cfg.local_claude,
            claude_skipped: cfg.claude_skipped,
            claude_checked: HashSet::new(),
            confirms: VecDeque::new(),
            ssh_hosts: cfg.ssh_hosts,
            modal_gen: 0,
            copy_on_select: cfg.copy_on_select,
            selection: None,
            clicks: Clicks::default(),
        }
    }

    /// The SSH host candidates were (re)read: offer them from now on, and in
    /// the wizard's first screen if it is open and untouched.
    pub fn on_ssh_hosts(&mut self, hosts: Vec<String>) {
        if let Some(w) = self.wizard_mut() {
            w.set_ssh_hosts(&hosts);
        }
        self.ssh_hosts = hosts;
        self.redraw = true;
    }

    /// Drain the queued effects, in order.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Queue `effect` for the event loop.
    pub(super) fn emit(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    /// Send a request to `host`'s daemon.
    pub(super) fn request(&mut self, host: &str, msg: Msg, to: ReplyTo) {
        self.emit(Effect::Request {
            host: host.to_owned(),
            msg,
            to,
        });
    }

    /// Queue a write of state.json; back-to-back saves are one write.
    pub(super) fn save(&mut self) {
        if self.mode.persists() && !matches!(self.effects.last(), Some(Effect::Save)) {
            self.emit(Effect::Save);
        }
    }

    // ── Proxy ─────────────────────────────────────────────────────────────────

    /// Called when a `FetchProxyConfig` effect completes.
    pub fn on_proxy_config(&mut self, profile_name: String, cfg: ConfigResponse) {
        self.proxy_config
            .insert(profile_name, (cfg, Instant::now()));
        self.redraw = true;
    }

    /// Called when a `FetchProxyStats` effect completes.
    pub fn on_proxy_stats(&mut self, profile_name: String, fetch: ProxyFetch) {
        if let Some(Modal::ProxyStats(view)) = &mut self.modal {
            if view.profile.as_deref() == Some(profile_name.as_str()) {
                view.on_fetched();
            }
        }
        self.proxy_status
            .entry(profile_name)
            .or_default()
            .apply(fetch);
        self.redraw = true;
    }

    /// Look up the credential of the active session when it uses a proxy and
    /// has a claude session id. A changed claude session id always fetches;
    /// otherwise a lookup newer than `min_age` seconds is reused.
    pub(super) fn refresh_session_cred(&mut self, min_age: u64) {
        let Some(v) = self.active_view() else { return };
        let (Some(profile), Some(csid)) = (v.proxy.clone(), v.claude_session_id.clone()) else {
            return;
        };
        if v.kind != crate::proto::SessionKind::Claude || csid.is_empty() {
            return;
        }
        let id = v.id;
        let now = self.now;
        self.request_models_once(&profile);
        let fresh = self.session_creds.get(&id).is_some_and(|c| {
            c.claude_session_id == csid && now.saturating_sub(c.asked_at) < min_age
        });
        if fresh {
            return;
        }
        let cred = match self.session_creds.remove(&id) {
            Some(old) if old.claude_session_id == csid => old.cred,
            _ => None,
        };
        self.session_creds.insert(
            id,
            SessionCred {
                claude_session_id: csid.clone(),
                cred,
                asked_at: now,
            },
        );
        self.effects.push(Effect::FetchSessionCredential {
            profile_name: profile,
            session: id,
            claude_session_id: csid,
        });
    }

    /// Called when a `FetchSessionCredential` effect completes. `result` is
    /// `Ok(None)` when the proxy does not know the conversation (yet); an
    /// error (rate limit, network) keeps what is cached.
    pub fn on_session_credential(
        &mut self,
        session: SessionId,
        claude_session_id: &str,
        result: Result<Option<SessionCredential>, String>,
    ) {
        let Some(entry) = self.session_creds.get_mut(&session) else {
            return;
        };
        // A reply for a conversation the session has since left.
        if entry.claude_session_id != claude_session_id {
            return;
        }
        if let Ok(cred) = result {
            entry.cred = cred;
            self.redraw = true;
        }
    }

    /// Fetch a profile's model catalogue (no stats windows) the first time a
    /// session on it is looked at, so the context gauge knows the window.
    fn request_models_once(&mut self, profile: &str) {
        let cached = self
            .proxy_status
            .get(profile)
            .is_some_and(|s| s.models.is_some());
        if cached || !self.models_requested.insert(profile.to_owned()) {
            return;
        }
        self.schedule_proxy_stats(profile, Vec::new());
    }

    /// The model catalogue of a session's proxy, once fetched (on the first
    /// look at a session on it, or by the stats popup); the status bar reads
    /// context windows from it.
    pub fn session_models(&self, v: &SessionView) -> Option<&ModelsResponse> {
        self.proxy_status
            .get(v.proxy.as_deref()?)
            .and_then(|s| s.models.as_ref())
    }

    /// The credential to show for a session: cached, and for its current
    /// claude session id.
    pub fn session_credential(&self, v: &SessionView) -> Option<&SessionCredential> {
        let entry = self.session_creds.get(&v.id)?;
        if v.claude_session_id.as_deref() != Some(entry.claude_session_id.as_str()) {
            return None;
        }
        entry.cred.as_ref()
    }

    /// Proxy config for a profile, if cached and fresh (5 min).
    pub fn proxy_config_cached(&self, name: &str) -> Option<&ConfigResponse> {
        self.proxy_config.get(name).and_then(|(cfg, when)| {
            if when.elapsed().as_secs() < 300 {
                Some(cfg)
            } else {
                None
            }
        })
    }

    /// Schedule a proxy config fetch if the cache is stale.
    pub fn maybe_fetch_proxy_config(&mut self, name: &str) {
        if self.proxy_config_cached(name).is_none() {
            self.effects.push(Effect::FetchProxyConfig {
                profile_name: name.to_owned(),
            });
        }
    }

    /// Schedule a proxy stats fetch for the named profile. The model
    /// catalogue is fetched along when it is not cached yet.
    pub fn schedule_proxy_stats(&mut self, name: &str, windows: Vec<Window>) {
        let models = self
            .proxy_status
            .get(name)
            .is_none_or(|s| s.models.is_none());
        self.effects.push(Effect::FetchProxyStats {
            profile_name: name.to_owned(),
            windows,
            models,
        });
    }

    /// Open the stats popup. Shows the active session's proxy profile, else
    /// the default profile (so a direct session can still look at the
    /// account), else a hint on how to configure a proxy.
    pub fn open_proxy_stats(&mut self) {
        let session_profile = self.active_view().and_then(|v| v.proxy.clone());
        let borrowed = session_profile.is_none();
        let profile = session_profile
            .or_else(|| self.proxy_default.clone())
            .or_else(|| self.proxy_profiles.first().cloned());
        let (mut view, outcome) = StatsView::open(profile, borrowed);
        view.can_switch = !borrowed
            && self
                .switchable_session()
                .is_some_and(|(_, p, _)| Some(&p) == view.profile.as_ref());
        self.stats_outcome(view, outcome);
        self.refresh_session_cred(SESSION_CRED_MIN_GAP_SECS);
        self.redraw = true;
    }

    /// A key press while the stats popup is open.
    pub(super) fn stats_key(&mut self, key: crossterm::event::KeyEvent) {
        let Some(Modal::ProxyStats(mut view)) = self.modal.take() else {
            return;
        };
        let cached = view
            .profile
            .as_deref()
            .and_then(|p| self.proxy_status.get(p))
            .map(|s| s.cached_windows())
            .unwrap_or_default();
        let outcome = view.on_key(&key, &cached);
        self.stats_outcome(view, outcome);
        self.redraw = true;
    }

    /// Apply a [`StatsOutcome`]: close the popup or keep it (fetching if asked).
    fn stats_outcome(&mut self, mut view: StatsView, outcome: StatsOutcome) {
        match outcome {
            StatsOutcome::Close => return,
            StatsOutcome::Fetch(windows) => {
                if let Some(name) = view.profile.clone() {
                    view.loading = true;
                    self.schedule_proxy_stats(&name, windows);
                }
            }
            StatsOutcome::Switch => {
                // The popup gives way to the prompt; Alt+s opens it again.
                self.ask_switch_subscription();
                return;
            }
            StatsOutcome::Nothing => {}
        }
        self.modal = Some(Modal::ProxyStats(view));
    }

    /// The active session's proxy profile and conversation id, when it can be
    /// moved to another subscription.
    fn switchable_session(&self) -> Option<(SessionId, String, String)> {
        let v = self.active_view()?;
        let csid = v.claude_session_id.clone().filter(|c| !c.is_empty())?;
        if v.kind != crate::proto::SessionKind::Claude {
            return None;
        }
        Some((v.id, v.proxy.clone()?, csid))
    }

    /// Ask before moving the active session to another subscription.
    fn ask_switch_subscription(&mut self) {
        let Some((session, profile, claude_session_id)) = self.switchable_session() else {
            return;
        };
        let prompt = ConfirmPrompt::new(
            "Next subscription",
            "Move this session to another subscription?\nThe prompt cache will rebuild.",
            vec![
                Choice::new(
                    'y',
                    "switch",
                    Some(ConfirmAction::SwitchSubscription {
                        session,
                        profile,
                        claude_session_id,
                    }),
                ),
                Choice::new('n', "cancel", None),
            ],
        );
        self.queue_confirm(prompt.ready());
    }

    /// The prompt was answered `y`: send the request.
    pub(super) fn start_switch(
        &mut self,
        profile: String,
        session: SessionId,
        claude_session_id: String,
    ) {
        self.effects.push(Effect::SwitchSubscription {
            profile_name: profile,
            session,
            claude_session_id,
        });
    }

    /// Called when a `SwitchSubscription` effect completes: say what the
    /// proxy answered, and look the credential up again so the status bar
    /// follows once the switch happens.
    pub fn on_switch_result(
        &mut self,
        session: SessionId,
        result: Result<SwitchOutcome, SwitchError>,
    ) {
        match result {
            Ok(o) => {
                let from = o.from.name().unwrap_or("current subscription");
                let to = o.to.as_ref().and_then(|t| t.name()).unwrap_or("next available");
                self.notify(format!("switch scheduled: {from} → {to} (on the next message)"));
                let active = self.active_view().map(|v| v.id) == Some(session);
                if active {
                    self.refresh_session_cred(0);
                }
            }
            Err(SwitchError::Api(e)) => self.notify(format!("switch failed: {e}")),
            Err(e) => self.notify(e.to_string()),
        }
    }

    /// Build the `SpawnSpec.env` for a proxy profile name. Fails when a
    /// profile is selected but cannot be resolved.
    pub(super) fn proxy_env_for(
        &self,
        proxy_name: Option<&str>,
    ) -> Result<Vec<(String, String)>, String> {
        crate::proxy::resolve::proxy_env_for(
            proxy_name,
            proxy_name.and_then(|n| self.proxy_config_cached(n)),
        )
    }

    // ── Geometry ──────────────────────────────────────────────────────────────

    /// The pane size as `(rows, cols)`.
    pub fn pane_size(&self) -> (u16, u16) {
        (
            self.height.saturating_sub(self.mode.chrome_rows()).max(1),
            self.width.max(1),
        )
    }

    pub fn active_view(&self) -> Option<&SessionView> {
        self.active.and_then(|i| self.sessions.get(i))
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    /// What state.json should contain now.
    pub fn to_state(&self) -> ClientState {
        ClientState {
            sessions: self.sessions.iter().map(SessionView::saved).collect(),
            active: self.active_view().map(|v| v.id),
            recent_dirs: self.recent_dirs.clone(),
            killed: self.killed.clone(),
            claude_skipped: self.claude_skipped.clone(),
        }
    }

    /// Show a transient notice in the status bar.
    pub fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            ticks_left: NOTICE_TICKS,
        });
        self.redraw = true;
    }

    /// The notice for a request an old daemon refused as an unknown op, or
    /// `None` for any other error. Shared by every feature that needs a newer
    /// daemon (terminals, git viewer, ...).
    pub(super) fn older_daemon_notice(host: &str, err: &std::io::Error) -> Option<String> {
        err.to_string()
            .starts_with("unsupported op")
            .then(|| format!("daemon on {host} is older: run `claudio daemon restart`"))
    }

    // ── Notifications ─────────────────────────────────────────────────────────

    /// Check for background sessions that entered a notification-worthy state
    /// since the last check, and populate `pending_notifs` with their labels.
    ///
    /// The caller (event loop) emits the OS notifications after each draw,
    /// outside the ratatui buffer, to avoid corrupting the terminal state.
    pub fn check_notifications(&mut self) {
        if !self.notify_enabled {
            return;
        }
        super::notifications::check_notifications(
            &self.sessions,
            self.active,
            &mut self.notified,
            &mut self.pending_notifs,
        );
    }

    /// Record a host-stats sample in the ring buffer.
    pub fn on_host_stats(&mut self, host: String, cpu_pct: f32, mem_used: u64, mem_total: u64) {
        let ring = self.host_stats.entry(host).or_default();
        ring.push_back(HostStatsSample { cpu_pct, mem_used, mem_total });
        while ring.len() > 30 {
            ring.pop_front();
        }
        self.redraw = true;
    }

    /// Advance animations, the clock and notice timeouts.
    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.now = crate::paths::unix_now();
        self.confirm_tick();
        self.refresh_session_cred(SESSION_CRED_POLL_SECS);
        if let Some(n) = &mut self.notice {
            n.ticks_left = n.ticks_left.saturating_sub(1);
            if n.ticks_left == 0 {
                self.notice = None;
            }
        }
        self.redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Incoming;
    use crate::proto::{RespawnSpec, SessionEvent, SessionInfo, SessionKind};
    use crate::tui::confirm::Choice;
    use crate::tui::state::SavedSession;
    use crate::tui::test_support::{
        self, alt, app_with, info, key, press, shell_info,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use uuid::Uuid;

    fn requests(effects: &[Effect]) -> Vec<&Msg> {
        effects
            .iter()
            .filter_map(|e| {
                if let Effect::Request { msg: m, .. } = e {
                    Some(m)
                } else {
                    None
                }
            })
            .collect()
    }

    /// The LOCAL rows on the wizard's first screen.
    fn local_rows(app: &App) -> Vec<String> {
        match &app.modal {
            Some(Modal::Wizard(w)) => w.host_step.as_ref().unwrap().local_items.clone(),
            _ => panic!("wizard not open"),
        }
    }

    #[test]
    fn wizard_lists_ssh_hosts_before_and_after_late_replies() {
        let mut cfg = test_support::config();
        cfg.ssh_hosts = vec!["prod-db".into(), "build-01".into()];
        let mut app = App::new(cfg);
        app.recover(&ClientState::default(), &[]);
        app.take_effects();
        let remote = |app: &App| match &app.modal {
            Some(Modal::Wizard(w)) => w.host_step.as_ref().unwrap().items.clone(),
            _ => panic!("wizard not open"),
        };
        assert_eq!(remote(&app), ["prod-db", "build-01"]);
        let gen = app.wizard_generation();
        app.on_reply(
            ReplyTo::Projects { host: "local".into(), gen },
            Ok(Msg::Projects { dirs: vec![] }),
        );
        assert_eq!(remote(&app), ["prod-db", "build-01"]);
    }

    #[test]
    fn wizard_offers_recent_local_dirs_that_are_not_open() {
        let mut cfg = test_support::config();
        cfg.recent_dirs
            .insert("local".into(), vec!["/srv/app".into(), "/srv/web".into()]);
        let mut app = App::new(cfg);
        // `/srv/app` is already open in a tab.
        app.recover(&ClientState::default(), &[info(Some(1), None)]);
        app.take_effects();

        app.open_wizard();
        assert_eq!(local_rows(&app), ["/srv/web"]);

        // Directories with recent claude activity follow once the daemon
        // answers, without repeating the open or already listed ones.
        let gen = app.wizard_generation();
        let dir = |path: &str| crate::proto::ProjectDir {
            path: path.into(),
            modified: 1,
            git: None,
            symlink: false,
            hidden: false,
        };
        app.on_reply(
            ReplyTo::Projects { host: "local".into(), gen },
            Ok(Msg::Projects {
                dirs: vec![dir("/srv/app"), dir("/srv/web"), dir("/srv/api")],
            }),
        );
        assert_eq!(local_rows(&app), ["/srv/web", "/srv/api"]);
    }

    #[test]
    fn recovery_respawns_dormant_sessions_and_attaches_the_saved_active() {
        let live = [info(Some(1), None), info(None, Some("c2"))];
        let saved = ClientState {
            active: Some(live[1].id),
            ..Default::default()
        };
        let mut app = test_support::app();
        app.recover(&saved, &live);
        let effects = app.take_effects();
        let reqs = requests(&effects);
        let Msg::Spawn(spec) = reqs[0] else {
            panic!("expected Spawn, got {reqs:?}")
        };
        assert_eq!((spec.id, spec.rows), (live[1].id, 27));
        assert_eq!(spec.args, ["--resume", "c2"]);
        assert_eq!(
            *reqs[1],
            Msg::Attach {
                id: live[1].id,
                rows: 27,
                cols: 100
            }
        );
        assert!(matches!(effects.last(), Some(Effect::Save)));
        assert_eq!(app.active, Some(1));
        assert!(app.modal.is_none());
    }

    #[test]
    fn no_sessions_opens_the_wizard() {
        let mut app = test_support::app();
        app.recover(&ClientState::default(), &[]);
        assert!(matches!(app.modal, Some(Modal::Wizard(_))));
        assert!(
            requests(&app.take_effects()).contains(&&Msg::RecentProjects {
                limit: PROJECTS_LIMIT
            })
        );
    }

    #[test]
    fn the_wizard_offers_the_ssh_hosts_and_asks_for_a_fresh_list() {
        let mut app = App::new(AppConfig {
            ssh_hosts: vec!["devbox".into()],
            ..test_support::config()
        });
        app.recover(&ClientState::default(), &[]);
        assert!(app.take_effects().iter().any(|e| matches!(e, Effect::LoadSshHosts)));
        let hosts = |app: &App| match &app.modal {
            Some(Modal::Wizard(w)) => w.host_step.as_ref().unwrap().items.clone(),
            _ => panic!("expected the wizard"),
        };
        assert_eq!(hosts(&app), ["devbox"]);
        // The fresh list replaces the untouched first screen...
        app.on_ssh_hosts(vec!["devbox".into(), "nas".into()]);
        assert_eq!(hosts(&app), ["devbox", "nas"]);
        // ...but not one the user is typing in.
        app.on_terminal(press(KeyCode::Char('n')));
        app.on_ssh_hosts(vec!["other".into()]);
        assert_eq!(hosts(&app), ["nas"]);
        assert_eq!(app.ssh_hosts, ["other"], "the next wizard has it");
    }

    #[test]
    fn switching_detaches_the_old_session_then_attaches_the_new() {
        let live = [info(Some(1), None), info(Some(2), None)];
        let mut app = app_with(&live);
        app.on_terminal(key(KeyCode::Left, KeyModifiers::ALT));
        let effects = app.take_effects();
        assert_eq!(
            requests(&effects),
            vec![
                &Msg::Detach { id: live[0].id },
                &Msg::Attach {
                    id: live[1].id,
                    rows: 27,
                    cols: 100
                }
            ]
        );
        assert_eq!(app.active, Some(1), "wraps around");
        assert!(!app.sessions[0].attached && app.sessions[1].attached);
        // Attached resets the mirror, but only for the attached session.
        app.on_incoming_from(
            "local",
            Incoming::Attached {
                id: live[1].id,
                rows: 10,
                cols: 40,
            },
        );
        assert_eq!(app.sessions[1].mirror.size(), (10, 40));
        app.on_incoming_from(
            "local",
            Incoming::Attached {
                id: live[0].id,
                rows: 5,
                cols: 5,
            },
        );
        assert_eq!(app.sessions[0].mirror.size(), (27, 100));
    }

    #[test]
    fn keys_go_to_the_active_session_unless_a_modal_is_open() {
        let live = [info(Some(1), None)];
        let mut app = app_with(&live);
        app.on_terminal(press(KeyCode::Char('h')));
        assert!(
            matches!(&app.take_effects()[..], [Effect::Input(id, b)] if *id == live[0].id && b == b"h")
        );
        app.on_terminal(alt('r'));
        app.on_terminal(press(KeyCode::Char('h')));
        assert!(app.take_effects().is_empty());
        assert!(matches!(&app.modal, Some(Modal::Rename { input, .. }) if input == "apph"));
    }

    #[test]
    fn rename_saves_and_empty_clears() {
        let mut app = app_with(&[info(Some(1), None)]);
        app.on_terminal(alt('r'));
        app.on_terminal(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.on_terminal(Event::Paste("api".into()));
        app.on_terminal(press(KeyCode::Enter));
        assert_eq!(app.sessions[0].name.as_deref(), Some("api"));
        // Rename sends the change to the daemon (durable journal) and saves state.
        let effects = app.take_effects();
        assert!(
            effects.iter().any(|e| matches!(e, Effect::Save)),
            "expected Save in effects"
        );
        assert!(
            effects.iter().any(|e| matches!(e, Effect::Request { msg: Msg::Rename { .. }, .. })),
            "expected Rename request in effects"
        );
        app.on_terminal(alt('r'));
        app.on_terminal(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.on_terminal(press(KeyCode::Enter));
        assert_eq!(app.sessions[0].name, None);
        assert_eq!(app.to_state().sessions[0].name, None);
    }

    #[test]
    fn close_persists_intent_before_kill_and_moves_to_a_neighbour() {
        let live = [
            info(Some(1), None),
            info(Some(2), None),
            info(Some(3), None),
        ];
        let mut app = app_with(&live);
        app.on_terminal(alt('x'));
        app.on_terminal(press(KeyCode::Char('n')));
        assert!(app.modal.is_none() && app.sessions.len() == 3, "n cancels");
        app.on_terminal(alt('x'));
        app.on_terminal(press(KeyCode::Char('y')));
        let effects = app.take_effects();
        let save = effects
            .iter()
            .position(|e| matches!(e, Effect::Save))
            .unwrap();
        let kill = effects
            .iter()
            .position(
                |e| matches!(e, Effect::Request { msg: Msg::Kill { id }, .. } if *id == live[0].id),
            )
            .unwrap();
        assert!(save < kill, "Save must come before Kill");
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.active_view().map(|v| v.id), Some(live[1].id));
        assert!(!app.to_state().sessions.iter().any(|s| s.id == live[0].id));
    }

    #[test]
    fn the_kill_prompt_names_the_session_and_esc_cancels_it() {
        let mut app = app_with(&[info(Some(1), None)]);
        app.on_terminal(alt('x'));
        match &app.modal {
            Some(Modal::Confirm(p)) => assert_eq!(p.text, "Kill session app?"),
            _ => panic!("expected the kill prompt"),
        }
        assert_eq!(hints(&app), ["[y] kill", "[n] cancel"]);
        app.on_terminal(press(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.sessions.len(), 1);
        assert!(app.killed.is_empty());
    }

    #[test]
    fn a_prompt_is_answered_by_plain_or_shifted_keys_only() {
        let mut app = app_with(&[info(Some(1), None)]);
        app.on_terminal(alt('x'));
        for mods in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            app.on_terminal(key(KeyCode::Char('y'), mods));
            app.on_terminal(key(KeyCode::Esc, mods));
            assert!(matches!(app.modal, Some(Modal::Confirm(_))), "{mods:?}");
        }
        app.on_terminal(key(KeyCode::Char('Y'), KeyModifiers::SHIFT));
        assert!(app.sessions.is_empty(), "Shift+Y kills");
    }

    #[test]
    fn tombstone_persisted_in_state_before_kill() {
        let live = [info(Some(1), None)];
        let mut app = app_with(&live);
        let id = live[0].id;
        app.on_terminal(alt('x'));
        app.on_terminal(press(KeyCode::Char('y')));
        // The tombstone must be in to_state() before Kill is sent.
        let effects = app.take_effects();
        let save_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::Save))
            .unwrap();
        let kill_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::Request { msg: Msg::Kill { id: k }, .. } if *k == id))
            .unwrap();
        assert!(save_pos < kill_pos, "tombstone must be saved before Kill");
        assert!(
            app.killed.iter().any(|t| t.id == id),
            "tombstone must be in app.killed"
        );
    }

    #[test]
    fn tombstone_cleared_on_kill_ok_reply() {
        let live = [info(Some(1), None)];
        let mut app = app_with(&live);
        let id = live[0].id;
        app.on_terminal(alt('x'));
        app.on_terminal(press(KeyCode::Char('y')));
        app.take_effects();
        assert!(!app.killed.is_empty());
        // Simulate a successful Kill reply.
        app.on_reply(ReplyTo::Kill(id), Ok(Msg::Pong));
        assert!(
            app.killed.is_empty(),
            "tombstone must be cleared after Kill OK"
        );
    }

    #[test]
    fn tombstone_cleared_on_no_such_session_reply() {
        let live = [info(Some(1), None)];
        let mut app = app_with(&live);
        let id = live[0].id;
        app.on_terminal(alt('x'));
        app.on_terminal(press(KeyCode::Char('y')));
        app.take_effects();
        // Simulate "no such session" error.
        app.on_reply(
            ReplyTo::Kill(id),
            Ok(Msg::Error {
                message: "no such session".into(),
            }),
        );
        assert!(
            app.killed.is_empty(),
            "tombstone cleared on 'no such session'"
        );
    }

    #[test]
    fn dropped_kill_is_resent_on_recovery() {
        // Simulate: session was killed, tombstone saved, but Kill never delivered.
        let id = Uuid::new_v4();
        let mut saved = ClientState::default();
        saved.killed.push(KillTombstone {
            host: "local".into(),
            id,
        });
        // The daemon still lists the session as live.
        let live = [SessionInfo {
            id,
            ..info(Some(42), None)
        }];
        let mut app = test_support::app();
        app.recover(&saved, &live);
        let effects = app.take_effects();
        // Kill must be re-sent.
        let kill_re_sent = effects
            .iter()
            .any(|e| matches!(e, Effect::Request { msg: Msg::Kill { id: k }, .. } if *k == id));
        assert!(kill_re_sent, "dropped Kill must be re-sent on recovery");
        // The tombstoned session must NOT appear in the session list.
        assert!(
            !app.sessions.iter().any(|v| v.id == id),
            "tombstoned session must not appear"
        );
    }

    /// A remote tab outlives the manager's restart: kept, detached, until its
    /// host connects, then respawned (its daemon restarted meanwhile) with
    /// its proxy profile, in its place, and re-attached as the active tab.
    #[test]
    fn remote_tabs_survive_a_restart_with_their_proxy() {
        let local = info(Some(1), None);
        let remote = SavedSession {
            id: Uuid::new_v4(),
            name: None,
            cwd: "/srv/remote".into(),
            host: "devbox".into(),
            claude_session_id: Some("c9".into()),
            created_at: 1,
            proxy: Some("work".into()),
            kind: SessionKind::Claude,
        };
        let saved = ClientState {
            sessions: vec![
                remote.clone(),
                SessionView::from_info("local", &local, (1, 1)).saved(),
            ],
            active: Some(remote.id),
            ..Default::default()
        };
        let mut app = test_support::app();
        app.recover(&saved, std::slice::from_ref(&local));
        let effects = app.take_effects();
        assert!(
            !requests(&effects)
                .iter()
                .any(|m| matches!(m, Msg::Attach { .. })),
            "nothing attaches before devbox connects: {effects:?}"
        );
        let ids: Vec<_> = app.sessions.iter().map(|v| v.id).collect();
        assert_eq!(ids, [remote.id, local.id]);
        assert_eq!((app.active, app.sessions[0].attached), (Some(0), false));
        assert_eq!(app.to_state().sessions[0], remote, "state.json keeps the tab");

        // devbox's daemon restarted: the session is dormant.
        let dormant = SessionInfo {
            id: remote.id,
            cwd: remote.cwd.clone(),
            ..info(None, Some("c9"))
        };
        app.recover_host("devbox", &[dormant]);
        let effects = app.take_effects();
        assert!(
            effects.iter().any(|e| matches!(e,
                Effect::SpawnWithProxy { host, msg: Msg::Spawn(s), proxy_name, .. }
                    if host == "devbox" && proxy_name == "work" && s.args == ["--resume", "c9"])),
            "respawned with its proxy: {effects:?}"
        );
        let attach = Msg::Attach {
            id: remote.id,
            rows: 27,
            cols: 100,
        };
        assert!(requests(&effects).contains(&&attach));
        assert_eq!(app.sessions[0].id, remote.id, "in its place");
        assert_eq!(app.sessions[0].proxy.as_deref(), Some("work"));
        assert_eq!((app.active, app.sessions[0].attached), (Some(0), true));
    }

    #[test]
    fn recover_host_does_not_wipe_other_hosts_sessions() {
        // Set up two sessions: one local, one remote.
        let local_id = Uuid::new_v4();
        let remote_id = Uuid::new_v4();

        let local_live = vec![SessionInfo {
            id: local_id,
            ..info(Some(1), None)
        }];
        let mut app = test_support::app();
        app.recover(&ClientState::default(), &local_live);
        app.take_effects();
        assert_eq!(app.sessions.len(), 1);

        // Manually add a fake "remote" session (simulating a previous recover_host call).
        app.sessions.push(SessionView::new(
            remote_id,
            "myserver",
            "/remote",
            SessionKind::Claude,
            (28, 100),
        ));
        assert_eq!(app.sessions.len(), 2);

        // Now simulate a second local reconnect: recover_host("local", ...) must
        // NOT remove the "myserver" session.
        app.recover_host("local", &local_live);
        app.take_effects();

        // The remote session must still be there.
        assert!(
            app.sessions.iter().any(|v| v.id == remote_id),
            "remote session must survive local recover_host"
        );
        assert!(
            app.sessions.iter().any(|v| v.id == local_id),
            "local session must still be there"
        );
    }

    #[test]
    fn attention_cycles_over_sessions_that_want_it() {
        let mut live = [
            info(Some(1), None),
            info(Some(2), None),
            info(Some(3), None),
        ];
        live[2].state = SessionState::NeedsApproval;
        let mut app = app_with(&live);
        app.on_terminal(alt('a'));
        assert_eq!(app.active, Some(2));
        app.on_terminal(alt('a'));
        assert_eq!(app.active, Some(2), "nothing else wants attention");
        let event = SessionEvent::State {
            state: SessionState::NeedsInput,
        };
        app.on_incoming_from(
            "local",
            Incoming::Event {
                id: live[0].id,
                event,
            },
        );
        app.on_terminal(alt('a'));
        assert_eq!(app.active, Some(0));
    }

    fn alt_shift(c: char) -> Event {
        key(KeyCode::Char(c), KeyModifiers::ALT | KeyModifiers::SHIFT)
    }

    #[test]
    fn alt_shift_digit_goes_to_that_session() {
        let live: Vec<_> = (1..=10).map(|p| info(Some(p), None)).collect();
        let mut app = app_with(&live);
        assert_eq!(app.active, Some(0));
        app.on_terminal(alt_shift('3'));
        assert_eq!(app.active, Some(2), "Alt+Shift+3 is tab 3");
        assert!(app.sessions[2].attached && !app.sessions[0].attached);
        app.on_terminal(alt_shift('0'));
        assert_eq!(app.active, Some(9), "Alt+Shift+0 is tab 10");
        app.on_terminal(alt_shift('1'));
        assert_eq!(app.active, Some(0));
        assert!(app.notice.is_none());
    }

    #[test]
    fn alt_shift_digit_without_that_session_only_notifies() {
        let mut app = app_with(&[info(Some(1), None), info(Some(2), None)]);
        app.on_terminal(alt_shift('5'));
        assert_eq!(app.active, Some(0));
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("no session 5")
        );
        app.on_terminal(alt_shift('0'));
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("no session 10")
        );
        assert_eq!(app.active, Some(0));
    }

    #[test]
    fn alt_digit_and_shift_digit_are_forwarded_to_claude() {
        let live = [info(Some(1), None), info(Some(2), None)];
        let mut app = app_with(&live);
        app.take_effects();
        app.on_terminal(alt('2'));
        app.on_terminal(key(KeyCode::Char('@'), KeyModifiers::SHIFT));
        assert_eq!(app.active, Some(0), "neither combo switches tabs");
        let inputs = app
            .take_effects()
            .iter()
            .filter(|e| matches!(e, Effect::Input(id, _) if *id == live[0].id))
            .count();
        assert_eq!(inputs, 2, "both keys reach the active session");
    }

    #[test]
    fn wizard_spawns_into_the_chosen_dir_and_records_it() {
        let mut app = app_with(&[]);
        // Step 0: press Enter to select "local" host (empty host filter → picks local).
        app.on_terminal(press(KeyCode::Enter));
        // The directory step opens in browse mode at `~/`: the home listing is requested.
        let effects = app.take_effects();
        assert!(requests(&effects)
            .iter()
            .any(|m| **m == Msg::ListDir { path: "/home/u".into() }));
        // Step 1: replace the `~/` input with "/w", then press Enter.
        app.on_terminal(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "/w".chars() {
            app.on_terminal(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_terminal(press(KeyCode::Enter));
        let effects = app.take_effects();
        assert!(requests(&effects)
            .iter()
            .any(|m| **m == Msg::ListClaudeSessions { cwd: "/w".into() }));
        let gen = app.wizard_generation();
        let reply = Msg::ClaudeSessions {
            cwd: "/w".into(),
            sessions: vec![],
        };
        app.on_reply(ReplyTo::ClaudeSessions { host: "local".into(), cwd: "/w".into(), gen }, Ok(reply));
        let effects = app.take_effects();
        let reqs = requests(&effects);
        let Msg::Spawn(spec) = reqs[0] else {
            panic!("expected Spawn, got {reqs:?}")
        };
        assert_eq!((spec.cwd.as_str(), spec.args.len()), ("/w", 0));
        assert_eq!(
            *reqs[1],
            Msg::Attach {
                id: spec.id,
                rows: 27,
                cols: 100
            }
        );
        assert!(app.modal.is_none());
        assert_eq!(
            app.recent_dirs.get("local").map(Vec::as_slice).unwrap_or(&[]),
            &["/w".to_owned()][..]
        );
        assert_eq!(app.sessions[0].label(), "w");
    }

    #[test]
    fn events_update_and_add_sessions() {
        let live = [info(Some(1), None)];
        let mut app = app_with(&live);
        let id = live[0].id;
        app.on_incoming_from(
            "local",
            Incoming::Event {
                id,
                event: SessionEvent::Title {
                    title: "Refactor".into(),
                },
            },
        );
        let event = SessionEvent::ClaudeSession {
            claude_session_id: "c9".into(),
        };
        app.on_incoming_from("local", Incoming::Event { id, event });
        assert_eq!(app.sessions[0].label(), "Refactor");
        assert_eq!(
            app.to_state().sessions[0].claude_session_id.as_deref(),
            Some("c9")
        );
        let other = info(Some(5), None);
        let other_id = other.id;
        app.on_incoming_from(
            "local",
            Incoming::Event {
                id: other_id,
                event: SessionEvent::Created {
                    info: other.clone(),
                },
            },
        );
        assert_eq!(app.sessions.len(), 2);
        // A session another client created gets the host it came from.
        assert_eq!(app.sessions[1].host, "local");
        app.on_incoming_from(
            "local",
            Incoming::Event {
                id: other_id,
                event: SessionEvent::Removed,
            },
        );
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.active, Some(0));
    }

    #[test]
    fn plain_mode_sessions_never_become_tabs() {
        let ephemeral = |info: SessionInfo| SessionInfo {
            ephemeral: true,
            ..info
        };
        let live = [
            info(Some(1), None),
            ephemeral(info(Some(2), None)),
            // Dormant: it must not be resumed either.
            ephemeral(info(None, Some("c3"))),
        ];
        let mut app = test_support::app();
        app.recover(&ClientState::default(), &live);
        assert_eq!(app.sessions.len(), 1);
        let effects = app.take_effects();
        assert!(!requests(&effects).iter().any(|m| matches!(m, Msg::Spawn(_))));

        let other = ephemeral(info(Some(4), None));
        let created = SessionEvent::Created {
            info: other.clone(),
        };
        app.on_incoming_from("local", Incoming::Event { id: other.id, event: created });
        assert_eq!(app.sessions.len(), 1);
        app.recover_host("local", &[live[0].clone(), other]);
        assert_eq!(app.sessions.len(), 1, "nor after a reconnect");
    }

    #[test]
    fn a_manager_spawn_is_not_ephemeral() {
        let mut app = app_with(&[info(Some(1), Some("c1"))]);
        app.spawn("local".into(), "/w".into(), None, None);
        let effects = app.take_effects();
        let spawn = requests(&effects).into_iter().find_map(|m| match m {
            Msg::Spawn(spec) => Some(spec.clone()),
            _ => None,
        });
        assert!(!spawn.expect("a Spawn").ephemeral);
    }

    #[test]
    fn a_fresh_reset_drops_the_saved_conversation() {
        let live = [info(Some(1), Some("c1"))];
        let mut app = app_with(&live);
        let id = live[0].id;
        let event = SessionEvent::ClaudeSessionCleared;
        app.on_incoming_from("local", Incoming::Event { id, event });
        let saved = app.to_state();
        assert_eq!(saved.sessions[0].claude_session_id, None);

        // Recovering it dormant, with the daemon knowing no conversation
        // either, starts a new one instead of resuming "c1".
        let dormant = SessionInfo {
            id,
            ..info(None, None)
        };
        let merged = crate::tui::state::merge_for_host("local", &saved, &[dormant]);
        assert_eq!(merged[0].respawn, Some(vec![]));
    }

    #[test]
    fn on_incoming_from_remote_tags_created_sessions_correctly() {
        let mut app = test_support::app();
        app.recover(&ClientState::default(), &[]);
        // Close wizard.
        app.modal = None;
        app.take_effects();

        let new_session = info(Some(99), None);
        let new_id = new_session.id;
        app.on_incoming_from(
            "myserver",
            Incoming::Event {
                id: new_id,
                event: SessionEvent::Created { info: new_session },
            },
        );
        let sv = app.sessions.iter().find(|v| v.id == new_id).unwrap();
        assert_eq!(
            sv.host, "myserver",
            "remote session must have host=myserver"
        );
    }

    #[test]
    fn local_disconnect_does_not_affect_remote_sessions() {
        let local_id = Uuid::new_v4();
        let remote_id = Uuid::new_v4();
        let mut app = test_support::app();
        for (id, host) in [(local_id, "local"), (remote_id, "myserver")] {
            app.sessions.push(SessionView {
                attached: true,
                ..SessionView::new(id, host, "/w", SessionKind::Claude, (28, 100))
            });
        }
        app.on_incoming_from("local", Incoming::Disconnected);
        // Local session detached.
        assert!(
            !app.sessions
                .iter()
                .find(|v| v.id == local_id)
                .unwrap()
                .attached
        );
        // Remote session NOT affected.
        assert!(
            app.sessions
                .iter()
                .find(|v| v.id == remote_id)
                .unwrap()
                .attached
        );
    }

    // ── Overview ──────────────────────────────────────────────────────────────

    #[test]
    fn overview_opens_on_alt_g_and_closes_on_esc() {
        let live = [info(Some(1), None), info(Some(2), None)];
        let mut app = app_with(&live);
        app.on_terminal(alt('g'));
        assert!(matches!(app.modal, Some(Modal::Overview { .. })));
        app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.modal.is_none());
    }

    #[test]
    fn overview_enter_activates_selected_session() {
        let live = [info(Some(1), None), info(Some(2), None)];
        let mut app = app_with(&live);
        assert_eq!(app.active, Some(0));
        app.on_terminal(alt('g'));
        // Navigate down to index 1.
        app.modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(app.modal, Some(Modal::Overview { selected: 1, .. })));
        // Press Enter — should activate session 1.
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal.is_none());
        assert_eq!(app.active, Some(1));
    }

    // ── Help ──────────────────────────────────────────────────────────────────

    #[test]
    fn help_opens_on_alt_h_and_any_key_closes() {
        let live = [info(Some(1), None)];
        let mut app = app_with(&live);
        app.on_terminal(alt('h'));
        assert!(matches!(app.modal, Some(Modal::Help)));
        // Any key closes help.
        app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.modal.is_none());
    }

    // ── Notifications ─────────────────────────────────────────────────────────

    #[test]
    fn notification_emitted_once_per_state_change() {
        let mut live = [info(Some(1), None), info(Some(2), None)];
        live[1].state = SessionState::NeedsApproval;
        let mut app = app_with(&live);
        // Session 1 (index 1) wants attention; session 0 is active.
        assert_eq!(app.active, Some(0));

        // First check: should produce one notification.
        app.check_notifications();
        assert_eq!(app.pending_notifs.len(), 1);
        let _ = app.pending_notifs.drain(..);

        // Second check without state change: no new notification.
        app.check_notifications();
        assert!(app.pending_notifs.is_empty(), "debounced: no second notif");

        // State change: new notification.
        let ev = SessionEvent::State {
            state: SessionState::NeedsInput,
        };
        app.on_incoming_from(
            "local",
            Incoming::Event {
                id: live[1].id,
                event: ev,
            },
        );
        app.check_notifications();
        assert_eq!(app.pending_notifs.len(), 1, "new state → new notification");
    }

    #[test]
    fn active_session_never_notifies() {
        let mut live = [info(Some(1), None)];
        live[0].state = SessionState::NeedsApproval;
        let mut app = app_with(&live);
        // Session 0 is both active and wants attention.
        assert_eq!(app.active, Some(0));
        app.check_notifications();
        assert!(
            app.pending_notifs.is_empty(),
            "active session must not notify"
        );
    }

    #[test]
    fn notifications_disabled_when_flag_is_off() {
        let mut live = [info(Some(1), None), info(Some(2), None)];
        live[1].state = SessionState::NeedsApproval;
        let mut app = App::new(AppConfig {
            notify: false,
            ..test_support::config()
        });
        app.recover(&ClientState::default(), &live);
        app.take_effects();
        app.check_notifications();
        assert!(app.pending_notifs.is_empty(), "notifications disabled");
    }

    // ── Terminal tabs ─────────────────────────────────────────────────────────

    #[test]
    fn terminal_opens_right_after_the_active_tab_and_is_activated() {
        let live = [info(Some(1), None), info(Some(2), None), info(Some(3), None)];
        let mut app = app_with(&live);
        app.on_terminal(alt('c'));
        let effects = app.take_effects();
        let spawn = requests(&effects)
            .into_iter()
            .find_map(|m| match m {
                Msg::SpawnShell(s) => Some(s.clone()),
                _ => None,
            })
            .expect("a SpawnShell request");
        assert_eq!((spawn.cwd.as_str(), spawn.rows, spawn.cols), ("/srv/app", 27, 100));
        assert_eq!(app.sessions.len(), 4);
        assert_eq!(app.sessions[1].id, spawn.id, "inserted after tab 1");
        assert_eq!(app.sessions[1].kind, SessionKind::Shell);
        assert_eq!(app.active, Some(1));
        assert!(requests(&effects).contains(&&Msg::Detach { id: live[0].id }));
        assert!(requests(&effects).contains(&&Msg::Attach { id: spawn.id, rows: 27, cols: 100 }));

        // From a terminal, the next one goes right after *it*, not at the end.
        app.on_terminal(alt('c'));
        assert_eq!(app.active, Some(2));
        assert_eq!(app.sessions[2].kind, SessionKind::Shell);
        assert_eq!(app.sessions[3].id, live[1].id);
    }

    #[test]
    fn terminal_without_a_session_uses_local_home() {
        let mut app = app_with(&[]);
        app.modal = None;
        app.on_terminal(alt('c'));
        let effects = app.take_effects();
        assert!(matches!(
            &effects[0],
            Effect::Request { host, msg: Msg::SpawnShell(s), .. } if host == "local" && s.cwd == "/home/u"
        ));
        assert_eq!((app.sessions.len(), app.active), (1, Some(0)));
    }

    #[test]
    fn terminal_ignores_the_active_sessions_proxy() {
        let mut app = app_with(&[info(Some(1), None)]);
        app.sessions[0].proxy = Some("work".into());
        app.on_terminal(alt('c'));
        let effects = app.take_effects();
        assert!(!effects.iter().any(|e| matches!(e, Effect::SpawnWithProxy { .. })));
        assert_eq!(app.sessions[1].proxy, None);
    }

    #[test]
    fn terminal_labels_ignore_the_title_and_keep_a_rename() {
        let mut app = app_with(&[shell_info(Some(1)), info(Some(2), None)]);
        assert_eq!(app.sessions[0].label(), "term");
        app.sessions[0].title = Some("user@host: ~/src".into());
        assert_eq!(app.sessions[0].label(), "term", "prompt titles do not rename it");
        assert_eq!(crate::tui::ui::tab_label(&app.sessions[0]), "term@local");
        assert_eq!(crate::tui::ui::tab_label(&app.sessions[1]), "app");
        app.sessions[0].name = Some("build".into());
        app.sessions[0].host = "devbox".into();
        assert_eq!(crate::tui::ui::tab_label(&app.sessions[0]), "build@devbox");
        assert_eq!(crate::tui::ui::view_glyph(&app.sessions[0], 0).0, "$");
    }

    #[test]
    fn dormant_terminal_respawns_as_a_shell_without_resume_or_proxy() {
        let mut live = [shell_info(None), info(None, Some("c2"))];
        live[0].claude_session_id = Some("stale".into());
        let saved = ClientState {
            sessions: vec![SavedSession {
                id: live[0].id,
                name: None,
                cwd: "/srv/app".into(),
                host: "local".into(),
                claude_session_id: None,
                created_at: 1,
                proxy: Some("work".into()),
                kind: SessionKind::Shell,
            }],
            ..Default::default()
        };
        let mut app = test_support::app();
        app.recover(&saved, &live);
        let effects = app.take_effects();
        assert!(
            matches!(&effects[0], Effect::Request { msg: Msg::SpawnShell(s), .. } if s.id == live[0].id),
            "{effects:?}"
        );
        assert!(matches!(&effects[1], Effect::Request { msg: Msg::Spawn(s), .. } if s.args == ["--resume", "c2"]));
        assert_eq!(app.sessions[0].kind, SessionKind::Shell);
        assert_eq!(app.to_state().sessions[0].kind, SessionKind::Shell);
    }

    #[test]
    fn an_older_daemon_refusing_a_terminal_drops_the_tab_with_a_notice() {
        let mut app = app_with(&[info(Some(1), None)]);
        app.on_terminal(alt('c'));
        let id = app.sessions[1].id;
        app.take_effects();
        let err = std::io::Error::other("unsupported op: spawn_shell");
        app.on_reply(ReplyTo::Spawned(id), Err(err));
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.active, Some(0));
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("daemon on local is older: run `claudio daemon restart`")
        );
        // Any other failure keeps the (dead) tab.
        app.on_terminal(alt('c'));
        let id = app.sessions[1].id;
        app.on_reply(ReplyTo::Spawned(id), Err(std::io::Error::other("no such directory")));
        assert_eq!(app.sessions[1].state, SessionState::Exited);
    }

    fn respawn_of(effects: &[Effect]) -> Option<(&RespawnSpec, &ReplyTo)> {
        effects.iter().find_map(|e| match e {
            Effect::Request {
                msg: Msg::Respawn(spec),
                to,
                ..
            } => Some((spec, to)),
            _ => None,
        })
    }

    fn hints(app: &App) -> Vec<String> {
        match &app.modal {
            Some(Modal::Confirm(p)) => p.choices.iter().map(Choice::hint).collect(),
            _ => panic!("expected a confirm prompt"),
        }
    }

    #[test]
    fn reset_without_a_session_is_a_notice() {
        let mut app = app_with(&[]);
        app.modal = None;
        app.on_terminal(alt('e'));
        assert!(app.modal.is_none());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("no session to reset")
        );
    }

    #[test]
    fn reset_asks_how_to_restart_a_claude_tab() {
        let mut app = app_with(&[info(Some(1), Some("c1"))]);
        let id = app.sessions[0].id;
        app.on_terminal(alt('e'));
        assert_eq!(
            hints(&app),
            [
                "[r] restart & resume this conversation",
                "[n] new conversation",
                "Esc cancel"
            ]
        );
        assert!(respawn_of(&app.take_effects()).is_none(), "nothing before the answer");

        // `r` keeps the conversation, answered on the same tab.
        app.on_terminal(press(KeyCode::Char('r')));
        assert!(app.modal.is_none());
        let effects = app.take_effects();
        let (spec, to) = respawn_of(&effects).expect("a Respawn request");
        assert_eq!((spec.id, spec.fresh, (spec.rows, spec.cols)), (id, false, (27, 100)));
        assert_eq!(*to, ReplyTo::Respawned(id));
        assert_eq!(app.sessions.len(), 1);

        // `n` starts a new conversation.
        app.on_terminal(alt('e'));
        app.on_terminal(press(KeyCode::Char('N')));
        let effects = app.take_effects();
        assert!(respawn_of(&effects).is_some_and(|(spec, _)| spec.fresh));
    }

    #[test]
    fn escape_cancels_a_reset() {
        let mut app = app_with(&[info(Some(1), Some("c1"))]);
        app.on_terminal(alt('e'));
        app.on_terminal(press(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(respawn_of(&app.take_effects()).is_none());
    }

    #[test]
    fn a_terminal_only_offers_a_restart() {
        let mut app = app_with(&[shell_info(Some(1))]);
        app.on_terminal(alt('e'));
        assert_eq!(hints(&app), ["[r] restart shell", "Esc cancel"]);
        app.on_terminal(press(KeyCode::Char('n')));
        assert!(app.modal.is_some(), "there is no `n` here");
        app.on_terminal(press(KeyCode::Char('r')));
        let effects = app.take_effects();
        assert!(respawn_of(&effects).is_some_and(|(spec, _)| !spec.fresh));
    }

    #[test]
    fn a_proxied_reset_waits_for_the_proxy_config() {
        let mut app = app_with(&[info(Some(1), Some("c1"))]);
        app.sessions[0].proxy = Some("work".into());
        let id = app.sessions[0].id;
        app.reset_session(id, false);
        let effects = app.take_effects();
        assert!(respawn_of(&effects).is_none(), "not sent before the env exists");
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::SpawnWithProxy { msg: Msg::Respawn(spec), proxy_name, to, .. }
                if spec.id == id && proxy_name == "work" && *to == ReplyTo::Respawned(id)
        )));
    }

    #[test]
    fn a_successful_respawn_reattaches_the_active_tab() {
        let mut app = app_with(&[info(Some(1), None)]);
        let id = app.sessions[0].id;
        app.on_reply(ReplyTo::Respawned(id), Ok(Msg::Spawned { id, pid: Some(9) }));
        let effects = app.take_effects();
        assert!(requests(&effects)
            .iter()
            .any(|m| matches!(m, Msg::Attach { id: i, .. } if *i == id)));
        assert!(app.sessions[0].attached);
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn a_failed_respawn_keeps_the_tab_and_explains() {
        let mut app = app_with(&[info(Some(1), None)]);
        let id = app.sessions[0].id;
        let notice = |app: &App| app.notice.as_ref().map(|n| n.text.clone());

        let old = std::io::Error::other("unsupported op: respawn");
        app.on_reply(ReplyTo::Respawned(id), Err(old));
        assert_eq!(
            notice(&app).as_deref(),
            Some("daemon on local is older: run `claudio daemon restart`")
        );
        let other = std::io::Error::other("working directory /gone does not exist");
        app.on_reply(ReplyTo::Respawned(id), Err(other));
        assert_eq!(
            notice(&app).as_deref(),
            Some("could not restart: working directory /gone does not exist")
        );
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn terminals_are_never_picked_for_attention() {
        let mut live = [info(Some(1), None), shell_info(Some(2)), info(Some(3), None)];
        live[2].state = SessionState::NeedsInput;
        let mut app = app_with(&live);
        app.on_terminal(alt('a'));
        assert_eq!(app.active, Some(2));
        assert!(!app.sessions[1].state.wants_attention());
    }

    // ── Proxy credential of the active session ────────────────────────────────

    fn cred_fetches(effects: &[Effect]) -> Vec<(&str, SessionId, &str)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::FetchSessionCredential {
                    profile_name,
                    session,
                    claude_session_id,
                } => Some((profile_name.as_str(), *session, claude_session_id.as_str())),
                _ => None,
            })
            .collect()
    }

    fn work_max() -> SessionCredential {
        SessionCredential {
            credential: crate::proxy::api::CredentialInfo {
                label: "work-max".into(),
                plan: "max".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Two claude sessions on proxy profile `p`; the first is active.
    fn proxied_app() -> (App, [SessionInfo; 2]) {
        let live = [info(Some(1), Some("c1")), info(Some(2), Some("c2"))];
        let mut app = app_with(&live);
        for v in &mut app.sessions {
            v.proxy = Some("p".into());
        }
        assert_eq!(app.active, Some(0));
        (app, live)
    }

    #[test]
    fn active_proxied_session_fetches_its_credential_on_activation() {
        let (mut app, live) = proxied_app();
        app.on_terminal(key(KeyCode::Right, KeyModifiers::ALT));
        let effects = app.take_effects();
        assert_eq!(cred_fetches(&effects), vec![("p", live[1].id, "c2")]);
        // Back at the first one soon after: no second request inside the gap.
        app.on_terminal(key(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(cred_fetches(&app.take_effects()), vec![("p", live[0].id, "c1")]);
        app.on_terminal(key(KeyCode::Right, KeyModifiers::ALT));
        assert!(
            cred_fetches(&app.take_effects()).is_empty(),
            "c2 was asked a moment ago"
        );
    }

    #[test]
    fn a_proxied_session_fetches_the_model_catalogue_once() {
        let (mut app, _) = proxied_app();
        let models_fetches = |effects: &[Effect]| {
            effects
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        Effect::FetchProxyStats {
                            profile_name,
                            windows,
                            models: true
                        } if profile_name == "p" && windows.is_empty()
                    )
                })
                .count()
        };
        app.refresh_session_cred(0);
        assert_eq!(models_fetches(&app.take_effects()), 1);
        // Later lookups, on any session of the profile: not again.
        app.refresh_session_cred(0);
        app.on_terminal(key(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(models_fetches(&app.take_effects()), 0);
        // A profile whose catalogue is cached already is never asked.
        let mut app = proxied_app().0;
        app.proxy_status.insert(
            "p".into(),
            ProxyStatus {
                models: Some(Default::default()),
                ..Default::default()
            },
        );
        app.refresh_session_cred(0);
        assert_eq!(models_fetches(&app.take_effects()), 0);
    }

    #[test]
    fn credential_is_polled_every_30_seconds_while_active() {
        let (mut app, live) = proxied_app();
        app.refresh_session_cred(0);
        assert_eq!(cred_fetches(&app.take_effects()), vec![("p", live[0].id, "c1")]);
        app.on_tick();
        assert!(cred_fetches(&app.take_effects()).is_empty(), "too soon");
        app.session_creds.get_mut(&live[0].id).unwrap().asked_at -= 29;
        app.on_tick();
        assert!(cred_fetches(&app.take_effects()).is_empty(), "29 s");
        app.session_creds.get_mut(&live[0].id).unwrap().asked_at -= 2;
        app.on_tick();
        assert_eq!(cred_fetches(&app.take_effects()), vec![("p", live[0].id, "c1")]);
        // Only the active session is polled.
        assert!(!app.session_creds.contains_key(&live[1].id));
    }

    #[test]
    fn opening_the_stats_popup_refreshes_the_credential() {
        let (mut app, live) = proxied_app();
        app.refresh_session_cred(0);
        app.take_effects();
        app.session_creds.get_mut(&live[0].id).unwrap().asked_at -= 6;
        app.open_proxy_stats();
        let effects = app.take_effects();
        assert_eq!(cred_fetches(&effects), vec![("p", live[0].id, "c1")]);
    }

    fn press_char(app: &mut App, c: char) {
        app.on_terminal(key(KeyCode::Char(c), KeyModifiers::NONE));
    }

    fn notice_text(app: &App) -> String {
        app.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default()
    }

    #[test]
    fn n_in_the_stats_popup_is_offered_only_to_proxied_sessions_with_an_id() {
        let (mut app, _) = proxied_app();
        app.open_proxy_stats();
        assert!(matches!(&app.modal, Some(Modal::ProxyStats(v)) if v.can_switch));
        press_char(&mut app, 'n');
        match &app.modal {
            Some(Modal::Confirm(p)) => {
                assert!(p.text.starts_with("Move this session to another subscription?"));
                let hints: Vec<_> = p.choices.iter().map(Choice::hint).collect();
                assert_eq!(hints, ["[y] switch", "[n] cancel"]);
            }
            _ => panic!("expected the confirm prompt"),
        }

        // No conversation id yet: the key does nothing.
        let (mut app, _) = proxied_app();
        app.sessions[0].claude_session_id = None;
        app.open_proxy_stats();
        assert!(matches!(&app.modal, Some(Modal::ProxyStats(v)) if !v.can_switch));
        press_char(&mut app, 'n');
        assert!(matches!(app.modal, Some(Modal::ProxyStats(_))));

        // A direct session, looking at the default profile's stats.
        let (mut app, _) = proxied_app();
        app.sessions[0].proxy = None;
        app.proxy_default = Some("p".into());
        app.open_proxy_stats();
        assert!(matches!(&app.modal, Some(Modal::ProxyStats(v)) if !v.can_switch));
    }

    #[test]
    fn confirming_the_switch_emits_the_effect_and_cancel_does_not() {
        let (mut app, live) = proxied_app();
        app.open_proxy_stats();
        press_char(&mut app, 'n');
        app.take_effects();
        press_char(&mut app, 'n');
        assert!(app.modal.is_none());
        assert!(!app
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::SwitchSubscription { .. })));

        app.open_proxy_stats();
        press_char(&mut app, 'n');
        press_char(&mut app, 'y');
        let effects = app.take_effects();
        assert!(effects.iter().any(|e| matches!(e,
            Effect::SwitchSubscription { profile_name, session, claude_session_id }
                if profile_name == "p" && *session == live[0].id && claude_session_id == "c1")));
    }

    #[test]
    fn switch_outcomes_become_notices() {
        use crate::proxy::api::{ApiError, SwitchCredential};
        let cred = |l: &str| SwitchCredential {
            label: l.into(),
            ..Default::default()
        };
        let (mut app, live) = proxied_app();
        let id = live[0].id;
        app.on_switch_result(
            id,
            Ok(SwitchOutcome {
                from: cred("work-max"),
                to: Some(cred("personal")),
                ..Default::default()
            }),
        );
        assert_eq!(
            notice_text(&app),
            "switch scheduled: work-max → personal (on the next message)"
        );
        // The credential is looked up again.
        assert_eq!(cred_fetches(&app.take_effects()), vec![("p", id, "c1")]);
        app.on_switch_result(
            id,
            Ok(SwitchOutcome {
                from: cred("work-max"),
                ..Default::default()
            }),
        );
        assert_eq!(
            notice_text(&app),
            "switch scheduled: work-max → next available (on the next message)"
        );
        app.on_switch_result(id, Err(SwitchError::UnknownSession));
        assert_eq!(
            notice_text(&app),
            "the proxy does not know this session yet — send a message first"
        );
        app.on_switch_result(
            id,
            Err(SwitchError::NoAlternative("no other subscription available".into())),
        );
        assert_eq!(notice_text(&app), "no other subscription available");
        app.on_switch_result(id, Err(SwitchError::TooOld));
        assert_eq!(notice_text(&app), "proxy too old for switching");
        app.on_switch_result(
            id,
            Err(SwitchError::Api(ApiError::Http(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
            ))),
        );
        assert!(notice_text(&app).starts_with("switch failed: "));
    }

    #[test]
    fn no_credential_fetch_for_direct_sessions_terminals_or_missing_ids() {
        // Direct session.
        let live = [info(Some(1), Some("c1"))];
        let mut app = app_with(&live);
        app.refresh_session_cred(0);
        app.on_tick();
        assert!(cred_fetches(&app.take_effects()).is_empty());
        // Proxied but no claude session id yet.
        let live = [info(Some(1), None)];
        let mut app = app_with(&live);
        app.sessions[0].proxy = Some("p".into());
        app.refresh_session_cred(0);
        app.on_tick();
        assert!(cred_fetches(&app.take_effects()).is_empty());
        // A terminal tab.
        let live = [shell_info(Some(1))];
        let mut app = app_with(&live);
        app.sessions[0].proxy = Some("p".into());
        app.sessions[0].claude_session_id = Some("c1".into());
        app.refresh_session_cred(0);
        app.on_tick();
        assert!(cred_fetches(&app.take_effects()).is_empty());
        assert!(app.session_creds.is_empty());
    }

    #[test]
    fn a_new_claude_session_id_fetches_again_at_once() {
        let (mut app, live) = proxied_app();
        app.refresh_session_cred(0);
        app.take_effects();
        app.on_incoming_from(
            "local",
            Incoming::Event {
                id: live[0].id,
                event: SessionEvent::ClaudeSession {
                    claude_session_id: "c1-reset".into(),
                },
            },
        );
        assert_eq!(
            cred_fetches(&app.take_effects()),
            vec![("p", live[0].id, "c1-reset")]
        );
    }

    #[test]
    fn credential_replies_are_matched_to_the_claude_session_id() {
        let (mut app, live) = proxied_app();
        let id = live[0].id;
        app.refresh_session_cred(0);
        app.take_effects();
        assert!(app.session_credential(&app.sessions[0]).is_none());

        // A reply for another conversation is dropped.
        app.on_session_credential(id, "other", Ok(Some(work_max())));
        assert!(app.session_credential(&app.sessions[0]).is_none());

        app.on_session_credential(id, "c1", Ok(Some(work_max())));
        let name = app.session_credential(&app.sessions[0]).and_then(|c| c.name());
        assert_eq!(name, Some("work-max"));

        // An error (429, network) keeps what is shown; a 404 clears it.
        app.on_session_credential(id, "c1", Err("rate limited".into()));
        assert!(app.session_credential(&app.sessions[0]).is_some());
        app.on_session_credential(id, "c1", Ok(None));
        assert!(app.session_credential(&app.sessions[0]).is_none());

        // After the claude session id changes, the old credential is not shown.
        app.on_session_credential(id, "c1", Ok(Some(work_max())));
        app.sessions[0].claude_session_id = Some("c1-reset".into());
        assert!(app.session_credential(&app.sessions[0]).is_none());

        // Closing the session forgets it.
        app.remove(0);
        assert!(!app.session_creds.contains_key(&id));
    }

    // ── Wizard generation ─────────────────────────────────────────────────────

    #[test]
    fn each_wizard_gets_its_own_generation() {
        let mut app = app_with(&[]);
        let first = app.wizard_generation();
        app.modal = None;
        assert_eq!(app.wizard_generation(), 0, "no wizard");
        app.on_terminal(alt('n'));
        assert!(app.wizard_generation() > first);
    }

    #[test]
    fn stale_wizard_reply_is_discarded() {
        let mut app = app_with(&[]);
        let gen = app.wizard_generation();
        // Simulate wizard being cancelled.
        app.modal = None;
        // Now a reply with the old gen arrives; it must be silently dropped.
        let reply = Msg::ClaudeSessions {
            cwd: "/w".into(),
            sessions: vec![],
        };
        app.on_reply(ReplyTo::ClaudeSessions { host: "local".into(), cwd: "/w".into(), gen }, Ok(reply));
        // No modal opened (wizard was closed).
        assert!(app.modal.is_none());
    }

    #[test]
    fn only_local_projects_are_cached_for_the_next_wizard() {
        let mut app = app_with(&[]);
        let gen = app.wizard_generation();
        app.modal = None;
        let reply = |path: &str| {
            Ok(Msg::Projects {
                dirs: vec![crate::proto::ProjectDir {
                    path: path.into(),
                    modified: 1,
                    git: None,
                    symlink: false,
                    hidden: false,
                }],
            })
        };
        let to = |host: &str| ReplyTo::Projects { host: host.into(), gen };
        app.on_reply(to("devbox"), reply("/remote"));
        assert!(app.projects.is_empty());
        // Even for a closed wizard: the next one starts with them.
        app.on_reply(to("local"), reply("/local"));
        assert_eq!(app.projects, ["/local"]);
    }

    /// The hosts `effects` subscribe to host stats on.
    fn subscriptions(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Request {
                    host,
                    msg: Msg::SubscribeHostStats,
                    to: ReplyTo::HostStats,
                } => Some(host.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_new_connection_subscribes_to_its_host_stats() {
        let mut app = app_with(&[info(Some(7), None)]);
        // A remote host, as the event loop handles its `Connected`.
        app.recover_host("devbox", &[]);
        app.subscribe_host_stats("devbox");
        assert_eq!(subscriptions(&app.take_effects()), ["devbox"]);
        // A new local daemon (e.g. one that replaced an outdated daemon).
        app.on_reconnected_local(&[info(None, None)], "/home/u".into());
        assert_eq!(subscriptions(&app.take_effects()), ["local"]);
    }

    #[test]
    fn a_refused_host_stats_subscription_is_silent() {
        let mut app = app_with(&[info(Some(7), None)]);
        app.notice = None;
        let refused = Err(std::io::Error::other("unsupported op"));
        app.on_reply(ReplyTo::HostStats, refused);
        assert!(app.notice.is_none());
    }

    #[test]
    fn plain_mode_subscribes_to_no_host_stats() {
        let mut app = App::new(AppConfig {
            mode: Mode::Plain,
            ..test_support::config()
        });
        app.subscribe_host_stats("local");
        assert!(subscriptions(&app.take_effects()).is_empty());
    }
}
