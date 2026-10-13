//! The session manager UI that a bare `claudio` opens.
//!
//! This module is the I/O edge: it owns the terminal and the daemon
//! connections (local + remote) and runs the event loop. State and update
//! logic live in [`app`], rendering in [`ui`].

pub mod app;
mod claude_update;
mod confirm;
mod connections;
mod fmt;
mod git_app;
mod git_view;
mod interaction;
mod keymap;
mod notifications;
mod plain;
mod proxy_state;
mod selection;
mod sessions;
mod state;
mod stats_view;

/// `recent` without the local directories that no longer exist, so the
/// wizard never offers a deleted directory (the check is I/O, so it happens
/// here rather than in the pure wizard).
fn existing_local_dirs(
    mut recent: std::collections::HashMap<String, Vec<String>>,
) -> std::collections::HashMap<String, Vec<String>> {
    if let Some(dirs) = recent.get_mut("local") {
        dirs.retain(|d| std::path::Path::new(d).is_dir());
    }
    recent
}

#[cfg(test)]
mod test_support;
mod ui;
mod wizard;

use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::client::{self, Client, Incoming};
use crate::freshness::{self, DaemonCheck};
use crate::paths;
use crate::proto::{Msg, SessionInfo};
use crate::remote::bootstrap::ensure_remote;

use app::{App, AppConfig, Effect, Mode, ReplyTo};
use connections::Connections;
use plain::PlainStart;
use state::ClientState;

/// Animation / clock tick.
const TICK: Duration = Duration::from_millis(250);
/// Minimum time between redraws (~60 fps), coalescing output bursts.
const FRAME: Duration = Duration::from_millis(16);
/// How often a daemon left outdated (its sessions were busy) is retried.
const OUTDATED_RETRY: Duration = Duration::from_secs(120);

/// Whether keyboard enhancement flags were pushed (and must be popped).
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Emit an OSC 9 desktop notification + single BEL to the outer terminal for a
/// session label. Written directly to stdout, between ratatui frames.
///
/// The label comes from claude (its title) and is sanitized first, so it
/// cannot smuggle escape sequences into the outer terminal.
fn emit_notification(label: &str) {
    use std::io::Write;
    let safe = sessions::sanitize_label(label, 200);
    let msg = format!("\x1b]9;claudio: {safe} needs you\x07");
    let _ = std::io::stdout().write_all(msg.as_bytes());
    let _ = std::io::stdout().flush();
}

/// Hand `text` to the outer terminal's clipboard with OSC 52, written to
/// stdout between ratatui frames like [`emit_notification`]. The terminal
/// must allow it (tmux: `set -g set-clipboard on`).
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(selection::osc52(text).as_bytes());
    let _ = out.flush();
}

/// What the UI is started as.
enum Launch {
    /// The session manager, with a startup proxy override for new sessions.
    Manager {
        proxy: crate::proxy::ProxyChoice,
        first_party: bool,
    },
    /// `claudio --plain`: one bare session; `args` go to claude.
    Plain {
        proxy: crate::proxy::ProxyChoice,
        first_party: bool,
        args: Vec<String>,
    },
}

/// What the app does first, once made.
enum Start {
    /// Recover the manager's tabs from state.json and the local daemon.
    Manager {
        saved: ClientState,
        live: Vec<SessionInfo>,
    },
    /// Start the one `--plain` session.
    Plain(PlainStart),
}

/// Run the manager until the user quits.
pub fn run() -> ExitCode {
    run_with_proxy(crate::proxy::ProxyChoice::Default, false)
}

/// Like [`run`] but applies a startup proxy override to every new session.
///
/// - `Direct`  → new sessions never use a proxy (--no-proxy).
/// - `Profile` → new sessions always pre-select that profile (--proxy <name>).
/// - `Default` → wizard pre-selects the config.toml default, if any.
///
/// `first_party` (--first-party) makes every session of this run first-party.
pub fn run_with_proxy(proxy_override: crate::proxy::ProxyChoice, first_party: bool) -> ExitCode {
    run_launch(Launch::Manager {
        proxy: proxy_override,
        first_party,
    })
}

/// `claudio --plain`: run claude alone, full screen, with the proxy `proxy`
/// picks, and exit with its status. The manager's help, proxy stats, history
/// and reset keys work; the rest of the keyboard is claude's.
pub fn run_plain(
    proxy: crate::proxy::ProxyChoice,
    first_party: bool,
    args: Vec<String>,
) -> ExitCode {
    run_launch(Launch::Plain {
        proxy,
        first_party,
        args,
    })
}

fn run_launch(launch: Launch) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("claudio: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(main(launch));
    rt.shutdown_background();
    code
}

async fn main(launch: Launch) -> ExitCode {
    // Load config and build the keymap from defaults + overrides.
    let config = crate::config::load();
    let mut key_notices = Vec::new();
    let (proxy_override, first_party, plain) = match launch {
        Launch::Manager { proxy, first_party } => (proxy, first_party, None),
        Launch::Plain {
            proxy,
            first_party,
            args,
        } => {
            let cwd = std::env::current_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_owned());
            let start = PlainStart {
                id: uuid::Uuid::new_v4(),
                args,
                cwd,
            };
            (proxy, first_party, Some(start))
        }
    };
    let keymap = match plain {
        Some(_) => keymap::Keymap::build_plain(&config.keys, &mut key_notices),
        None => keymap::Keymap::build(&config.keys, &mut key_notices),
    };
    let plain_id = plain.as_ref().map(|p| p.id);

    // The manager's session list; `--plain` neither reads nor writes it.
    let saved = match plain {
        Some(_) => ClientState::default(),
        None => ClientState::load(&paths::client_state()),
    };
    let (local_client, live) = match connect_local().await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("claudio: cannot reach the session daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (mut terminal, size) = match enter_terminal() {
        Ok(t) => t,
        Err(e) => {
            restore_terminal();
            eprintln!("claudio: cannot set up the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (proxy_profiles, proxy_default) = crate::proxy::resolve::load_proxy_profiles();
    let app = App::new(AppConfig {
        mode: if plain.is_some() { Mode::Plain } else { Mode::Manager },
        size,
        home: local_client.welcome().host.home.clone(),
        keymap,
        notify: config.ui.notify && plain.is_none(),
        copy_on_select: config.ui.copy_on_select,
        proxy_override,
        first_party,
        proxy_profiles,
        proxy_default,
        claude: config.claude,
        local_claude: client_claude(&local_client),
        recent_dirs: existing_local_dirs(saved.recent_dirs.clone()),
        claude_skipped: saved.claude_skipped.clone(),
        // `--plain` has no wizard to offer them in.
        ssh_hosts: match plain {
            Some(_) => Vec::new(),
            None => crate::remote::hosts::candidates(),
        },
    });
    let start = match plain {
        Some(start) => Start::Plain(start),
        None => Start::Manager { saved, live },
    };
    let mut conns = Connections::with_local(local_client.clone());
    // A panic must still reach the cleanup below, so catch it and re-raise.
    let result = std::panic::AssertUnwindSafe(event_loop(
        &mut terminal,
        &mut conns,
        app,
        start,
        local_client,
        config.update.check,
        key_notices,
    ))
    .catch_unwind()
    .await;
    restore_terminal();
    // However the UI ended, a plain session dies with it: nothing lingers in
    // the daemon, and a later manager has nothing to recover. (Killing an
    // already closed session is a harmless "no such session".)
    if let Some(id) = plain_id {
        if let Some(client) = conns.client("local") {
            let kill = client.request(Msg::Kill { id });
            let _ = tokio::time::timeout(Duration::from_secs(3), kill).await;
        }
    }
    match result {
        Err(panic) => std::panic::resume_unwind(panic),
        Ok(Ok(exit)) => {
            if let Some(message) = &exit.message {
                eprintln!("claudio: {message}");
            }
            if plain_id.is_some() {
                exit.exit_code()
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Err(e)) => {
            eprintln!("claudio: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Start the local daemon if needed (replacing an outdated one), connect and
/// list its sessions.
async fn connect_local() -> io::Result<(Client, Vec<SessionInfo>)> {
    let check = client::ensure_local_daemon().await?;
    let client = client::connect(&paths::daemon_socket()).await?;
    if check == DaemonCheck::Deferred {
        client.mark_daemon_outdated();
    }
    list_sessions(&client).await.map(|s| (client, s))
}

/// Once the outdated daemon behind `client` is idle, ask for a reconnect:
/// connecting runs the freshness check again (`connect_local`'s, or the
/// remote bridge's), which replaces it now. A busy one waits for next time.
async fn retry_outdated(
    host: String,
    generation: u64,
    client: Client,
    tx: mpsc::Sender<HostEvent>,
) {
    let Ok(Msg::Sessions { sessions }) = client.request(Msg::ListSessions).await else {
        return;
    };
    if !freshness::busy(&sessions) {
        let _ = tx.send(HostEvent::Reconnect { host, generation }).await;
    }
}

/// The `claude --version` its daemon reported at connect, if claude is there.
fn client_claude(client: &Client) -> Option<String> {
    client.welcome().host.claude.as_ref().map(|c| c.version.clone())
}

async fn list_sessions(client: &Client) -> io::Result<Vec<SessionInfo>> {
    match client.request(Msg::ListSessions).await? {
        Msg::Sessions { sessions } => Ok(sessions),
        other => Err(io::Error::other(format!(
            "unexpected reply to ListSessions: {other:?}"
        ))),
    }
}

// ── Terminal setup ────────────────────────────────────────────────────────────

/// Set the terminal up for the UI; returns it with its size `(width, height)`.
fn enter_terminal() -> io::Result<(Term, (u16, u16))> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }
    let terminal = Terminal::new(CrosstermBackend::new(out))?;
    let size = terminal.size()?;
    Ok((terminal, (size.width, size.height)))
}

/// Undo everything `enter_terminal` did. Safe to call more than once and
/// from the panic hook.
fn restore_terminal() {
    let mut out = io::stdout();
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = disable_raw_mode();
}

// ── Events ────────────────────────────────────────────────────────────────────

/// A tagged incoming item from any host's daemon.
enum HostEvent {
    /// Terminal output or event from a host's daemon, tagged with `(host, generation)`.
    /// The generation is compared against the current generation in Connections;
    /// stale events (gen < current) are dropped.
    Incoming(String, u64, Incoming),
    /// A host connection was established (via bootstrap + connect_ssh).
    Connected {
        host: String,
        client: Client,
        sessions: Vec<SessionInfo>,
        generation: u64,
    },
    /// A remote host connection attempt failed.
    ConnectFailed {
        host: String,
        error: String,
        generation: u64,
    },
    /// A local reconnect completed.
    LocalReconnected {
        client: Client,
        sessions: Vec<SessionInfo>,
        generation: u64,
    },
    /// A local reconnect failed; retry after delay.
    LocalReconnectFailed { generation: u64 },
    /// Drop `host`'s connection and connect again: its outdated daemon has
    /// become idle (see [`retry_outdated`]).
    Reconnect { host: String, generation: u64 },
    /// Proxy config fetched (or failed).
    ProxyConfig {
        profile_name: String,
        config: Option<crate::proxy::api::ConfigResponse>,
    },
    /// Proxy stats, pool health and model catalogue fetched (or failed).
    ProxyStats {
        profile_name: String,
        fetch: proxy_state::ProxyFetch,
    },
    /// The credential claude-proxy reports for a session (or why not).
    SessionCredential {
        session: crate::proto::SessionId,
        claude_session_id: String,
        result: Result<Option<crate::proxy::api::SessionCredential>, String>,
    },
    /// The proxy answered a subscription switch request.
    SubscriptionSwitch {
        session: crate::proto::SessionId,
        result: Result<crate::proxy::api::SwitchOutcome, crate::proxy::api::SwitchError>,
    },
    /// Background upgrade check completed. `Some(tag)` means a newer version is
    /// available; `None` means we are up to date (or the check failed silently).
    UpgradeAvailable(Option<String>),
    /// The newest claude version on its release channel.
    ClaudeLatest(String),
}

// ── Event loop ────────────────────────────────────────────────────────────────

/// Resolves when the terminal's session is hung up or the process is asked to
/// terminate (never, where there are no such signals).
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut hup), Ok(mut term)) = (
        signal(SignalKind::hangup()),
        signal(SignalKind::terminate()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = hup.recv() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    std::future::pending::<()>().await
}

async fn event_loop(
    terminal: &mut Term,
    conns: &mut Connections,
    mut app: App,
    start: Start,
    local_client: Client,
    update_check_enabled: bool,
    key_notices: Vec<String>,
) -> io::Result<plain::Exit> {
    let remote_hosts = match start {
        Start::Plain(start) => {
            app.start_plain(start);
            Vec::new()
        }
        Start::Manager { saved, live } => {
            app.start_manager(&saved, &live);
            // Every remote host a saved tab lives on.
            let hosts: std::collections::BTreeSet<String> = saved
                .sessions
                .into_iter()
                .map(|s| s.host)
                .filter(|h| h != "local")
                .collect();
            hosts.into_iter().collect()
        }
    };
    let manager = app.mode == Mode::Manager;

    // Show any config parse notices in the status bar at startup.
    for notice in key_notices {
        app.notify(notice);
    }

    // Merge all incoming streams into one tagged channel.
    let (ev_tx, mut ev_rx) = mpsc::channel::<HostEvent>(1024);

    // Start the local incoming reader, tagged with the current local generation.
    if let Some(rx) = local_client.take_incoming() {
        let gen = conns.current_generation("local");
        spawn_reader("local".to_owned(), gen, rx, ev_tx.clone());
    }
    // `conns` owns the connection from here: dropping it there closes it.
    drop(local_client);

    // Start background connections to the saved tabs' remote hosts.
    for host in remote_hosts {
        // The entry must exist before the first attempt to track backoff.
        conns.ensure_host(&host);
        let gen = conns.next_generation(&host);
        spawn_connect(host, gen, ev_tx.clone(), false);
    }

    // Kick off a non-blocking upgrade check. The result arrives as
    // HostEvent::UpgradeAvailable, which sets app.upgrade_notice.
    if manager {
        let tx = ev_tx.clone();
        tokio::spawn(async move {
            let result = crate::upgrade::check_once(update_check_enabled).await;
            let _ = tx.send(HostEvent::UpgradeAvailable(result)).await;
        });
    }

    // And, at most once a day, compare the local claude with its release
    // channel. The result arrives as HostEvent::ClaudeLatest.
    if manager && app.claude_policy.update_check != crate::config::UpdatePolicy::Off {
        let tx = ev_tx.clone();
        tokio::spawn(async move {
            if let Some(latest) = crate::claude::update::check_due().await {
                let _ = tx.send(HostEvent::ClaudeLatest(latest)).await;
            }
        });
    }

    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<(ReplyTo, io::Result<Msg>)>();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 60-second proxy stats refresh.
    let mut proxy_tick = tokio::time::interval(Duration::from_secs(60));
    proxy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut outdated_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + OUTDATED_RETRY, OUTDATED_RETRY);
    outdated_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_draw = Instant::now() - FRAME;
    // A plain session must not outlive its terminal.
    let mut shutdown = Box::pin(shutdown_signal());

    loop {
        for effect in app.take_effects() {
            run_effect(effect, &mut app, conns, &reply_tx, &ev_tx);
        }
        if app.quit {
            return Ok(std::mem::take(&mut app.exit));
        }
        let wait = FRAME.saturating_sub(last_draw.elapsed());
        if app.redraw && wait.is_zero() {
            terminal.draw(|f| ui::draw(f, &app))?;
            app.redraw = false;
            last_draw = Instant::now();
            // Check for and emit attention notifications after drawing so
            // the OSC escape doesn't land inside a ratatui buffer update.
            app.check_notifications();
            for label in app.pending_notifs.drain(..) {
                emit_notification(&label);
            }
        }

        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => app.on_terminal(ev),
                Some(Err(e)) => return Err(e),
                None => return Ok(Default::default()),
            },
            _ = &mut shutdown, if !manager => return Ok(Default::default()),
            ev = ev_rx.recv() => match ev {
                Some(HostEvent::Incoming(host, gen, inc)) => {
                    // Drop stale events from replaced connections.
                    if gen < conns.current_generation(&host) {
                        continue;
                    }
                    match inc {
                        // Reconnect with backoff.
                        Incoming::Disconnected => {
                            conns.disconnect(&host);
                            app.on_incoming_from(&host, Incoming::Disconnected);
                            let (delay, gen) = conns.next_attempt(&host);
                            if host == "local" {
                                spawn_reconnect_local(delay, gen, ev_tx.clone());
                            } else {
                                spawn_connect_after(host, gen, delay, ev_tx.clone(), true);
                            }
                        }
                        Incoming::HostStats { cpu_pct, mem_used, mem_total, .. } => {
                            app.on_host_stats(host, cpu_pct, mem_used, mem_total);
                        }
                        inc => app.on_incoming_from(&host, inc),
                    }
                }
                Some(HostEvent::Connected { host, client, sessions, generation }) => {
                    // A newer attempt is already in flight.
                    if generation < conns.current_generation(&host) {
                        continue;
                    }
                    let home = client.welcome().host.home.clone();
                    let remote_claude = client_claude(&client);
                    conns.reset_delay(&host);
                    // Start the reader tagged with the current generation.
                    let cur_gen = conns.current_generation(&host);
                    if let Some(rx) = client.take_incoming() {
                        spawn_reader(host.clone(), cur_gen, rx, ev_tx.clone());
                    }
                    conns.connected(&host, client);
                    // Record this host in the MRU so it appears first next time.
                    crate::remote::hosts::touch(&host);
                    app.on_ssh_hosts(crate::remote::hosts::candidates());
                    app.on_host_connected(&host, &home);
                    // Recover this host's sessions only.
                    app.recover_host(&host, &sessions);
                    app.subscribe_host_stats(&host);
                    app.check_remote_claude(&host, remote_claude.as_deref());
                    app.redraw = true;
                }
                Some(HostEvent::ConnectFailed { host, error, generation }) => {
                    // A newer attempt is already in flight.
                    if generation < conns.current_generation(&host) {
                        continue;
                    }
                    app.on_host_error(&host, &error);
                    conns.set_bootstrap_failed(&host, error.contains("bootstrap") || error.contains("install"));
                    // Retry with backoff, bootstrapping again only if that
                    // is what failed.
                    let (delay, gen) = conns.next_attempt(&host);
                    let skip_bootstrap = !conns.bootstrap_failed(&host);
                    spawn_connect_after(host, gen, delay, ev_tx.clone(), skip_bootstrap);
                }
                Some(HostEvent::LocalReconnected { client, sessions, generation }) => {
                    // A newer attempt is already in flight.
                    if generation < conns.current_generation("local") {
                        continue;
                    }
                    conns.reset_delay("local");
                    let home = client.welcome().host.home.clone();
                    let cur_gen = conns.current_generation("local");
                    if let Some(rx) = client.take_incoming() {
                        spawn_reader("local".to_owned(), cur_gen, rx, ev_tx.clone());
                    }
                    app.set_local_claude(client_claude(&client));
                    conns.connected("local", client);
                    app.on_reconnected_local(&sessions, home);
                }
                Some(HostEvent::LocalReconnectFailed { generation }) => {
                    if generation < conns.current_generation("local") {
                        continue;
                    }
                    let (delay, gen) = conns.next_attempt("local");
                    spawn_reconnect_local(delay, gen, ev_tx.clone());
                }
                Some(HostEvent::Reconnect { host, generation }) => {
                    if generation < conns.current_generation(&host) || !conns.is_connected(&host) {
                        continue;
                    }
                    // Dropping the client closes the connection; the newer
                    // generation silences its reader.
                    conns.disconnect(&host);
                    app.on_incoming_from(&host, Incoming::Disconnected);
                    let gen = conns.next_generation(&host);
                    if host == "local" {
                        spawn_reconnect_local(Duration::ZERO, gen, ev_tx.clone());
                    } else {
                        spawn_connect(host, gen, ev_tx.clone(), true);
                    }
                }
                Some(HostEvent::ProxyConfig { profile_name, config }) => {
                    if let Some(cfg) = config {
                        app.on_proxy_config(profile_name, cfg);
                    }
                }
                Some(HostEvent::ProxyStats { profile_name, fetch }) => {
                    app.on_proxy_stats(profile_name, fetch);
                }
                Some(HostEvent::SessionCredential { session, claude_session_id, result }) => {
                    app.on_session_credential(session, &claude_session_id, result);
                }
                Some(HostEvent::SubscriptionSwitch { session, result }) => {
                    app.on_switch_result(session, result);
                }
                Some(HostEvent::UpgradeAvailable(tag)) => {
                    if let Some(t) = tag {
                        app.upgrade_notice = Some(t);
                        app.redraw = true;
                    }
                }
                Some(HostEvent::ClaudeLatest(latest)) => app.check_local_claude(&latest),
                None => return Ok(Default::default()),
            },
            Some((to, reply)) = reply_rx.recv() => app.on_reply(to, reply),
            _ = tick.tick() => app.on_tick(),
            _ = proxy_tick.tick() => {
                // Refresh proxy stats for the active session's profile (they
                // feed the status bar, which `--plain` does not have).
                let proxy_name = app.active_view().and_then(|v| v.proxy.clone());
                if let Some(name) = proxy_name.filter(|_| manager) {
                    app.schedule_proxy_stats(&name, vec![stats_view::Window::H24]);
                }
            }
            _ = outdated_tick.tick() => {
                for (host, conn) in &conns.map {
                    if let Some(client) = conn.client.as_ref().filter(|c| c.daemon_outdated()) {
                        let retry = retry_outdated(
                            host.clone(),
                            conn.generation,
                            client.clone(),
                            ev_tx.clone(),
                        );
                        tokio::spawn(retry);
                    }
                }
            }
            _ = tokio::time::sleep(wait), if app.redraw => {}
        }
    }
}

fn run_effect(
    effect: Effect,
    app: &mut App,
    conns: &mut Connections,
    reply_tx: &mpsc::UnboundedSender<(ReplyTo, io::Result<Msg>)>,
    ev_tx: &mpsc::Sender<HostEvent>,
) {
    match effect {
        Effect::Request { host, msg, to } => {
            let tx = reply_tx.clone();
            match conns.client(&host) {
                Some(client) => {
                    let reply = client.request(msg);
                    tokio::spawn(async move {
                        let _ = tx.send((to, reply.await));
                    });
                }
                None => {
                    let _ = tx.send((
                        to,
                        Err(io::Error::new(
                            io::ErrorKind::NotConnected,
                            format!("{host}: not connected"),
                        )),
                    ));
                }
            }
        }
        Effect::Input(id, bytes) => {
            // Route to the session's host (look up from app.sessions).
            let host = app
                .sessions
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.host.clone())
                .unwrap_or_else(|| "local".to_owned());
            if let Some(client) = conns.client(&host) {
                client.send_input(id, &bytes);
            }
        }
        Effect::Connect(host) => {
            // The wizard picked a host that is already connected: reuse the
            // connection, and let the wizard advance as if it had just connected.
            if conns.is_connected(&host) {
                if let Some(client) = conns.client(&host) {
                    let home = client.welcome().host.home.clone();
                    app.on_host_connected(&host, &home);
                    // Don't re-spawn a reader: the existing one is still running.
                }
                return;
            }
            // The entry must exist before the first attempt to track backoff.
            conns.ensure_host(&host);
            let gen = conns.next_generation(&host);
            spawn_connect(host, gen, ev_tx.clone(), false);
        }
        Effect::LoadSshHosts => app.on_ssh_hosts(crate::remote::hosts::candidates()),
        Effect::CopyToClipboard(text) => copy_to_clipboard(&text),
        Effect::Save => {
            if let Err(e) = app.to_state().save(&paths::client_state()) {
                app.notify(format!("could not save state: {e}"));
            }
        }
        Effect::FetchProxyConfig { profile_name } => {
            let tx = ev_tx.clone();
            let name = profile_name.clone();
            tokio::spawn(async move {
                let config = fetch_proxy_config(&name).await;
                let _ = tx
                    .send(HostEvent::ProxyConfig {
                        profile_name: name,
                        config,
                    })
                    .await;
            });
        }
        Effect::FetchProxyStats {
            profile_name,
            windows,
            models,
        } => {
            let tx = ev_tx.clone();
            tokio::spawn(async move {
                let fetch = fetch_proxy_stats(&profile_name, &windows, models).await;
                let _ = tx
                    .send(HostEvent::ProxyStats {
                        profile_name,
                        fetch,
                    })
                    .await;
            });
        }
        Effect::FetchSessionCredential {
            profile_name,
            session,
            claude_session_id,
        } => {
            let tx = ev_tx.clone();
            tokio::spawn(async move {
                let result = fetch_session_credential(&profile_name, &claude_session_id).await;
                let _ = tx
                    .send(HostEvent::SessionCredential {
                        session,
                        claude_session_id,
                        result,
                    })
                    .await;
            });
        }
        Effect::SwitchSubscription {
            profile_name,
            session,
            claude_session_id,
        } => {
            let tx = ev_tx.clone();
            tokio::spawn(async move {
                let result = switch_subscription(&profile_name, &claude_session_id).await;
                let _ = tx
                    .send(HostEvent::SubscriptionSwitch {
                        session,
                        result,
                    })
                    .await;
            });
        }
        Effect::SpawnWithProxy {
            host,
            msg,
            proxy_name,
            first_party,
            to,
        } => {
            // Resolve the client now (synchronous, cheap) so we can move it
            // into the async task. Client is Clone + Send + 'static.
            let client = conns.client(&host).cloned();
            let tx = reply_tx.clone();
            tokio::spawn(async move {
                // Fetch proxy config with a 5 s timeout so the first spawn
                // uses the real model/rate-limit overrides, not fallback defaults.
                let cfg = tokio::time::timeout(
                    Duration::from_secs(5),
                    fetch_proxy_config(&proxy_name),
                )
                .await
                .ok()
                .flatten();

                // Build env from profile + (possibly fresh) config.
                let env = crate::proxy::resolve::proxy_env_for(
                    Some(&proxy_name),
                    cfg.as_ref(),
                    first_party,
                );
                match env {
                    Err(e) => {
                        let _ = tx.send((
                            to,
                            Err(io::Error::other(format!("proxy env error: {e}"))),
                        ));
                    }
                    Ok(env) => {
                        match client {
                            Some(c) => {
                                let reply = c.request(msg.with_env(env));
                                let _ = tx.send((to, reply.await));
                            }
                            None => {
                                let _ = tx.send((
                                    to,
                                    Err(io::Error::new(
                                        io::ErrorKind::NotConnected,
                                        format!("{host}: not connected"),
                                    )),
                                ));
                            }
                        }
                    }
                }
            });
        }
    }
}

/// Fetch proxy config for a named profile. Returns `None` on any error.
async fn fetch_proxy_config(profile_name: &str) -> Option<crate::proxy::api::ConfigResponse> {
    let (url, token) = crate::proxy::resolve::resolve_profile(profile_name)?;
    match crate::proxy::api::fetch_config(&url, &token).await {
        Ok(Some(cfg)) => Some(cfg),
        Ok(None) => None,
        Err(_) => None,
    }
}

/// Fetch proxy stats for `windows`, pool health, and (when `models`) the
/// model catalogue for a named profile. Requests run sequentially to stay
/// well inside the proxy's per-user rate limit (1 req/s, burst 10).
async fn fetch_proxy_stats(
    profile_name: &str,
    windows: &[stats_view::Window],
    models: bool,
) -> proxy_state::ProxyFetch {
    use crate::proxy::api;
    let mut out = proxy_state::ProxyFetch::default();
    let Some((url, token)) = crate::proxy::resolve::resolve_profile(profile_name) else {
        let err = format!("proxy profile '{profile_name}' not found");
        out.stats = windows.iter().map(|w| (*w, Err(err.clone()))).collect();
        return out;
    };
    for w in windows {
        let result = api::fetch_stats(&url, &token, w.param())
            .await
            .map_err(describe_api_error);
        out.stats.push((*w, result));
    }
    out.pool = api::fetch_pool_health(&url, &token).await.ok();
    if models {
        out.models = api::fetch_models(&url, &token).await.ok();
    }
    out
}

/// Ask the proxy which credential a conversation uses. `Ok(None)` is "not
/// known (yet)": no credential to show, not an error.
async fn fetch_session_credential(
    profile_name: &str,
    claude_session_id: &str,
) -> Result<Option<crate::proxy::api::SessionCredential>, String> {
    let (url, token) = crate::proxy::resolve::resolve_profile(profile_name)
        .ok_or_else(|| format!("proxy profile '{profile_name}' not found"))?;
    crate::proxy::api::fetch_session(&url, &token, claude_session_id)
        .await
        .map_err(describe_api_error)
}

/// Ask the proxy to move a conversation to its next subscription.
async fn switch_subscription(
    profile_name: &str,
    claude_session_id: &str,
) -> Result<crate::proxy::api::SwitchOutcome, crate::proxy::api::SwitchError> {
    use crate::proxy::api::{ApiError, SwitchError};
    let Some((url, token)) = crate::proxy::resolve::resolve_profile(profile_name) else {
        return Err(SwitchError::Api(ApiError::Http(
            reqwest::StatusCode::UNAUTHORIZED,
        )));
    };
    crate::proxy::api::switch_session(&url, &token, claude_session_id).await
}

/// A one-line, user-facing description of a proxy API error.
fn describe_api_error(e: crate::proxy::api::ApiError) -> String {
    use crate::proxy::api::ApiError;
    match e {
        ApiError::Http(s) if s.as_u16() == 403 => {
            "stats need a user token (admin and anonymous tokens are refused)".to_owned()
        }
        ApiError::Http(s) if s.as_u16() == 404 => {
            "this proxy has no claudio API (upgrade claude-proxy)".to_owned()
        }
        ApiError::Http(s) if s.as_u16() == 429 => {
            "rate limited by the proxy; wait a few seconds".to_owned()
        }
        other => other.to_string(),
    }
}

/// Spawn a task that reads incoming items from `rx` and forwards them,
/// tagged with `host` and `gen`, to `tx`.
fn spawn_reader(
    host: String,
    gen: u64,
    mut rx: mpsc::Receiver<Incoming>,
    tx: mpsc::Sender<HostEvent>,
) {
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            if tx
                .send(HostEvent::Incoming(host.clone(), gen, item))
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

/// Spawn a task that reconnects to the local daemon (starting it if needed)
/// after `delay`; the outcome comes back tagged with `generation`.
fn spawn_reconnect_local(delay: Duration, generation: u64, tx: mpsc::Sender<HostEvent>) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let event = match connect_local().await {
            Ok((client, sessions)) => HostEvent::LocalReconnected {
                client,
                sessions,
                generation,
            },
            Err(_) => HostEvent::LocalReconnectFailed { generation },
        };
        let _ = tx.send(event).await;
    });
}

/// Spawn a background task that bootstraps + connects to `host`.
fn spawn_connect(host: String, gen: u64, tx: mpsc::Sender<HostEvent>, skip_bootstrap: bool) {
    tokio::spawn(async move {
        do_connect(host, gen, tx, skip_bootstrap).await;
    });
}

/// Spawn a task that sleeps `delay` then connects.
fn spawn_connect_after(
    host: String,
    gen: u64,
    delay: Duration,
    tx: mpsc::Sender<HostEvent>,
    skip_bootstrap: bool,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        do_connect(host, gen, tx, skip_bootstrap).await;
    });
}

async fn do_connect(host: String, gen: u64, tx: mpsc::Sender<HostEvent>, skip_bootstrap: bool) {
    // Bootstrap (upload binary if needed). Skip on reconnect or when the last
    // failure was not a bootstrap failure.
    if !skip_bootstrap {
        if let Err(e) = ensure_remote(&host).await {
            let _ = tx
                .send(HostEvent::ConnectFailed {
                    host,
                    error: e,
                    generation: gen,
                })
                .await;
            return;
        }
    }
    // Connect the SSH bridge.
    match client::connect_ssh(&host).await {
        Ok(client) => {
            let sessions = match list_sessions(&client).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx
                        .send(HostEvent::ConnectFailed {
                            host,
                            error: e.to_string(),
                            generation: gen,
                        })
                        .await;
                    return;
                }
            };
            let _ = tx
                .send(HostEvent::Connected {
                    host,
                    client,
                    sessions,
                    generation: gen,
                })
                .await;
        }
        Err(e) => {
            let _ = tx
                .send(HostEvent::ConnectFailed {
                    host,
                    error: e.to_string(),
                    generation: gen,
                })
                .await;
        }
    }
}
