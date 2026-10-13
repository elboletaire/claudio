//! One live session: `claude` (or a login shell) under a PTY, driven by a single actor task.
//!
//! The actor alone owns the PTY master, the child killer, the emulated
//! [`Screen`], the [`ProbeResponder`], the hook [`Tracker`] and the list of
//! subscribed clients — no locks. Three helper threads do the blocking work:
//!
//! - **reader**: PTY output → actor (`blocking_send`, so a busy actor slows
//!   the child down rather than buffering without bound);
//! - **writer**: actor → PTY input (keyboard input and probe replies), so a
//!   child that stops reading stdin can never stall the async runtime;
//! - **waiter**: reaps the child and reports its exit code.
//!
//! Output fans out to subscribers with `try_send`. A subscriber whose queue is
//! full is marked *lagging* and skipped; once its queue drains it gets a fresh
//! `Attached` + snapshot instead of the bytes it missed. A slow client
//! therefore never blocks the PTY.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};

use super::{host, Daemon};
use crate::claude::hooks;
use crate::claude::state::Tracker;
use crate::proto::{
    Envelope, Frame, Msg, SessionEvent, SessionId, SessionKind, SessionState, SpawnSpec,
};
use crate::proxy::env::env_diff;
use crate::term::probe::ProbeResponder;
use crate::term::screen::Screen;

/// Identifies one client connection.
pub type ClientId = u64;

/// Largest `D` frame payload the daemon sends.
const CHUNK: usize = 64 * 1024;

/// A lagging subscriber is resynced once this many queue slots are free.
const RESYNC_ROOM: usize = 64;

/// Timer-based lag recovery: check for resyncs this often even without new output.
const LAG_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Queue depths: actor commands, PTY output chunks, PTY input writes.
const CMD_QUEUE: usize = 256;
const OUTPUT_QUEUE: usize = 64;
const INPUT_QUEUE: usize = 256;

/// How long after child exit to drain remaining PTY output before giving up.
const EXIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the actor waits for `Daemon::spawn` to commit the handle to
/// `registry.live` after the child exits early. Phase 3 holds no I/O — just
/// a mutex acquire and in-memory map operations — so this should be
/// microseconds in practice. The ceiling is a safety net for extreme load.
const COMMIT_SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);

/// If claude exits with no SessionStart within this window, consider it a
/// failed `--resume` (conversation gone) and retry once without `--resume`.
const RESUME_RETRY_WINDOW: Duration = Duration::from_secs(3);

/// After `Kill`'s SIGHUP, how long the child gets before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// Default size when a spec carries a zero dimension.
const DEFAULT_SIZE: (u16, u16) = (24, 80);

/// How often the metadata (git status, transcript totals) of a session with
/// attached clients is refreshed without a hook asking for it.
const META_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Hooks closer together than this (tool-use bursts) share one refresh.
const META_HOOK_GAP: Duration = Duration::from_secs(2);

/// Messages to a session actor.
pub enum Cmd {
    /// Keyboard/mouse bytes for the PTY.
    Input(Vec<u8>),
    /// Subscribe `queue`: resize, reply `Attached` (echoing `req`), send the
    /// snapshot — all in one step so no output is lost or doubled.
    Attach {
        client: ClientId,
        queue: mpsc::Sender<Frame>,
        req: Option<u64>,
        rows: u16,
        cols: u16,
    },
    Detach {
        client: ClientId,
    },
    /// Resize, then resync every subscriber except `client`.
    Resize {
        client: ClientId,
        rows: u16,
        cols: u16,
    },
    Hook {
        event: String,
        payload: Value,
    },
    Status(oneshot::Sender<Status>),
    /// Kill the child and stop. The registry has already forgotten it.
    Kill,
    /// Like `Kill`, first handing over the subscribers so a respawn can
    /// attach them to the new process.
    Stop(oneshot::Sender<Vec<Subscription>>),
}

/// An attached client's id and output queue.
pub type Subscription = (ClientId, mpsc::Sender<Frame>);

/// What `ListSessions` needs from a live actor.
pub struct Status {
    pub state: SessionState,
    pub title: Option<String>,
}

/// The registry's handle on a live session.
pub struct Handle {
    pub tx: mpsc::Sender<Cmd>,
    /// This spawn's hook token; hooks carrying any other token are ignored.
    pub token: String,
    pub pid: Option<u32>,
}

/// Ask a live actor for its status; `None` if it is gone or too busy.
pub async fn status(tx: &mpsc::Sender<Cmd>, wait: Duration) -> Option<Status> {
    let (reply, rx) = oneshot::channel();
    tx.send(Cmd::Status(reply)).await.ok()?;
    tokio::time::timeout(wait, rx).await.ok()?.ok()
}

/// Kill a live session's child and wait until its actor is gone; returns the
/// clients that were attached to it. The actor stops on `Stop` without
/// running `on_exit`, so nothing is announced.
pub async fn stop(handle: &Handle) -> Vec<Subscription> {
    let (reply, mut subscribers) = oneshot::channel();
    let _ = handle.tx.send(Cmd::Stop(reply)).await;
    let _ = tokio::time::timeout(2 * KILL_GRACE, handle.tx.closed()).await;
    // Answered before the actor kills its child; empty if it was already on
    // its way out (or is still stuck after the timeout).
    subscribers.try_recv().unwrap_or_default()
}

/// Start claude (or, for `SessionKind::Shell`, the login shell) for `spec` under
/// a new PTY and spawn its actor.
/// Called from `spawn_blocking` — must not use tokio primitives.
///
/// Returns the registry [`Handle`] and a oneshot sender the caller must use to
/// signal that the handle has been committed to `registry.live`. The actor
/// awaits this signal before acting on child exit, which prevents the
/// commit-before-exit race described in §Race.
pub fn spawn(
    daemon: &Arc<Daemon>,
    spec: &SpawnSpec,
    kind: SessionKind,
) -> io::Result<(Handle, oneshot::Sender<()>)> {
    let cwd = host::expand_tilde(&spec.cwd);
    if !cwd.is_dir() {
        return Err(io::Error::other(format!(
            "working directory {} does not exist",
            cwd.display()
        )));
    }
    let token = hooks::new_token();
    let path = daemon.config.path.as_deref();
    let (cmd, program) = match kind {
        SessionKind::Claude => {
            let cmd = command(
                &daemon.config,
                spec,
                &cwd,
                &token,
                daemon.skip_permissions(),
            );
            let program = PathBuf::from(cmd.get_argv()[0].clone());
            (cmd, program)
        }
        SessionKind::Shell => {
            let shell = super::login_path::login_shell();
            (shell_command(&shell, &cwd, path), shell)
        }
    };
    let (rows, cols) = size_or_default(spec.rows, spec.cols);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::other(format!("could not open a pty: {e}")))?;
    let child = pair.slave.spawn_command(cmd).map_err(|e| {
        io::Error::other(format!("could not start {}: {e}", program.display()))
    })?;
    drop(pair.slave);

    let pid = child.process_id();
    let mut killer = child.clone_killer();
    let io = match start_io(pair.master.as_ref(), child) {
        Ok(io) => io,
        Err(e) => {
            let _ = killer.kill();
            return Err(io::Error::other(format!(
                "could not start session i/o: {e}"
            )));
        }
    };

    // Detect whether this spawn uses --resume (for retry logic, §2.3).
    let spawned_with_resume =
        kind == SessionKind::Claude && spec.args.iter().any(|a| a == "--resume");
    let spawn_time = std::time::Instant::now();

    // A shell has no hooks to report state: it is simply idle, forever.
    let mut tracker = Tracker::new();
    if kind == SessionKind::Shell {
        tracker.state = SessionState::Idle;
    }

    let (tx, cmds) = mpsc::channel(CMD_QUEUE);
    // The committed channel lets Daemon::spawn signal that the handle has been
    // inserted into registry.live. The actor awaits this before running on_exit
    // so it never calls forget_live or retry_spawn_fresh before the handle is
    // visible — fixing the commit-before-exit race (§Race).
    let (committed_tx, committed_rx) = oneshot::channel::<()>();
    let actor = Actor {
        id: spec.id,
        daemon: Arc::clone(daemon),
        token: token.clone(),
        master: pair.master,
        input: io.input,
        killer,
        pid,
        screen: Screen::new(rows, cols),
        probe: ProbeResponder::new(rows, cols),
        tracker,
        title: None,
        subs: HashMap::new(),
        spawned_with_resume,
        spawn_time,
        kind,
        meta: MetaRefresher::new(spec.id, Arc::clone(daemon), cwd, kind),
    };
    tokio::spawn(actor.run(cmds, io.output, io.exit, committed_rx));
    Ok((Handle { tx, token, pid }, committed_tx))
}

/// `<claude> --settings <hooks> [--allow-dangerously-skip-permissions]
/// [args…] [-n <name>]`, in `cwd`, with the session env applied.
///
/// The skip-permissions flag is added here, on every launch path (new spawn,
/// `--resume`, resume-retry), rather than in `spec.args`, so the journal never
/// stores it. It is left out when the args already choose a permissions flag.
fn command(
    cfg: &super::Config,
    spec: &SpawnSpec,
    cwd: &Path,
    token: &str,
    skip_permissions: bool,
) -> CommandBuilder {
    let settings = hooks::settings(&cfg.claudio, &cfg.socket, token);
    let mut cmd = CommandBuilder::new(cfg.claude_bin());
    cmd.arg("--settings");
    cmd.arg(settings.to_string());
    if skip_permissions
        && !spec
            .args
            .iter()
            .any(|a| a == host::ALLOW_SKIP_PERMISSIONS || a == "--dangerously-skip-permissions")
    {
        cmd.arg(host::ALLOW_SKIP_PERMISSIONS);
    }
    for a in &spec.args {
        cmd.arg(a);
    }
    if let Some(name) = &spec.name {
        cmd.arg("-n");
        cmd.arg(name);
    }
    cmd.cwd(cwd);
    apply_env(&mut cmd, cfg.path.as_deref(), &spec.env);
    cmd
}

/// Start from the daemon's env (the builder's default), apply the shared
/// [`env_diff`] (scrubs what would leak from a daemon started inside claude;
/// a proxy token replaces any API key), set the terminal type and the
/// daemon's login-shell `PATH` ([`super::Config::path`]), then set the spec's
/// env (an explicit `PATH` there wins).
fn apply_env(cmd: &mut CommandBuilder, path: Option<&OsStr>, env: &[(String, String)]) {
    let diff = env_diff(env);
    for key in diff.remove {
        cmd.env_remove(key);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    for (k, v) in diff.set {
        cmd.env(k, v);
    }
}

/// `<shell> -l` in `cwd`: a plain terminal. No hooks, no proxy env, and the
/// claude session markers are scrubbed so a `claude` typed into it is a
/// top-level session, not a child of whatever started the daemon.
fn shell_command(shell: &Path, cwd: &Path, path: Option<&OsStr>) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(shell);
    cmd.arg("-l");
    cmd.cwd(cwd);
    apply_env(&mut cmd, path, &[]);
    cmd
}

/// claude animates its terminal title with a leading status glyph
/// (`✳ Claude Code`, `◐ Fix the parser`); strip it so the title only changes
/// — and is only broadcast — when the actual text does.
fn clean_title(title: &str) -> &str {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
}

fn size_or_default(rows: u16, cols: u16) -> (u16, u16) {
    if rows == 0 || cols == 0 {
        DEFAULT_SIZE
    } else {
        (rows, cols)
    }
}

/// Channels to the session's helper threads.
struct Io {
    input: std_mpsc::SyncSender<Vec<u8>>,
    output: mpsc::Receiver<Vec<u8>>,
    exit: oneshot::Receiver<Option<i32>>,
}

/// Spawn the reader, writer and waiter threads.
fn start_io(
    master: &(dyn MasterPty + Send),
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) -> std::io::Result<Io> {
    let reader = master.try_clone_reader().map_err(std::io::Error::other)?;
    let writer = master.take_writer().map_err(std::io::Error::other)?;
    let (out_tx, output) = mpsc::channel(OUTPUT_QUEUE);
    let (input, in_rx) = std_mpsc::sync_channel(INPUT_QUEUE);
    let (exit_tx, exit) = oneshot::channel();

    thread("claudio-pty-read", move || read_pty(reader, out_tx))?;
    thread("claudio-pty-write", move || write_pty(writer, in_rx))?;
    thread("claudio-pty-wait", move || {
        let code = child.wait().ok().map(|s| s.exit_code() as i32);
        let _ = exit_tx.send(code);
    })?;
    Ok(Io {
        input,
        output,
        exit,
    })
}

fn thread(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .map(drop)
}

fn read_pty(mut reader: Box<dyn Read + Send>, out: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // EIO once the child side closes.
            Err(_) => break,
        }
    }
}

fn write_pty(mut writer: Box<dyn Write + Send>, input: std_mpsc::Receiver<Vec<u8>>) {
    for bytes in input {
        if writer
            .write_all(&bytes)
            .and_then(|_| writer.flush())
            .is_err()
        {
            break;
        }
    }
}

/// A client subscribed to this session's output.
struct Subscriber {
    queue: mpsc::Sender<Frame>,
    /// Its queue overflowed; output is withheld until a resync.
    lagging: bool,
    /// The `Attach` request id, until an `Attached` carrying it is queued.
    pending_req: Option<u64>,
}

impl Subscriber {
    /// Queue `Attached` + the snapshot. Returns `false` if the client is gone.
    fn sync(&mut self, id: SessionId, (rows, cols): (u16, u16), snapshot: &[u8]) -> bool {
        let attached = Envelope {
            req: self.pending_req,
            msg: Msg::Attached { id, rows, cols },
        };
        self.lagging = false;
        match self.queue.try_send(Frame::Control(attached)) {
            Ok(()) => self.pending_req = None,
            Err(TrySendError::Full(_)) => {
                self.lagging = true;
                return true;
            }
            Err(TrySendError::Closed(_)) => return false,
        }
        self.offer(id, snapshot)
    }

    /// Queue `bytes` as `D` frames, marking the subscriber lagging if its
    /// queue fills. Returns `false` if the client is gone.
    fn offer(&mut self, id: SessionId, bytes: &[u8]) -> bool {
        for chunk in bytes.chunks(CHUNK) {
            let frame = Frame::Data {
                session: id,
                bytes: chunk.to_vec(),
            };
            match self.queue.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.lagging = true;
                    return true;
                }
                Err(TrySendError::Closed(_)) => return false,
            }
        }
        true
    }

    /// A lagging subscriber is ready for a resync.
    fn can_resync(&self) -> bool {
        self.lagging && self.queue.capacity() > RESYNC_ROOM
    }
}

struct Actor {
    id: SessionId,
    daemon: Arc<Daemon>,
    token: String,
    master: Box<dyn MasterPty + Send>,
    input: std_mpsc::SyncSender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pid: Option<u32>,
    screen: Screen,
    probe: ProbeResponder,
    tracker: Tracker,
    title: Option<String>,
    subs: HashMap<ClientId, Subscriber>,
    /// Whether this spawn included `--resume` (for retry logic on quick exit).
    spawned_with_resume: bool,
    /// When the child was spawned, for measuring quick-exit window.
    spawn_time: std::time::Instant,
    /// What the child is: claude, or a plain login shell.
    kind: SessionKind,
    /// Git status and transcript totals for the status bar (owns the cwd).
    meta: MetaRefresher,
}

/// Produces [`SessionEvent::Meta`] for one session: the git status of its
/// directory and, for claude, the transcript's model, context and totals.
///
/// One refresh runs at a time, off the actor task (git is a child process,
/// the transcript read is blocking I/O), and the actor never waits for it.
struct MetaRefresher {
    id: SessionId,
    daemon: Arc<Daemon>,
    cwd: PathBuf,
    kind: SessionKind,
    /// The transcript cursor, held by the running refresh.
    cursor: Arc<std::sync::Mutex<Option<crate::claude::projects::TranscriptCursor>>>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    started_at: Option<std::time::Instant>,
}

impl MetaRefresher {
    fn new(id: SessionId, daemon: Arc<Daemon>, cwd: PathBuf, kind: SessionKind) -> Self {
        Self {
            id,
            daemon,
            cwd,
            kind,
            cursor: Arc::default(),
            busy: Arc::default(),
            started_at: None,
        }
    }

    /// Start a refresh unless one is running or the last one started less
    /// than `min_gap` ago. `session_id` is the current claude conversation.
    fn refresh(&mut self, session_id: Option<&str>, min_gap: Duration) {
        use std::sync::atomic::Ordering;
        if self.started_at.is_some_and(|t| t.elapsed() < min_gap) {
            return;
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        self.started_at = Some(std::time::Instant::now());
        let (id, daemon, cwd, kind) = (self.id, Arc::clone(&self.daemon), self.cwd.clone(), self.kind);
        let (cursor, busy) = (Arc::clone(&self.cursor), Arc::clone(&self.busy));
        let session_id = session_id.map(str::to_owned);
        tokio::spawn(async move {
            let git = super::git_status::read(&cwd).await;
            let transcript = match (kind, session_id) {
                (SessionKind::Claude, Some(sid)) => {
                    let cwd = cwd.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut slot = cursor.lock().unwrap_or_else(|e| e.into_inner());
                        let cursor = slot
                            .take()
                            .filter(|c| c.session_id() == sid)
                            .unwrap_or_else(|| {
                                crate::claude::projects::TranscriptCursor::new(&cwd, &sid)
                            });
                        let cursor = slot.insert(cursor);
                        cursor.advance();
                        cursor.has_assistant().then(|| cursor.totals().clone())
                    })
                    .await
                    .ok()
                    .flatten()
                }
                _ => None,
            };
            daemon.broadcast(id, meta_event(&cwd, git, transcript));
            busy.store(false, Ordering::Release);
        });
    }
}

/// The `Meta` event for a refresh's findings. The branch falls back to
/// `.git/HEAD` when git itself could not answer.
fn meta_event(
    cwd: &Path,
    git: Option<crate::proto::GitStatus>,
    transcript: Option<crate::claude::projects::TranscriptTotals>,
) -> SessionEvent {
    let branch = match &git {
        Some(status) => status.branch().map(str::to_owned),
        None => read_git_branch(cwd),
    };
    let (model, context_tokens, output_tokens, turns) = match transcript {
        Some(t) => (t.model, t.context_tokens, Some(t.output_tokens), Some(t.turns)),
        None => (None, None, None, None),
    };
    SessionEvent::Meta {
        branch,
        model,
        context_tokens,
        git,
        output_tokens,
        turns,
    }
}

/// The git part of `Meta` for `cwd`, for a session that has no actor yet.
pub(super) async fn git_meta(cwd: &Path) -> SessionEvent {
    meta_event(cwd, super::git_status::read(cwd).await, None)
}

impl Actor {
    async fn run(
        mut self,
        mut cmds: mpsc::Receiver<Cmd>,
        mut output: mpsc::Receiver<Vec<u8>>,
        mut exit: oneshot::Receiver<Option<i32>>,
        committed: oneshot::Receiver<()>,
    ) {
        // Timer-based lag recovery: resync even when the child is silent.
        let mut lag_check = tokio::time::interval(LAG_CHECK_INTERVAL);
        lag_check.tick().await; // consume the immediate first tick
        let mut meta_tick = tokio::time::interval(META_REFRESH_INTERVAL);
        meta_tick.tick().await;

        // Wrap in Option so we can .take() it in the exit branch (once).
        let mut committed = Some(committed);

        loop {
            tokio::select! {
                // Commands first: keystrokes and attaches must not queue
                // behind a long burst of output.
                biased;
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Kill) | None => {
                        drop(output);
                        self.kill(exit).await;
                        return;
                    }
                    Some(Cmd::Stop(reply)) => {
                        let subs = std::mem::take(&mut self.subs);
                        let _ = reply.send(subs.into_iter().map(|(c, s)| (c, s.queue)).collect());
                        drop(output);
                        self.kill(exit).await;
                        return;
                    }
                    Some(cmd) => self.on_cmd(cmd),
                },
                Some(bytes) = output.recv() => self.on_output(&bytes),
                code = &mut exit => {
                    let exit_code = code.ok().flatten();
                    // Drain until the reader thread closes the channel (PTY EOF)
                    // so final output is never lost. A deadline prevents blocking
                    // forever if another process holds the PTY slave open.
                    let drain_deadline =
                        tokio::time::Instant::now() + EXIT_DRAIN_TIMEOUT;
                    loop {
                        match tokio::time::timeout_at(drain_deadline, output.recv()).await {
                            Ok(Some(bytes)) => self.on_output(&bytes),
                            Ok(None) | Err(_) => break,
                        }
                    }
                    // §Race fix: wait for Daemon::spawn Phase 3 to commit the
                    // handle to registry.live before running on_exit. Without
                    // this, a child that exits before Phase 3 would find nothing
                    // in registry.live, strand the in-flight id, and then have
                    // Phase 3 insert a stale live handle for a dead process.
                    if let Some(rx) = committed.take() {
                        match tokio::time::timeout(COMMIT_SIGNAL_TIMEOUT, rx).await {
                            Ok(Ok(())) => {
                                // Handle committed; proceed normally.
                            }
                            Ok(Err(_)) | Err(_) => {
                                // Sender was dropped (spawn failed after actor
                                // start, extremely rare) or timed out. The handle
                                // was never committed; skip registry mutations.
                                tracing::warn!(
                                    id = %self.id,
                                    "child exited before spawn was committed; skipping registry ops"
                                );
                                self.daemon.broadcast(
                                    self.id,
                                    SessionEvent::Exited { code: exit_code },
                                );
                                return;
                            }
                        }
                    }
                    self.on_exit(exit_code);
                    return;
                }
                _ = lag_check.tick() => self.resync_lagging(),
                // Only a watched session is worth a git run.
                _ = meta_tick.tick(), if !self.subs.is_empty() => {
                    self.meta.refresh(self.tracker.claude_session_id.as_deref(), Duration::ZERO);
                }
            }
        }
    }

    /// Timer-driven resync for lagging subscribers.
    ///
    /// Called on every lag-check tick regardless of new PTY output, so a
    /// subscriber that overflowed during a burst gets resynced even if the
    /// child subsequently goes silent (e.g. waiting for input).
    fn resync_lagging(&mut self) {
        if !self.subs.values().any(Subscriber::can_resync) {
            return;
        }
        let id = self.id;
        let size = self.screen.size();
        let snapshot = self.screen.snapshot();
        self.subs.retain(|_, sub| {
            if sub.can_resync() {
                sub.sync(id, size, &snapshot)
            } else {
                !sub.queue.is_closed()
            }
        });
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Input(bytes) => self.write(bytes),
            Cmd::Attach {
                client,
                queue,
                req,
                rows,
                cols,
            } => self.attach(client, queue, req, rows, cols),
            Cmd::Detach { client } => {
                self.subs.remove(&client);
            }
            Cmd::Resize { client, rows, cols } => self.resize(client, rows, cols),
            Cmd::Hook { event, payload } => self.on_hook(&event, &payload),
            Cmd::Status(reply) => {
                let _ = reply.send(Status {
                    state: self.tracker.state,
                    title: self.title.clone(),
                });
            }
            // Handled by `run`.
            Cmd::Kill | Cmd::Stop(_) => {}
        }
    }

    /// PTY output: answer probes, update the screen, then fan out.
    fn on_output(&mut self, bytes: &[u8]) {
        self.probe.feed(bytes);
        let replies = self.probe.take_responses();
        if !replies.is_empty() {
            self.write(replies);
        }
        self.screen.feed(bytes);
        self.update_title();
        self.fan_out(bytes);
    }

    fn update_title(&mut self) {
        let title = self.screen.title().map(|t| clean_title(&t).to_owned());
        if title != self.title {
            self.title = title.clone();
            let title = title.unwrap_or_default();
            self.daemon
                .broadcast(self.id, SessionEvent::Title { title });
        }
    }

    /// Send output to every subscriber; resync lagging ones that caught up.
    /// The screen already includes `bytes`, so a resync replaces them.
    fn fan_out(&mut self, bytes: &[u8]) {
        let (id, size) = (self.id, self.screen.size());
        let snapshot = self
            .subs
            .values()
            .any(Subscriber::can_resync)
            .then(|| self.screen.snapshot());
        self.subs.retain(|_, sub| match (&snapshot, sub.lagging) {
            (Some(snap), true) if sub.can_resync() => sub.sync(id, size, snap),
            (_, true) => !sub.queue.is_closed(),
            (_, false) => sub.offer(id, bytes),
        });
    }

    fn attach(
        &mut self,
        client: ClientId,
        queue: mpsc::Sender<Frame>,
        req: Option<u64>,
        rows: u16,
        cols: u16,
    ) {
        // Re-attaching replaces the old subscription.
        self.subs.remove(&client);
        self.resize(client, rows, cols);
        let mut sub = Subscriber {
            queue,
            lagging: false,
            pending_req: req,
        };
        if sub.sync(self.id, self.screen.size(), &self.screen.snapshot()) {
            self.subs.insert(client, sub);
        }
    }

    /// Resize the PTY, screen and probe responder, then resync every
    /// subscriber except `client` (their mirrors are now the wrong size).
    fn resize(&mut self, client: ClientId, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 || self.screen.size() == (rows, cols) {
            return;
        }
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Err(e) = self.master.resize(size) {
            tracing::warn!(id = %self.id, error = %e, "pty resize failed");
        }
        self.screen.resize(rows, cols);
        self.probe.resize(rows, cols);

        let (id, snapshot) = (self.id, self.screen.snapshot());
        self.subs
            .retain(|c, sub| *c == client || sub.sync(id, (rows, cols), &snapshot));
    }

    fn on_hook(&mut self, event: &str, payload: &Value) {
        for ev in self.tracker.apply(event, payload) {
            if let SessionEvent::ClaudeSession { claude_session_id } = &ev {
                self.daemon
                    .record_claude_session(self.id, claude_session_id);
            }
            self.daemon.broadcast(self.id, ev);
        }
        // Refresh the metadata (git status, model, tokens) after a hook. The
        // turn boundaries always refresh; a burst of tool-use hooks shares one.
        let min_gap = match event {
            "SessionStart" | "Stop" | "StopFailure" | "UserPromptSubmit" => Duration::ZERO,
            _ => META_HOOK_GAP,
        };
        self.meta
            .refresh(self.tracker.claude_session_id.as_deref(), min_gap);
    }

    fn write(&mut self, bytes: Vec<u8>) {
        if let Err(std_mpsc::TrySendError::Full(_)) = self.input.try_send(bytes) {
            tracing::warn!(id = %self.id, "pty input queue full; input dropped");
        }
    }

    /// The child exited: the session goes dormant (it stays journaled).
    ///
    /// **Resume-failure retry (design §2.3):** if this spawn used `--resume`
    /// and the child exited within [`RESUME_RETRY_WINDOW`] with a non-zero
    /// exit code before any `SessionStart` hook fired, we broadcast a notice
    /// and re-spawn without `--resume` (the conversation may have been deleted).
    /// The retry happens at most once per spawn.
    fn on_exit(&mut self, code: Option<i32>) {
        tracing::info!(id = %self.id, ?code, "session exited");

        // Check resume-retry condition (§2.3).
        let quick_exit = self.spawn_time.elapsed() < RESUME_RETRY_WINDOW;
        let no_session_start = self.tracker.state == crate::proto::SessionState::Starting;
        let nonzero_exit = code.map(|c| c != 0).unwrap_or(true);

        if self.spawned_with_resume && quick_exit && no_session_start && nonzero_exit {
            tracing::info!(id = %self.id, "resume failed (conversation gone?); will retry fresh");
            // Forget the stale live entry before retrying, so retry_spawn_fresh
            // does not find a stale entry and return the old pid without spawning.
            self.daemon.forget_live(self.id, &self.token);
            // Emit a notice (not Title) so the TUI shows it in the status bar.
            self.daemon.broadcast(
                self.id,
                SessionEvent::Notice {
                    text: "conversation not found; started fresh".to_owned(),
                },
            );
            // Attempt a fresh spawn (no --resume) via the daemon.
            // This is best-effort; we proceed to dormant on failure.
            let id = self.id;
            let daemon = Arc::clone(&self.daemon);
            tokio::spawn(async move {
                daemon.retry_spawn_fresh(id).await;
            });
            return;
        }

        let ours = self.daemon.forget_live(self.id, &self.token);
        self.daemon.broadcast(
            self.id,
            SessionEvent::State {
                state: SessionState::Exited,
            },
        );
        self.daemon
            .broadcast(self.id, SessionEvent::Exited { code });

        // A clean exit is the user ending the conversation (Ctrl+D twice,
        // `/exit`): close the session like a Kill would. A crash leaves it
        // dormant so it can come back with `--resume`. A shell has nothing to
        // resume, and exits with its last command's status (Ctrl+D after a
        // failing command is still "I am done"), so any exit closes it.
        // Not a Kill: a respawn that took the id over since `forget_live`
        // must keep its new process.
        if ours && (code == Some(0) || self.kind == SessionKind::Shell) {
            self.daemon.close_exited(self.id);
        }
    }

    /// SIGHUP the child; SIGKILL it if it is still around after a grace
    /// period. The waiter has not reaped it yet, so the pid is still ours.
    async fn kill(mut self, exit: oneshot::Receiver<Option<i32>>) {
        let _ = self.killer.kill();
        if tokio::time::timeout(KILL_GRACE, exit).await.is_err() {
            if let Some(pid) = self.pid.and_then(|p| i32::try_from(p).ok()) {
                // SAFETY: kill(2) has no memory-safety preconditions.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

/// Read the current git branch from `cwd/.git/HEAD` (the fallback when git
/// is not installed). `None` when the directory is not a git repo or HEAD is
/// detached.
fn read_git_branch(cwd: &std::path::Path) -> Option<String> {
    let head_path = cwd.join(".git/HEAD");
    let content = std::fs::read_to_string(head_path).ok()?;
    let line = content.trim();
    line.strip_prefix("ref: refs/heads/").map(str::to_owned)
}

#[cfg(test)]
mod tests {
    #[test]
    fn title_glyphs_are_stripped() {
        assert_eq!(clean_title("◐ Claude Code"), "Claude Code");
        assert_eq!(clean_title("✳ Fix the parser"), "Fix the parser");
        assert_eq!(clean_title("plain"), "plain");
        assert_eq!(clean_title("◑ "), "");
    }

    use super::*;

    #[test]
    fn full_queue_lags_then_resyncs_with_pending_req() {
        let id = uuid::Uuid::new_v4();
        let (queue, mut rx) = mpsc::channel(RESYNC_ROOM + 8);
        let mut sub = Subscriber {
            queue,
            lagging: false,
            pending_req: Some(7),
        };

        // Fill the queue: the next output marks it lagging (not gone).
        for _ in 0..RESYNC_ROOM + 8 {
            assert!(sub.offer(id, b"x"));
        }
        assert!(!sub.lagging);
        assert!(sub.offer(id, b"y"));
        assert!(sub.lagging);
        assert!(!sub.can_resync());

        // Once it drains, a resync sends Attached (with the pending req) and
        // the snapshot, and clears the flag.
        while rx.try_recv().is_ok() {}
        assert!(sub.can_resync());
        assert!(sub.sync(id, (24, 80), b"SNAP"));
        assert!(!sub.lagging);
        let attached = Envelope {
            req: Some(7),
            msg: Msg::Attached {
                id,
                rows: 24,
                cols: 80,
            },
        };
        assert_eq!(rx.try_recv().unwrap(), Frame::Control(attached));
        assert_eq!(
            rx.try_recv().unwrap(),
            Frame::Data {
                session: id,
                bytes: b"SNAP".to_vec()
            }
        );
        assert_eq!(sub.pending_req, None);

        // A vanished client is reported so the actor can drop it.
        drop(rx);
        assert!(!sub.offer(id, b"z"));
    }

    #[test]
    fn output_is_chunked() {
        let id = uuid::Uuid::new_v4();
        let (queue, mut rx) = mpsc::channel(8);
        let mut sub = Subscriber {
            queue,
            lagging: false,
            pending_req: None,
        };
        assert!(sub.offer(id, &vec![b'a'; CHUNK + 1]));
        let sizes: Vec<usize> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|f| match f {
                Frame::Data { bytes, .. } => bytes.len(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(sizes, [CHUNK, 1]);
    }

    #[test]
    fn env_is_scrubbed_and_token_replaces_api_key() {
        let mut cmd = CommandBuilder::new("claude");
        cmd.env("CLAUDECODE", "1");
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "cli");
        cmd.env("CLAUDE_CODE_CHILD_SESSION", "1");
        cmd.env("CLAUDE_CODE_USE_GATEWAY", "1");
        cmd.env("MY_SETTING", "keep");
        cmd.env("ANTHROPIC_API_KEY", "old");
        apply_env(
            &mut cmd,
            None,
            &[("ANTHROPIC_AUTH_TOKEN".into(), "tok".into())],
        );
        assert_eq!(cmd.get_env("CLAUDECODE"), None);
        assert_eq!(cmd.get_env("CLAUDE_CODE_ENTRYPOINT"), None);
        assert_eq!(cmd.get_env("CLAUDE_CODE_CHILD_SESSION"), None);
        // A first-party env drops an inherited gateway flag; user settings survive.
        assert_eq!(cmd.get_env("CLAUDE_CODE_USE_GATEWAY"), None);
        assert_eq!(
            cmd.get_env("MY_SETTING").and_then(|v| v.to_str()),
            Some("keep")
        );
        assert_eq!(cmd.get_env("ANTHROPIC_API_KEY"), None);
        assert_eq!(cmd.get_env("ANTHROPIC_AUTH_TOKEN"), Some(OsStr::new("tok")));
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(cmd.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
    }

    #[test]
    fn shell_command_is_a_plain_login_shell() {
        let cmd = shell_command(Path::new("/bin/zsh"), Path::new("/tmp"), None);
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, ["/bin/zsh", "-l"], "no --settings, no hooks");
        assert_eq!(cmd.get_cwd().map(|c| c.as_os_str()), Some(OsStr::new("/tmp")));
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(cmd.get_env("CLAUDECODE"), None);
    }

    #[test]
    fn login_path_is_set_unless_the_spec_sets_one() {
        let login = OsStr::new("/home/u/.npm-packages/bin:/usr/bin");
        let mut cmd = CommandBuilder::new("claude");
        cmd.env("PATH", "/usr/bin");
        // A proxy env (no PATH of its own) keeps the login PATH.
        apply_env(
            &mut cmd,
            Some(login),
            &[("ANTHROPIC_AUTH_TOKEN".into(), "t".into())],
        );
        assert_eq!(cmd.get_env("PATH"), Some(login));
        let mut cmd = CommandBuilder::new("claude");
        apply_env(
            &mut cmd,
            Some(login),
            &[("PATH".into(), "/explicit".into())],
        );
        assert_eq!(cmd.get_env("PATH"), Some(OsStr::new("/explicit")));
        // Without a login PATH, the daemon's own stays.
        let mut cmd = CommandBuilder::new("claude");
        cmd.env("PATH", "/usr/bin");
        apply_env(&mut cmd, None, &[]);
        assert_eq!(cmd.get_env("PATH"), Some(OsStr::new("/usr/bin")));
        // Terminals get it too.
        let cmd = shell_command(Path::new("/bin/sh"), Path::new("/tmp"), Some(login));
        assert_eq!(cmd.get_env("PATH"), Some(login));
    }

    #[test]
    fn api_key_kept_without_auth_token() {
        let mut cmd = CommandBuilder::new("claude");
        cmd.env("ANTHROPIC_API_KEY", "key");
        apply_env(&mut cmd, None, &[]);
        assert_eq!(cmd.get_env("ANTHROPIC_API_KEY"), Some(OsStr::new("key")));
    }

    #[test]
    fn command_line_shape() {
        let cfg = super::super::Config {
            socket: "/run/d.sock".into(),
            lock: "/run/d.lock".into(),
            journal: "/j.json".into(),
            claude: "/bin/claude".into(),
            claudio: "/bin/claudio".into(),
            skip_permissions: true,
            build: None,
            path: None,
        };
        let spec = SpawnSpec {
            id: uuid::Uuid::nil(),
            cwd: "/".into(),
            name: Some("work".into()),
            args: vec!["--resume".into(), "abc".into()],
            env: vec![],
            rows: 24,
            cols: 80,
            ephemeral: false,
        };
        let cmd = command(&cfg, &spec, Path::new("/"), "tok123", false);
        let argv = argv_of(&cmd);
        assert_eq!(argv[0], "/bin/claude");
        assert_eq!(argv[1], "--settings");
        let settings: Value = serde_json::from_str(&argv[2]).unwrap();
        let hook = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(hook.contains("tok123") && hook.contains("/run/d.sock"));
        assert_eq!(&argv[3..], ["--resume", "abc", "-n", "work"]);

        // With skip-permissions on, the flag follows --settings, ahead of
        // the spec args (which are what the journal stores).
        let cmd = command(&cfg, &spec, Path::new("/"), "tok123", true);
        assert_eq!(
            &argv_of(&cmd)[3..],
            [ALLOW, "--resume", "abc", "-n", "work"]
        );
        assert_eq!(spec.args, ["--resume", "abc"]);
    }

    const ALLOW: &str = "--allow-dangerously-skip-permissions";

    fn argv_of(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn skip_permissions_not_duplicated() {
        let cfg = super::super::Config {
            socket: "/run/d.sock".into(),
            lock: "/run/d.lock".into(),
            journal: "/j.json".into(),
            claude: "/bin/claude".into(),
            claudio: "/bin/claudio".into(),
            skip_permissions: true,
            build: None,
            path: None,
        };
        for given in [ALLOW, "--dangerously-skip-permissions"] {
            let spec = SpawnSpec {
                id: uuid::Uuid::nil(),
                cwd: "/".into(),
                name: None,
                args: vec![given.into()],
                env: vec![],
                rows: 24,
                cols: 80,
                ephemeral: false,
            };
            let argv = argv_of(&command(&cfg, &spec, Path::new("/"), "t", true));
            assert_eq!(&argv[3..], [given]);
        }
    }
}
