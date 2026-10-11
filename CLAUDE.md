# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`claudio` is a session manager TUI and drop-in wrapper for the `claude` CLI. It has four modes, chosen in `src/main.rs`:

- **No arguments (bare `claudio`)**: it opens the **session manager** TUI — a terminal multiplexer for many Claude Code sessions, local and over SSH.
- **No `-p`/`--print`**: it `exec`s the real `claude` unchanged (passthrough).
- **`-p`**: it emulates print mode by driving the *interactive* TUI under a PTY. It types the prompt, waits for the `Stop` hook, then reads the answer and real token usage from the session JSONL.
- **`--api`**: it serves an OpenAI-compatible server (axum) on the same PTY backend, using a pool of persistent interactive sessions.

**claudio never invokes `claude -p`.** Every path goes through the interactive TUI. Don't install the binary *as* `claude`, because it has to find the real one on `PATH` (or `$CLAUDIO_CLAUDE_PATH`).

## Commands

```bash
make                 # release build → target/release/claudio
make static          # static musl binary → dist/claudio (ARCH=aarch64 for cross)
make test-unit       # unit tests only: cargo test --locked --bin claudio (no claude needed)
make test-manager    # manager integration tests (fake claude, no API key required)
make test            # unit + E2E (E2E needs an authenticated `claude` on PATH, ~5 min)
make e2e             # E2E only
make fmt             # cargo fmt
```

### Test suites and gates

**Manager integration tests** (fake `claude`, no real API):

```bash
make test-manager
# equivalent:
cargo test --test manager -- --test-threads=1
```

Must run single-threaded. Two tests inside are gated:

- `CLAUDIO_E2E=1` — enables `test_real_claude_session` (sends a prompt to the
  real `claude`, costs tokens, requires an authenticated `claude` on PATH).
- `CLAUDIO_PROXY_TEST=1` — enables the proxy-integration test. It builds and
  runs claude-proxy from source (`$CLAUDIO_PROXY_SRC`, default `../claude-proxy`;
  needs `go`).
  The test for real-claude via proxy additionally needs `CLAUDIO_E2E=1` and
  `CLAUDIO_PROXY_URL`.

**Print-mode / API E2E tests** (tests/integration.rs):

```bash
CLAUDIO_E2E=1 CLAUDIO_CADENCE=0 cargo test --test integration -- --test-threads=1 --nocapture
```

Without `CLAUDIO_E2E=1` the integration tests silently no-op. They call the
real Claude API and must run single-threaded.

**Remote / SSH tests** (tests/remote.rs):

```bash
CLAUDIO_SSH_TEST_HOST=<host> cargo test --test remote -- --test-threads=1 --nocapture
# diag subcommands (compiled in by default; may be gated behind --features diag in future):
cargo test --features diag --test remote -- --test-threads=1 --nocapture
```

Tests `t1`, `t7`, `t8` are unit-style and always run. Tests `t2`–`t6` require
`CLAUDIO_SSH_TEST_HOST` to be set to a reachable SSH host alias.

- Run one unit test: `cargo test --bin claudio <name_substring>`.
- Run one E2E test: `CLAUDIO_E2E=1 CLAUDIO_CADENCE=0 cargo test --test integration <name> -- --test-threads=1 --nocapture`.
- CI runs only the release build and unit tests, on Linux and macOS (claudio is Unix-only). Keep code compiling on both.

## Architecture

### Session manager (bare `claudio`)

A bare `claudio` calls `tui::run()`. The manager's architecture spans:

**`proto.rs`** — wire framing shared by daemon and client. Frames are
`[u32-BE length][u8 tag][payload]`. Tag `J` carries JSON control messages
(`Envelope { req, msg: Msg }`); unknown `op` fields are ignored. Tag `D`
carries terminal bytes: `[16-byte session UUID][raw bytes]`. The protocol
version (`PROTO = 1`) is embedded in the socket file name so old and new
daemons coexist.

**`daemon/`** — the per-host session daemon (invoked as `claudio --daemon`):
- `mod.rs` — shared `Daemon` state: the registry of journaled and live
  sessions, spawn as reserve → spawn → commit (`in_flight`/`to_kill`), kill,
  `respawn` (Alt+e; a reserved transition, so a concurrent Kill wins), and
  `close_exited` (a clean claude exit, or any shell exit, removes the session
  unless something replaced it meanwhile). `Config::claude_bin()` resolves
  claude on every use: `$CLAUDIO_CLAUDE_PATH` if set, else `claude` in
  `Config::path`, then well-known install dirs (`~/.local/bin`,
  `~/.npm-packages/bin`, …).
- `login_path.rs` — at daemon start, runs the user's login shell (`$SHELL
  -l -i -c`, setsid, stdin `/dev/null`, 3 s deadline, group killed on
  timeout) and reads its `PATH` between markers. It is merged with the
  inherited `PATH` (login first) into `Config::path`. A daemon started over
  ssh only has the system `PATH`, which can find a stale `/usr/local/bin/claude`.
  `Config::path` is used to find claude, and is the `PATH` of every session,
  terminal, `claude --version/--help` probe and `claude update`. A spec's own
  `PATH` still wins. On failure the inherited `PATH` stays. Tests never read
  real rc files. Unit tests use fake shell scripts, daemon tests build
  `Config` directly, and the manager harness pins `SHELL=/bin/sh` with a temp
  `HOME`.
- `server.rs` — accepts Unix socket connections, uid-checks peers, runs the
  per-client loop. Slow requests (git, `UpdateClaude`, attach) reply from
  spawned tasks through the client's queue, never inline.
- `session.rs` — one actor task per live session: owns the PTY writer, VT
  engine (`alacritty_terminal`), and subscriber list. Fans PTY output to
  all attached clients. Hook events (`__hook <Event> <socket> <token>`) are
  authenticated by uid plus a per-spawn token, enforced with a read deadline
  and size cap. On `SessionStart` the claude session id is journaled
  immediately so recovery works even when no client is attached. A session
  is `SessionKind::Claude` or `Shell` (terminal tab: a login shell, no hooks).
  `command()` adds `--allow-dangerously-skip-permissions` when the config
  allows it and this host's `claude --help` lists it. `MetaRefresher`
  produces `SessionEvent::Meta` (git status, model, context, output tokens,
  turns) off the actor task: on hooks (tool-use bursts share one refresh)
  and every 10 s while a client is attached; one refresh runs at a time. The
  transcript is read incrementally (`claude::projects::TranscriptCursor`).
- `git_status.rs` — `git status --porcelain=v2 --branch` for the status
  bar's `⎇` segment: 1 s deadline, capped output, same scrubbed env as
  `git.rs`.
- `journal.rs` — durable session list (`daemon-sessions-v1.json`): writes via
  temp + fsync + rename (`paths::write_atomic`). Never stores secrets or the
  daemon's `--settings` injection; ephemeral (`--plain`) sessions are not
  written. On fresh daemon start, non-killed entries are re-spawned with
  `claude --resume <claude_session_id>`.
- `git.rs` — read-only git queries for the git viewer (log, commit, diff):
  bounded output, sanitized text, no textconv/ext-diff, repo env scrubbed.
- `update.rs` — `claude update` (or the official installer) on request, in its
  own process group, killed as a group on timeout.
- `host.rs`, `stats.rs` — host facts for `Welcome`, directory listing, CPU and
  memory samples. `ctl.rs` — `claudio daemon status|stop|restart` (status
  shows the claude path and version the daemon uses).

**`client.rs`** — generic client connection and local daemon autostart.
Connects to the socket; if unavailable, starts the daemon (double-fork +
`setsid`, or `systemd-run --user` on systemd hosts) and retries.

**`freshness.rs`** — keeps each host's daemon at least as new as the claudio
binary on it. `Welcome.build`/`build_time` identify the daemon's binary;
`ensure_current` runs before the local TUI connects and in the remote bridge
before it relays. An older daemon is shut down (SIGTERM by the lock's PID if
it predates `Shutdown`) and restarted from the current binary, unless
`busy` (a claude turn or permission prompt, or a `--plain` session): then it
is left running, the client is flagged `daemon_outdated` (over SSH the
bridge prints `DEFERRED` on stderr), and the TUI reconnects that host once it
is idle (checked every 2 min).

**`tui/`** — the manager UI, driven by tokio + ratatui:
- `mod.rs` — I/O edge: owns the terminal, runs the event loop, gathers the
  `AppConfig`, drives daemon connections and every `Effect`. Emits OSC 9
  notifications (`ESC ] 9 ; … BEL`) for background sessions that need
  attention.
- `app.rs` — pure coordinator (`App::new(AppConfig)`): processes events into
  state transitions and returns `Effect` values. No I/O here; all I/O is in
  `mod.rs`. `Mode::Manager` or `Mode::Plain`.
- `sessions.rs` — `SessionView` and spawning (`send_spawn`/`send_with_proxy`,
  the one path that builds spawns, respawns and their proxy env), recovery,
  the wizard's opening.
- `interaction.rs` — keys, mouse, modals (`Modal`), daemon events and replies.
- `wizard.rs` — the new-session wizard (start screen → directory browser →
  resume?), a pure state machine.
- `git_view.rs` + `git_app.rs` — the git viewer (Alt+l): pure view state
  machine, and its `App` glue.
- `stats_view.rs` — the multi-page proxy stats popup (Alt+s).
- `confirm.rs` — the generic confirm prompt (kill, reset, claude update).
- `claude_update.rs` — local/remote claude version checks and updates.
- `plain.rs` — `claudio --plain`: `Mode::Plain` is one local, ephemeral
  session full screen (no tab/status bar, wizard or state.json).
  `Keymap::build_plain` keeps only Alt+h/s/l/e; the session is killed when
  the UI exits, and the UI exits with claude.
- `connections.rs` — local + SSH connections, reconnect backoff.
- `ui/` — ratatui rendering: `mod.rs` (layout, tab bar, pane, popups),
  `status.rs` (two-line bottom bar; line 2 is prioritised segments — cpu,
  ctx gauge, git, mem, 5h, conversation, 7d — dropped whole when narrow),
  `stats.rs`, `git.rs`, `confirm.rs`.
- `fmt.rs` — shared text and number formatting.
- `keymap.rs` — `DEFAULT_BINDINGS` table (the single source of truth); `Keymap`
  (defaults + config overrides); `parse_key_spec`; collision detection.
  `Alt+Shift+<digit>` (go to tab N) is fixed, outside the table.
- `state.rs` — `ClientState`: persisted session order, names and recent dirs
  (`state.json`).
- `proxy_state.rs` — per-profile proxy stats and per-session credential cache.
- `notifications.rs` — debounce logic for attention notifications.
- `selection.rs` — copy-on-select (`[ui] copy_on_select`, default on): the
  pure `Selection` (anchor/head spans, cell/word/line unit, `contains`),
  multi-click counting and the OSC 52 encoder. A left drag in the pane
  selects only while the session's mouse reporting is off (otherwise the
  mouse is forwarded as before); the release emits `Effect::CopyToClipboard`
  with `Screen::text` and a "copied N chars" notice. The selection is
  cleared by a click, key, paste, tab switch, resize or re-attach. Glue:
  `App::select_mouse` in `interaction.rs`; highlight in `ui/mod.rs`.
- `test_support.rs` — shared fixtures for the TUI's unit tests.

**`remote/`** — SSH bootstrap and bridge:
- `bootstrap.rs` — `ensure_remote(host)`: runs `__probe` + `uname -sm` in one
  ssh round trip; uploads the binary if missing or stale (SHA-256 check).
  Same-platform: uploads self. Cross-platform: downloads release asset to
  `~/.cache/claudio/<version>/` and uploads that.
- `bridge.rs` — `--slave` mode: the stdio↔socket bridge the local client runs
  on the remote as `ssh HOST ~/.local/bin/claudio --slave`. Starts the remote
  daemon if not running (`systemd-run --user` preferred, `setsid` fallback),
  and replaces an outdated one (`freshness.rs`).
- `hosts.rs` — SSH host candidates: MRU list (`hosts.json`) + `~/.ssh/config`
  aliases.
- `probe.rs` — `__probe` subcommand: prints `{version, proto, os, arch,
  build, mtime}` as JSON (`Probe::own()`: this binary, hashed once).

**`proxy/`** — claude-proxy HTTP client and profile management:
- `profile.rs` — load/save profiles from `config.toml` (`[proxy]` section,
  mode 0600); `from_env()` parses `CLAUDIO_PROXY_URL=<token>@host`.
- `resolve.rs` — `ProxyChoice` and profile resolution, shared by the TUI and
  `--plain`.
- `env.rs` — `session_env(profile, config)`: builds the env for a
  proxy-backed session (falls back to built-in model defaults when the
  proxy's `/v1/claudio/config` is unreachable); `env_diff` is the one rule
  for what a launched claude inherits (session markers scrubbed, an API key
  dropped when a proxy token is set).
- `cmd.rs` — `claudio proxy login|status|logout|use`.
- `api.rs` — HTTP client for `/v1/claudio/config`, `/models`, `/me/stats`,
  `/pool/health` and `/session` (which credential a claude session is on).

**`term/`** — terminal primitives:
- `mod.rs` — `alacritty_terminal::Term`-based screen + snapshot logic.
- `keys.rs` — key → byte encoding for input forwarding to the PTY.

**`claude/`** — claude knowledge:
- `mod.rs` — `SESSION_MARKERS` (env a parent claude sets for its children),
  `binary()`, `exec()`.
- `projects.rs` — scan `~/.claude/projects/` for existing sessions (feeds the
  wizard's resume list).
- `hooks.rs` — `__hook <Event> <socket> <token>` relay: sends the hook payload
  to the daemon socket.
- `state.rs` — session state tracker derived from hook events.
- `update.rs` — version parsing and the daily release-channel check.

### Non-obvious constraints (don't "fix" these — manager)

- **Keys must never collide with claude/terminal bindings.** All manager keys
  use the `Alt` modifier. `keymap.rs` enforces no-collision among manager
  bindings; the `config.toml` parser rejects collisions with a notice.
- **`Attached` precedes the snapshot `D` frames.** The daemon sends the
  `Attached` control message, then immediately the snapshot as a `D` frame.
  The client must not render anything until it has received `Attached`.
- **Protocol additions must be backward compatible.** New `op` values in `Msg`
  must be accepted (ignored) by older daemons. New `SessionEvent` variants
  deserialize to `SessionEvent::Unknown` so an old client never panics.
- **The PTY test harness asserts on the rendered screen model, never on raw
  bytes.** All `wait_for` calls in `tests/manager.rs` operate on the
  `alacritty_terminal::Term` VT model. Asserting on raw bytes is fragile and
  forbidden.
- **Don't send `ESC` immediately before `Alt+key` in tests.** `ESC ESC q` is
  ambiguous — the VT parser may interpret it as two separate `ESC` sequences.
  Use the crossterm key-event encoding instead.
- **Secrets never go in argv or state files.** Proxy tokens are passed only
  through `Spawn.env` over the Unix socket. The journal and `state.json` store
  only profile names, never tokens.
- **New request ops must fail safe on old daemons.** An old daemon answers an
  unknown op with `Error "unsupported op…"`; `App::older_daemon_notice` turns
  that into "run `claudio daemon restart`". Never add a field to an existing
  op that an old daemon would silently ignore into wrong behaviour (that is
  why terminals are `SpawnShell`, not `Spawn{kind}`).
- **A daemon is replaced only by a newer build.** `freshness::compare` orders
  builds by version, then binary mtime; "different" is not enough, or two
  binaries (a release and a dev build) would replace each other's daemon
  forever. A daemon keeps its startup lock until it has fully stopped, so
  its successor never runs beside it.
- **Test fakes and the test config.** The daemon runs `claude --help` once to
  probe for flags, so fake claudes must ignore `--help` (and `--version`)
  without side effects. Every test claudio starts with `[claude]
  update_check/remote_check = "off"`: the update prompts are modal and would
  swallow the keys a test types.
- **`app.rs` is pure; I/O is only in `tui/mod.rs`.** `App::update` takes
  events and returns `Effect` values. `mod.rs` drives the effects. Don't add
  I/O to `app.rs`.

### Print-mode pipeline (`-p`)

1. `cli.rs` handles arguments *dynamically*. It owns only a few flags (`-p`, `--api`, `--fast`, `--log-messages[-file]`, `--output-format`, `--input-format`, `--settings`, `--session-id`), drops `--bare` and `--no-session-persistence` with a warning (they would disable hooks and the JSONL), and forwards everything else verbatim. Never add a mirror of claude's flag grammar. Wrapper-only knobs belong in `CLAUDIO_*` env vars (`WrapperEnv`) so the CLI surface stays exactly claude's. `cli::HELP_APPENDIX` documents these flags and env vars and is appended to `claudio --help`; update it when you add one.
2. `driver.rs` spawns claude under a PTY (`portable-pty`). `PtySession` is the reusable unit: spawn, then `turn(prompt)` repeatedly. `driver::run` is the single-shot `-p` path built on it.
3. `vt.rs` answers the terminal probes that Ink sends at startup (DA1/DA2, DSR, XTVERSION, OSC colors…), byte-for-byte as xterm-380 would.
4. `hooks.rs` signals completion. `SessionStart`/`Stop` hooks are injected via a merged `--settings` and report back over loopback TCP (default) or a watched directory (`CLAUDIO_HOOK_TRANSPORT=file`). The hook command is a copy of this binary under a random hex name in `/tmp`, so `main.rs` has to recognize relay invocations first: argv `<EventName> <port>` (concealed relay) or `__hook <Event>` (legacy).
5. `session.rs` reads the session JSONL, which is the source of truth for the final assistant text and usage. The terminal render is never parsed for answers.
6. `emit.rs` formats output as `text`, `json`, or `stream-json`, shaped like `claude -p`.

### API server (`--api`, `src/api/`)

- `routes/chat.rs` takes a request down one of two paths:
  - **plain**: `prompt.rs` flattens `messages[]`.
  - **agentic** (request has `tools` and `CLAUDIO_API_AGENTIC` is on): `agentic.rs` builds a "tool-execution gateway" prompt and parses a fenced ```` ```tool_calls ```` block back into OpenAI `tool_calls`.

  Either way, the turn is resolved whole, so streaming is "resolve then chunk" into a fixed SSE set.
- `backend.rs` dispatches to `pool.rs` (`SessionPool`). OpenAI clients resend the full history, so the pool matches a request to a live conversation with a cumulative **prefix-hash chain** and types only the delta.
  - Past `CLAUDIO_API_MAX_LIVE`, LRU sessions are demoted: the process is killed and the mapping kept. They are revived later with `claude --resume`.
  - The system/agentic prompt is re-injected every `CLAUDIO_API_REINJECT_TURNS` turns.
- `config.rs` reads `CLAUDIO_API_*` env vars, falling back to legacy `OPENAI_PROXY_*` names. `--api` implies `CLAUDIO_FAST=1`.

### Non-obvious constraints (don't "fix" these)

- **The system prompt goes in the typed user message, never `--system-prompt`.** Anthropic classifies the system prompt of interactive requests. A third-party-looking one gets the request billed as a third-party app (`400 … extra usage`). The user message isn't scored.
- **The client's system prompt is never relayed verbatim in agentic mode.** A prompt describing another harness ("you are operating inside pi…") makes Opus/Sonnet treat it as prompt injection. `agentic.rs` instead reads it for facts: `ClientEnv::detect` fingerprints pi, opencode, hermes, or generic, and extracts the working directory and date. It then presents its own neutral gateway framing.
- **Tool argument help is rendered from each client's JSON Schema.** Don't add hardcoded per-name argument tables, because clients name the same concept differently (pi `path` vs opencode `filePath`).
- **API sessions run with `--tools ""`, `--strict-mcp-config`, `--disable-slash-commands`, and `--setting-sources project`, in a clean cwd** (`CLAUDIO_API_CWD`, default `/tmp`). The server never executes tools; the client does.
- **Prompts are typed with bracketed paste** (auto-detected from `?2004h`) so multi-line prompts submit atomically. Typing cadence and delays are cosmetic (`CLAUDIO_CADENCE`, `CLAUDIO_FAST`). The Ink-quiescence wait is a correctness wait.

### Debugging

- `CLAUDIO_DEBUG=1` prints a trace timeline.
- `CLAUDIO_RAW_LOG=<path>` dumps the raw PTY bytes.
- `--log-messages` (`msglog.rs`) logs all four hops (CLI→claudio→claude→claudio→CLI), correlated by id. `--log-messages-file <path>` writes them untruncated as JSONL.
- The API server logs pool decisions at INFO (`MATCH`, `RESUME`, `demoting LRU`).

`docs/print-and-api.md` has the full env-var tables and client configs for pi, opencode, and hermes. Docker (`docker-compose.yml`) mounts `./workspace` as `/work`.
