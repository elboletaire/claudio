# claudio manager — design (v1)

Status: **implemented on branch `manager`**. This version folds in the Fable
5.1 architecture review and the GPT Astra security review.
**[open]** = the user still needs to decide. **[spike]** = settled by the P0
prototype. **[later]** = deliberately deferred.

## Implementation notes / deviations from design

The following are places where the implementation differs from or does not yet
cover the design above. They are listed as facts, not as bugs.

### Deviations that were intentional

- **Arrow keys chosen for session navigation.** The design left keys as
  `[open]`. The implementation chose `Alt+←` / `Alt+→` for prev/next session,
  `Alt+n` for new, `Alt+r` for rename, `Alt+x` for close, `Alt+a` for
  next-attention, `Alt+s` for proxy stats, `Alt+g` for overview, `Alt+h` for
  help, `Alt+q` for quit. `Alt+←/→` were in the design's "likely word movement
  in claude's input" ruled-out list; they were reconsidered and verified free
  in practice by the user.
- **`--proxy <host>` flag not implemented.** The design mentioned a `--proxy`
  CLI flag. Proxy selection uses `CLAUDIO_PROXY_URL=<token>@host` (ephemeral)
  or a saved default profile from `config.toml`. There is no `--proxy` flag.
- **The statusLine feed is not implemented.** Design §2.2 described chaining
  the user's own `statusLine` command. This is deferred (`[later]` in the
  design).
- **Git worktrees are not implemented.** Design §3.2 step 4 listed git
  worktree creation (via `claude -w <name>`) as an option in the wizard. This
  is deferred.
- **The snapshot does not restore the primary screen.** The snapshot restores
  alt-screen state, bracketed paste, application cursor keys, and cursor
  visibility, but does not serialize and replay the primary screen scroll-back
  buffer. This matches the design intent (the design describes restoring
  modes, not the primary screen), but clients that switch between primary and
  alt screen may see the primary screen blank on reattach.
- **Close requires confirmation, not a command palette.** Design §3.3 said
  "Close is never a single keystroke" and pointed to a command palette.
  The implementation shows a confirm popup (`[y] kill · [n] cancel`; Esc
  cancels too) after `Alt+x`, without a full command palette.
- **`SO_PEERCRED` uid check is platform-dependent.** Linux uses `SO_PEERCRED`.
  macOS uses `getpeereid`. The implementation handles both via the `libc`
  crate.
- **Session journal path.** The design said `~/.local/state/claudio/sessions.json`.
  The implementation uses `~/.config/claudio/daemon-sessions-v1.json` (versioned,
  under `config_dir()`).

### Future work (deferred from design)

- Overview / "mission control" popup (partially implemented: `Alt+g` opens a
  per-session summary list, but it does not show the full last assistant
  message from the Stop hook).
- Scripting CLI (`claudio ls`, `claudio new`, `claudio attach`, `claudio hosts`).
- Auto-respawn with `--resume` when claude crashes.
- Scrollback / copy mode in the terminal mirror.
- Git worktrees.
- Desktop notifications via `notify-send` / `osascript` fallback (currently
  only OSC 9 + BEL is emitted).
- Mouse click to switch tabs.
- `claudio daemon upgrade` / session migration between protocol versions.
- The statusLine feed.
- Supply the proxy token through claude's `apiKeyHelper` so it never sits in
  the process environment.
- zoxide seeding for the directory picker.

## 1. Goal

Turn `claudio` into a terminal-native manager for many Claude Code sessions,
local and over SSH. It should be persistent, resumable and proxy-aware, and aimed
at a senior DevOps user who wants things automatic.

The modes that exist today stay as they are:
- `claudio -p …` emulates print mode.
- `claudio --api` runs the OpenAI-compatible server.
- `claudio <claude args>` is a transparent passthrough.

A bare `claudio` (no args) now opens the manager TUI. Scope is Linux and macOS.

## 2. Architecture

```
            ┌──────────────── claudio (TUI client) ────────────────┐
            │ ratatui UI · per-session VT mirror · key router       │
            │ conn manager (local + ssh) · proxy client · state.json│
            └───────┬───────────────────────────────┬───────────────┘
        unix socket │                                │ ssh host ~/.local/bin/claudio --slave
                    ▼                                ▼  (stdio bridge ⇄ remote unix socket)
        claudio daemon (local)               claudio daemon (remote, detached)
        PTY + VT engine per session          same binary, same protocol
        hook ingest + session journal        survives ssh drops / logout
                    │                                │
                 claude …                         claude …
```

### 2.0 The daemon

- **One daemon per host owns the PTYs.** Closing or crashing the TUI, or an ssh
  drop, never kills `claude`. The local and remote paths are symmetric.
- **No tmux dependency.** The daemon is a small mux, with precedent in herdr,
  wezterm-mux-server and the zed remote server.
- **Socket.** The daemon listens on
  `$XDG_RUNTIME_DIR/claudio/daemon-<proto>.sock`, falling back to
  `/tmp/claudio-$UID/` when there is no XDG runtime dir.
  - The directory is checked to be ours, mode 0700, and not a symlink.
  - Every connection is checked for peer uid (`SO_PEERCRED` / `getpeereid`).
  - The daemon recreates the socket if a tmp cleaner unlinks it.
  - **The protocol version is part of the socket name**, so old and new
    daemons can coexist (see §2.6).
- **Autostart.** The client, or the `--slave` bridge, starts the daemon when
  none is listening. A `flock` on a pidfile next to the socket prevents a race
  that would start two daemons.
  - Detaching uses double-fork + `setsid`.
  - On systemd hosts it uses `systemd-run --user --scope` (or checks
    `loginctl` linger), because logind's `KillUserProcesses` would otherwise
    kill it on ssh logout.
- **Remote.** `ssh host ~/.local/bin/claudio --slave` is a thin stdio⇄socket
  bridge. An ssh drop kills only the bridge.
- **Transport is the system `ssh` binary**, used as a subprocess. One ssh
  process per host carries every session for that host. claudio honours the
  user's `~/.ssh/config`, agent, ProxyJump and ControlMaster, and never manages
  ControlMaster itself.

### 2.0.1 Screen model (byte stream + mirror, the tmux model)

- The daemon runs one VT engine per session. It is authoritative: it answers
  the child's terminal probes (the shared `vt.rs` responder) and tracks the
  screen while no client is attached.
- **Attach always begins with a snapshot.** The snapshot is an escape-sequence
  string that redraws the cells **and restores the terminal modes**:
  - alt screen
  - bracketed paste `?2004`
  - application cursor keys
  - mouse modes `?1000/1002/1006`
  - cursor visibility and position
  - kitty keyboard flags

  If the engine doesn't expose a mode, the daemon shadows the DECSET/DECRST
  sequences itself.
- After the snapshot, live output bytes follow. Snapshot and subscription
  happen under one barrier inside the session actor, so no byte is lost or
  doubled.
- The client feeds snapshot + output into a mirror emulator and renders from
  that. If the two ever drift, any new snapshot resyncs them; the palette
  offers "force redraw".
- **Color queries.** OSC 10/11 queries are answered with the attached client's
  real colors, which the client reports in `Hello`. The hardcoded
  white-on-black is used only while no client is attached.
- **Size.** PTY size follows the latest attach or resize. Output fans out to
  every attached client. A resize triggers a fresh snapshot for the other
  clients, because their mirrors are now the wrong size.

### 2.1 Wire protocol

Length-prefixed frames with a 1-byte tag. Frame size has a hard cap (e.g.
1 MiB).

- **Control frames (tag `J`)** are JSON objects with an `"op"` field, an
  optional `"req"` id, and unknown fields ignored. They are debuggable with
  `socat` and survive version skew on a daemon that can't be restarted.
  - Every request/response pair carries `req`.
  - Events carry none.
- **Data frames (tag `D`)**: `session_id (16 bytes) + raw bytes`, used for
  `Input` and `Output`. This is the only hot path.
- **Handshake.** `Hello{claudio_version, proto, caps, client_colors}`, answered
  by `Hello{…, host:{os, arch, claude_path, claude_version}}`. The probe is
  folded into the handshake.
- **Requests:**
  - `ListSessions`
  - `Spawn{id, cwd, argv, env, size}`. Idempotent by `id`; the reply is
    `Spawned{id, pid}` or `Error`.
  - `Attach{id}` / `Detach{id}`
  - `Resize{id, rows, cols}`
  - `Kill{id}`
  - `ListDir{path}`
  - `ListClaudeSessions{cwd}`
  - `Ping`
- **Daemon-pushed:**
  - `Snapshot{id}` (followed by a `D` frame)
  - `Event{id, SessionEvent}`
  - `Exited{id, status}`
  - `Pong`
- **Backpressure.** Each client has a bounded output queue in the daemon.
  - When the queue overflows, the daemon drops it and sends a fresh snapshot.
  - A slow ssh client can therefore never block the PTY reader.
  - A client that stays wedged is disconnected.
  - `Ping`/`Pong` detects a half-dead bridge. ssh keepalives cover only TCP.
- **Input.** Input that may not have been delivered is never replayed after a
  reconnect.

### 2.2 Session state: hooks over the daemon socket

The daemon spawns claude with merged `--settings` hooks:

```
<abs claudio> __hook <Event> <socket> <session-token>
```

The hook command connects to the **daemon's Unix socket**. The daemon
authenticates it by uid plus a per-spawn token that is bound to a session
generation, and enforces a read deadline and a size cap.

- The manager doesn't reuse the `print/` loopback-TCP relay (it is
  unauthenticated, and its accept loop can block). It doesn't use the
  temp-binary concealment either.
- The hook relay supports these events:
  - `SessionStart`
  - `UserPromptSubmit`
  - `PreToolUse`
  - `Notification`
  - `Stop`
  - `StopFailure`
  - `SessionEnd`
- Unknown events and unknown notification types are logged and ignored.

| Event | State |
|---|---|
| `UserPromptSubmit`, `PreToolUse` | Working |
| `Notification`: `permission_prompt`, `worker_permission_prompt`, `elicitation_dialog` | **NeedsApproval** |
| `Notification`: `agent_needs_input`, `idle_prompt` | **NeedsInput** |
| `Stop` | Idle, *provisionally*: a later `UserPromptSubmit`/`PreToolUse` (e.g. a blocking user Stop hook continues the turn) moves it back to Working. `stop_hook_active` is not treated as a completion flag. |
| `StopFailure` | Error |
| process exit | Exited |
| `SessionStart{source, session_id}` | **journal the new claude session id** (it changes on `/clear` and fork) |
| daemon (re)start / reattach before any hook | Unknown (never assume Idle) |

Captured hook payloads from claude 2.1.280 are versioned test fixtures.

Extra metrics come from the JSONL tail:
- `ai-title`
- `last-prompt`
- model, from the last assistant `message.model`
- ctx %, from the last `usage`
- `cost-state`

**[later]** The statusLine feed, chained to the user's own statusLine command.

### 2.3 Persistence and recovery

There are two journals, each written durably (write-tmp + fsync + rename):

- **Daemon journal**, at `~/.local/state/claudio/sessions.json` on that host.
  For each session it records {id, cwd, argv minus secrets, claude_session_id,
  transcript path, generation, killed}. It is updated the moment
  `SessionStart` reports a new id, so it is correct even while no client is
  connected.
- **Client state**, at `~/.config/claudio/state.json`. It records the intended
  sessions as {id, name, host, cwd, proxy_profile, created_at, order}, plus the
  last known claude_session_id. **Kill intent is persisted before** sending
  `Kill`, so recovery can never resurrect a session the user closed.

Recovery runs on start or reconnect:

1. Ask each daemon for `ListSessions`. A live session is reattached.
2. If the daemon is fresh (after a reboot or crash), it re-spawns every
   non-killed entry in its own journal as `claude --resume <claude_session_id>`.
   - The newest id wins: the daemon journal beats client state.
   - The client re-sends the secret env, because the daemon never stores it.
3. If `--resume` fails ("No conversation found"), claudio spawns a fresh claude
   in the same cwd, keeps the name, and shows a one-line notice.
4. A host that can't be reached is shown as offline and retried with backoff.

### 2.4 SSH bootstrap

1. On the bridge's connection, run `~/.local/bin/claudio __probe`. It is
   claudio-specific and prints `{version, proto, os, arch}`. Note that
   `claudio --version` passes through to claude.
2. If the binary is missing or incompatible, install it:
   - **Selection** is by os + arch, not arch alone. Linux uses static musl;
     macOS uses the darwin assets.
   - **Source.** claudio copies itself when it is the exact same target.
     Otherwise it downloads the release asset and **verifies the sha256 locally**
     before upload.
   - **Upload.** The upload goes to a unique temp file under an install
     `flock`. The staged bytes are re-verified remotely, then `chmod` and
     `mv` move it atomically into `~/.local/bin/claudio`.
3. The remote binary is always invoked by absolute path. claudio never edits
   remote dotfiles.
4. Missing `claude` on the remote is reported from `Hello.host`, with an offer
   to run the official install command.

With the proxy enabled, the remote needs **no claude login**.

### 2.5 claude-proxy integration

#### Configuration
- A proxy profile is {url, token}.
- Sources: `claudio proxy login` (reads the token from a prompt or stdin, and
  stores it in `~/.config/claudio/config.toml`, mode 0600), or the env var
  `CLAUDIO_PROXY_URL=<token>@host`.
- `--proxy <host>` only selects a profile. **Tokens never go in argv.**
- Session state stores the profile *name*, never the token.
- Proxy use can be toggled per session; the tab shows a badge.
- Recommend a dedicated, revocable user token per machine.

#### Env injected at spawn
The env goes through `Spawn.env` and is redacted from all daemon logs:

- `ANTHROPIC_BASE_URL`
- `ANTHROPIC_AUTH_TOKEN`
- `CLAUDE_CODE_USE_GATEWAY=1`
- `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`
- `CLAUDE_CODE_AUTO_COMPACT_WINDOW`
- `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`
- `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`
- `API_TIMEOUT_MS`
- `ANTHROPIC_DEFAULT_{FABLE,OPUS,SONNET,HAIKU}_MODEL`

That is the default, gateway mode, where Claude Code turns its feature flags
off. A first-party session (`first_party = true` on the profile, or
`--first-party` for one run) is this list without `CLAUDE_CODE_USE_GATEWAY` and
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, plus `DISABLE_AUTOUPDATER=1` and
`ENABLE_TOOL_SEARCH=true` (tool search stays on: Claude Code turns it off for a
non-Anthropic base URL otherwise). `env_diff` drops an inherited
`CLAUDE_CODE_USE_GATEWAY` from a first-party env; an inherited
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is kept.

`ANTHROPIC_API_KEY` is unset. Note that `/proc/<pid>/environ` is readable by
the same uid and root.

**[later]** Supply the token through claude's `apiKeyHelper` (`claudio __token`,
which asks the daemon), so it never sits in the environment.

#### New proxy package `internal/claudioapi`
The package claims the whole `/v1/claudio` and `/v1/claudio/` subtree for every
method:
- Unknown paths get a local 404 and wrong methods a 405. Nothing is ever
  forwarded upstream.
- These requests are not written to `request_log`, and they have their own rate
  and query bounds.

Endpoints:
- `GET /v1/claudio` returns {version, capabilities}.
- `GET /v1/claudio/config` returns the recommended env. The `[1m]` default per
  family is derived from the model catalogue. The catalogue logic is extracted
  into a shared function, not reused through the path-forwarding
  `models1m.go`.
- `GET /v1/claudio/models`
- `GET /v1/claudio/me/stats?period=` **requires a non-empty user identity.**
  - Admin and anonymous callers get a 403.
  - Every query is scoped by `user_token_id`.
  - Caches are keyed by identity.
- `GET /v1/claudio/pool/health` returns coarse buckets only (ok / busy /
  saturated per provider), with no raw percentages. A raw percentage would
  reveal one subscription's utilization when a provider has a single account.
  It has a 30 s cache. Routing events are not exposed until per-user filtering
  exists.

claudio does not see the `X-Router-*` response headers, because claude talks to
the proxy directly. Quota signals come from `/me/stats` (`blocked_until`).

### 2.6 Upgrades and version skew

- A new claudio does **not** kill a running daemon. The protocol version is in
  the socket name, so after an incompatible upgrade a new daemon starts beside
  the old one.
- `claudio daemon upgrade` migrates sessions one at a time:
  1. The old daemon ends claude cleanly.
  2. The new daemon re-spawns it with `--resume <id>` in the same cwd.

  Only an in-flight turn is lost.
- Handing PTY file descriptors over with SCM_RIGHTS is deliberately not done.
- JSON control frames tolerate minor skew, so not every version bump is an
  upgrade.

## 3. UX

### 3.1 Layout

```
 1 ● api-refactor ~/r/claudio ⎇main 12m │ 2 ◆ prod-db @vps1 /srv/app 3h │ 3 ? docs 2m │ +
 ┌──────────────────────────────────────────────────────────────────────────────┐
 │                       active claude session (full width)                     │
 └──────────────────────────────────────────────────────────────────────────────┘
 vps1:/srv/app ⎇main · opus-5-5[1m] · ctx 43% · $1.20 │ proxy ok · 1.2M tok today │ menu key
```

- **Top bar:** one tab per session showing index, state glyph, editable name,
  and a short host/path. The active tab is expanded; inactive tabs compress as
  space runs out.
- **State glyphs:** ● working (animated) · ◆ needs approval (red, pulsing) ·
  ? needs input (yellow) · ✓ idle · ✗ exited · ⇄ reconnecting · · unknown.
- **Bottom bar:** details of the active session plus proxy quota.
- **Attention:** when a background session needs you, its tab pulses.
  **[later]** Desktop notification through OSC 9/777 to the outer terminal,
  falling back to `notify-send`/`osascript`.
- **Names:** default to claude's `ai-title`. A user rename is applied with
  claude's `-n/--name` on (re)spawn, so it also shows up in `/resume`. The
  flag was verified in 2.1.280.

### 3.2 New-session wizard

Keyboard-first, fuzzy, with smart defaults. Enter accepts the default at each
step.

1. **Where:** `local` comes first, then the remembered hosts **most recently
   used first** (from `~/.config/claudio/hosts.json`), then every other `Host`
   alias in `~/.ssh/config`. Aliases from `Include`d files count; wildcard
   patterns (`*`, `?`, `!`) are skipped. Typing filters the list fuzzily, and a
   free-form `user@host` is accepted too.
   - The default is the current session's host.
   - claudio needs only the alias. The system `ssh` resolves User, Port,
     ProxyJump and keys from the user's config.
2. **Directory:** a fuzzy list seeded from:
   - the current dir
   - recent dirs
   - dirs that have claude history (the `cwd` fields in `~/.claude/projects`)

   You can also type a path with completion through `ListDir`.
   **[later]** zoxide seeding.
3. **Resume?** If the dir has claude sessions, the list shows `New session`
   plus each existing session with title, last prompt, age and message count.
4. **Options:** collapsed, remembering the last choice. They cover:
   - proxy profile
   - model
   - permission mode
   - **[later]** git worktree, through claude's `-w <name>`

### 3.3 Keys **[open]**

**Hard rule: no collisions with Claude Code's own bindings or with common
terminal and window-manager bindings.**

The keys are configurable in `config.toml`. Every candidate is checked by a P0
**key-capture harness**, which records what the outer terminal actually
delivers and diffs it against claude's binding table, extracted from the
installed bundle.

Ruled out, as verified against claude 2.1.280 and Ghostty's defaults:

| Keys | Reason |
|---|---|
| Shift+arrows | claude: selection extend, message selector, "view" tasks. Ghostty: `adjust_selection` |
| Alt+↑/↓, Ctrl+↑/↓ | claude |
| Alt+←/→ | likely word movement in claude's input |
| Alt+o/p/t/w/v/m/j | claude |
| Alt+b/f/d | readline-style input |
| most Ctrl+letters | claude |
| Shift+Tab | claude |
| Alt+1..9 | Ghostty `goto_tab` (the terminal consumes them) |
| Ctrl+Shift+arrows | Ghostty tabs |
| Ctrl+Alt+arrows | Ghostty splits |
| Super+… | window manager |

Candidates to verify: Alt+Shift+arrows, Alt+punctuation (`, . / ; '`), F-keys,
Alt+a/k/n/s/q.

Planned actions:
- previous/next session
- new session
- overview
- **last-session toggle**
- **cycle through the sessions that need attention**
- command palette
- rename
- send the next key raw
- quit the UI (sessions keep running)

**Close is never a single keystroke.** It goes through the palette, with a
*detach / kill / cancel* confirm.

Mouse: click a tab to switch.

### 3.4 Later

- **Overview / "mission control":** every session's last assistant message
  (from the `Stop` hook's `last_assistant_message`), its state, ctx % and cost.
  Enter jumps to that session.
- **Scripting CLI:**
  - `claudio ls`
  - `claudio new --host vps1 --dir /srv/app [--resume]`
  - `claudio attach <name>`
  - `claudio hosts`
- Auto-respawn with `--resume` when claude crashes.
- Scrollback / copy mode in the mirror.
- Git worktrees with `.worktreeinclude` carry-over.
- Desktop notifications.

## 4. Code structure (target)

```
src/
  main.rs        mode dispatch: manager | -p | --api | --daemon | --slave | __hook | __probe | passthrough
  print/         existing -p path: driver, emit, session, legacy tcp/file hook relay
  api/           existing --api server
  term/          shared: probe responder (today's vt.rs), key → bytes encoding
  proto.rs       frames + messages; serde-only deps (no tokio/ratatui) so daemon & client share it
  daemon/        listener, session registry, session actor (pty + vt + subscribers), hook ingest, journal, fs RPCs
  bridge.rs      --slave stdio⇄socket bridge + daemon autostart
  client/        connection manager: local + ssh transports, bootstrap/upload, reconnect
  claude/        claude knowledge: projects-dir scan, session listing, settings/hook injection, JSONL tail
  proxy/         claude-proxy HTTP client + profiles
  tui/           app state + event loop; widgets: tabbar, statusbar, term pane, wizard, palette, overview
  config.rs      ~/.config/claudio/{config.toml,state.json}
```

**Concurrency:**
- The daemon and the client use tokio. The sync `-p` path stays untouched under
  `print/`.
- `portable-pty` reads are blocking, so each session gets one dedicated reader
  thread feeding a tokio mpsc. Each session is an **actor task** that alone
  owns the PTY writer, the VT engine and the subscriber list: a single owner,
  with no locks.
- In the TUI, crossterm input runs on its own thread feeding a channel.

**Libraries:**
- TUI: **ratatui 0.30 + crossterm 0.29**.
- **VT engine [spike]:** `alacritty_terminal` 0.26 (most correct, used by Zed,
  unstable API, no serialization) vs `vt100` 0.16 (simpler; `state_formatted()`
  produces snapshots). The spike replays recorded claude PTY streams through
  both, comparing:
  - fidelity: 2026 sync, wide chars and emoji, alt screen, mode exposure
  - performance
  - snapshot effort

  Whichever wins sits behind a small `Screen` trait.
- Kitty keyboard protocol toward the child: phase 1 doesn't advertise it, so
  keys use the legacy encoding.

## 5. Phases

| Phase | Scope | Exit criterion |
|---|---|---|
| P0 spike | Engine comparison on recorded streams; an in-process prototype (no daemon) rendering live local claude sessions in a ratatui pane with the tab bar; key routing; the key-capture harness | Claude Code usable inside claudio with no glitches; engine and keys chosen |
| P1 local | Daemon + protocol, local sessions, hook state, the wizard (local dirs + resume picker), both journals + recovery, rename, close/detach, `__probe` | Kill the TUI or the daemon and everything comes back; a reboot falls back to `--resume` |
| P2 ssh | Hosts from history + `~/.ssh/config`, bootstrap/upload, bridge, systemd-run detach, reconnect/backoff, remote dir browsing | Pull the network, log out remotely; on reconnect the session is intact |
| P3 proxy | `claudio proxy login`/profiles + client; `/v1/claudio/*` in claude-proxy (Go) | Gateway mode with 1M defaults; stats in the bottom bar |
| P4 polish | Overview, attention cycling, notifications, statusLine feed, worktrees, scrollback/copy, mouse, scripting CLI, `daemon upgrade` | — |

### Test plan highlights

Fault tests:
- a lost `Spawn` reply (idempotency)
- kill vs recovery
- `/clear`, then a reboot (the resume id must be the new one)
- concurrent daemon start and concurrent uploads
- incompatible daemon versions
- oversized frames
- slow readers (overflow → resnapshot)
- snapshot/output interleaving
- wrong-uid socket peers and symlinked runtime dirs
- forged hook connections

On the proxy, run with two users and with disabled, anonymous and admin
identities, an empty admin token config, unknown `/v1/claudio` paths and
methods, and an exhausted quota. Assert no cross-user data and no upstream
calls.
