//! Transparent, *dynamic* `claude` argument handling.
//!
//! This binary is a drop-in for `claude`. It deliberately does **not** mirror
//! claude's flag grammar — that would drift every release. Instead it
//! understands only the handful of flags it must own, and forwards everything
//! else (including the prompt, and any flag claude may add in the future)
//! verbatim to the real `claude`.
//!
//! Two facts make this possible:
//!   1. Without `-p`/`--print`, `main` execs the real claude unchanged, so
//!      interactive use, subcommands, `--help`, and `--version` are 100% native.
//!   2. Interactive `claude` auto-runs a positional prompt to completion. So in
//!      print mode we forward the prompt as-is and let claude parse and run it —
//!      we never type it and never need to know any other flag's arity.
//!
//! Flags this wrapper owns:
//!   -p/--print            mode switch (consumed; not forwarded)
//!   --api                 run the OpenAI-compatible API server (consumed)
//!   --fast                strip human-like delays for lowest latency (consumed)
//!   --log-messages        log the CLI⇄claudio⇄claude message flow (consumed)
//!   --log-messages-file   append the raw message flow as JSONL (consumed)
//!   --output-format       we format output ourselves (consumed)
//!   --input-format        only `text` supported by this backend (consumed)
//!   --settings            captured; merged with our hooks; re-injected
//!   --session-id          captured (to locate the transcript) and forwarded
//!
//! Flags that would break the backend, so they are dropped with a warning:
//!   --bare                     skips hooks (we need the Stop hook)
//!   --no-session-persistence   suppresses the session JSONL (our source of truth)
//!
//! Everything else is forwarded untouched. Wrapper-only knobs live in
//! `CLAUDIO_*` env vars so the CLI surface stays byte-for-byte claude's.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

impl OutputFormat {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(OutputFormat::Text),
            "json" => Some(OutputFormat::Json),
            "stream-json" => Some(OutputFormat::StreamJson),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookTransport {
    Tcp,
    File,
}

#[derive(Debug)]
pub struct Parsed {
    pub print_mode: bool,
    /// `--api`: run the OpenAI-compatible API server instead of a single turn.
    /// Mutually exclusive with the `-p` print path; takes precedence in `main`.
    pub api_mode: bool,
    /// `--fast`: strip the human-like delays (typing cadence, Ink-quiescence
    /// wait, pre-Enter dwell) for lowest latency. OR-ed with `CLAUDIO_FAST`.
    pub fast: bool,
    /// `--log-messages`: log the full CLI⇄claudio⇄claude message flow to stderr.
    /// OR-ed with `CLAUDIO_LOG_MESSAGES`.
    pub log_messages: bool,
    /// `--log-messages-file <path>`: append the raw message flow as JSONL to a
    /// file. Falls back to `CLAUDIO_LOG_MESSAGES_FILE`.
    pub log_messages_file: Option<String>,
    pub output_format: OutputFormat,
    pub session_id: Option<String>,
    pub user_settings: Option<String>,
    /// Flags to forward verbatim to the interactive `claude` child (our owned
    /// flags removed; the prompt removed — we type it ourselves; every other
    /// flag preserved in order, including ones we've never heard of).
    pub forward: Vec<String>,
    /// The prompt to type into the TUI. Extracted here for the common form;
    /// `main` overrides it with stdin when input is piped.
    pub prompt: Option<String>,
    pub warnings: Vec<String>,
}

/// A *minimal* set of claude flags known to take a value. This is used for one
/// purpose only: locating the trailing positional prompt so we can type it
/// rather than mis-read a flag's value as the prompt. It is NOT a model of
/// claude's grammar — unknown flags are still forwarded verbatim, and `--`
/// always delimits the prompt unambiguously (recommended for scripts).
fn known_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--model"
            | "--agent"
            | "--agents"
            | "--append-system-prompt"
            | "--system-prompt"
            | "--append-system-prompt-file"
            | "--system-prompt-file"
            | "--debug-file"
            | "--effort"
            | "--fallback-model"
            | "--json-schema"
            | "--max-budget-usd"
            | "-n"
            | "--name"
            | "--permission-mode"
            | "--plugin-dir"
            | "--plugin-url"
            | "--setting-sources"
            | "--remote-control-session-name-prefix"
    )
}

fn split_eq(tok: &str) -> (&str, Option<&str>) {
    match tok.split_once('=') {
        Some((f, v)) => (f, Some(v)),
        None => (tok, None),
    }
}

/// Parse argv with program name already stripped.
pub fn parse(args: &[String]) -> Parsed {
    let mut p = Parsed {
        print_mode: false,
        api_mode: false,
        fast: false,
        log_messages: false,
        log_messages_file: None,
        output_format: OutputFormat::Text,
        session_id: None,
        user_settings: None,
        forward: Vec::new(),
        prompt: None,
        warnings: Vec::new(),
    };

    let mut i = 0;
    while i < args.len() {
        let tok = &args[i];

        // End of options: everything after "--" is the prompt (unambiguous
        // form, recommended for scripts). We type it; it is not forwarded.
        if tok == "--" {
            let rest: Vec<String> = args[i + 1..].to_vec();
            if !rest.is_empty() {
                p.prompt = Some(rest.join(" "));
            }
            break;
        }

        let (flag, inline) = split_eq(tok);

        // Helper: fetch this owned flag's value (inline `=v` or next token) and
        // return how many argv tokens it consumed.
        let take_value = |args: &[String], i: usize| -> (Option<String>, usize) {
            if let Some(v) = inline {
                (Some(v.to_string()), 1)
            } else if i + 1 < args.len() {
                (Some(args[i + 1].clone()), 2)
            } else {
                (None, 1)
            }
        };

        match flag {
            "-p" | "--print" => {
                p.print_mode = true;
                i += 1;
            }
            "--api" => {
                // Owned switch: start the OpenAI-compatible API server. All
                // server configuration lives in CLAUDIO_API_* / OPENAI_PROXY_*
                // env vars so the CLI surface stays claude's.
                p.api_mode = true;
                i += 1;
            }
            "--fast" => {
                // Owned switch: disable all human-like delays for lowest latency.
                p.fast = true;
                i += 1;
            }
            "--log-messages" => {
                // Owned switch: log the CLI⇄claudio⇄claude message flow to stderr.
                p.log_messages = true;
                i += 1;
            }
            "--log-messages-file" => {
                let (v, n) = take_value(args, i);
                p.log_messages_file = v;
                i += n;
            }
            "--output-format" => {
                let (v, n) = take_value(args, i);
                match v.as_deref().and_then(OutputFormat::parse) {
                    Some(f) => p.output_format = f,
                    None => {
                        if let Some(v) = v {
                            p.warnings
                                .push(format!("unknown --output-format '{v}', using text"));
                        }
                    }
                }
                i += n;
            }
            "--input-format" => {
                let (v, n) = take_value(args, i);
                if let Some(v) = v {
                    if v != "text" {
                        p.warnings.push(format!(
                            "--input-format {v} is not supported by the interactive backend; using text"
                        ));
                    }
                }
                i += n;
            }
            "--settings" => {
                let (v, n) = take_value(args, i);
                p.user_settings = v;
                i += n; // not forwarded; driver injects merged settings
            }
            "--session-id" => {
                let (v, n) = take_value(args, i);
                p.session_id = v.clone();
                // capture AND forward so claude uses the same id
                if inline.is_some() {
                    p.forward.push(tok.clone());
                } else {
                    p.forward.push("--session-id".to_string());
                    if let Some(v) = v {
                        p.forward.push(v);
                    }
                }
                i += n;
            }
            "--bare" | "--no-session-persistence" => {
                p.warnings.push(format!(
                    "{flag} is incompatible with the interactive backend (it disables the hooks/transcript this wrapper relies on); dropping it"
                ));
                i += 1;
            }
            _ => {
                // Unknown to us → forward verbatim. We do not consume following
                // tokens: claude's own parser handles their arity.
                p.forward.push(tok.clone());
                i += 1;
            }
        }
    }

    // If the prompt wasn't given via `--`, extract the trailing positional from
    // the forwarded tokens (so we type it rather than letting claude run it).
    if p.print_mode && p.prompt.is_none() {
        if let Some(idx) = last_positional_index(&p.forward) {
            p.prompt = Some(p.forward.remove(idx));
        }
    }

    p
}

/// Index of the last bare positional in a forwarded-args list, skipping the
/// values of the minimal known value-flags. Best-effort: `--` (handled above)
/// is the unambiguous escape hatch for anything this heuristic can't resolve.
fn last_positional_index(forward: &[String]) -> Option<usize> {
    let mut i = 0;
    let mut last = None;
    while i < forward.len() {
        let tok = &forward[i];
        let (flag, inline) = split_eq(tok);
        if tok.starts_with('-') && tok != "-" {
            if known_value_flag(flag) && inline.is_none() {
                i += 2; // skip the flag's value
            } else {
                i += 1;
            }
        } else {
            last = Some(i);
            i += 1;
        }
    }
    last
}

/// Wrapper-only knobs, sourced from the environment so the CLI surface stays
/// identical to claude's.
pub struct WrapperEnv {
    pub debug: bool,
    pub timeout_sec: u64,
    pub hook_transport: HookTransport,
    pub raw_log: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub claude_path: String,
    /// Strip human-like delays (cadence, quiescence wait, dwell) for low latency.
    pub fast: bool,
}

impl WrapperEnv {
    pub fn from_env() -> Self {
        let g = |k: &str| std::env::var(k).ok();
        WrapperEnv {
            debug: g("CLAUDIO_DEBUG")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false),
            timeout_sec: g("CLAUDIO_TIMEOUT_SEC")
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            hook_transport: match g("CLAUDIO_HOOK_TRANSPORT").as_deref() {
                Some("file") => HookTransport::File,
                _ => HookTransport::Tcp,
            },
            raw_log: g("CLAUDIO_RAW_LOG"),
            cols: g("CLAUDIO_COLS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(120),
            rows: g("CLAUDIO_ROWS").and_then(|v| v.parse().ok()).unwrap_or(40),
            claude_path: g("CLAUDIO_CLAUDE_PATH").unwrap_or_else(|| "claude".into()),
            fast: g("CLAUDIO_FAST")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false),
        }
    }
}

/// Appended to `claude --help` output so `claudio --help` documents the
/// wrapper-only surface. Everything not listed here is forwarded to `claude`.
pub const HELP_APPENDIX: &str = "\n\
\x20Claudio custom flags\n\
\x20────────────────────\n\
\x20This binary is `claudio`, a drop-in `claude` wrapper. Without -p/--print it\n\
\x20execs the real `claude` unchanged; the flags and env vars below are its own.\n\
\n\
\x20Flags (consumed by claudio, not forwarded to claude):\n\
\x20  -p, --print                 Emulate print mode via the interactive PTY (claudio never runs `claude -p`).\n\
\x20  --api                       Run the OpenAI-compatible API server (POST /v1/chat/completions, /v1/models).\n\
\x20  [--proxy NAME|--no-proxy] [--first-party]\n\
\x20                              Open the session manager. --proxy NAME pre-selects a proxy profile for new\n\
\x20                              sessions, --no-proxy starts them without one. --first-party makes every\n\
\x20                              session of this run first-party (Monitor, /advisor, WebSearch), whatever\n\
\x20                              the profile says; a session keeps it across restarts. Gateway mode is the\n\
\x20                              default. See the README (proxy section).\n\
\x20  --plain [--proxy NAME|--no-proxy] [--first-party] [claude args…]\n\
\x20                              Run one plain interactive claude, full screen, with the proxy env of a\n\
\x20                              manager session: no tabs, status bar or wizard. Alt+h (help), Alt+s\n\
\x20                              (proxy stats), Alt+l (git history) and Alt+e (restart) are claudio's;\n\
\x20                              every other key goes to claude. Exits with claude; the session is\n\
\x20                              not kept. Must be the first argument; -p/--api are rejected.\n\
\x20  --fast                      Strip human-like typing/quiescence delays for lowest latency.\n\
\x20  --log-messages              Log the full CLI⇄claudio⇄claude message flow to stderr (colorized).\n\
\x20  --log-messages-file <path>  Also append the raw message flow to <path> as JSON Lines (untruncated).\n\
\n\
\x20Environment variables — general:\n\
\x20  CLAUDIO_CLAUDE_PATH=<path>        Path to the real `claude` binary (default: claude).\n\
\x20  CLAUDIO_FAST=1                    Same as --fast.\n\
\x20  CLAUDIO_LOG_MESSAGES=1            Same as --log-messages.\n\
\x20  CLAUDIO_LOG_MESSAGES_FILE=<path>  Same as --log-messages-file.\n\
\x20  CLAUDIO_TIMEOUT_SEC=<n>           Per-turn PTY backend timeout, seconds (default: 300).\n\
\x20  CLAUDIO_COLS=<n> / CLAUDIO_ROWS=<n>  Emulated terminal size (default: 120x40).\n\
\x20  CLAUDIO_DEBUG=1                   Verbose wrapper diagnostics.\n\
\x20  CLAUDIO_RAW_LOG=<path>            Dump the raw PTY byte stream to a file.\n\
\n\
\x20Environment variables — API server (--api):\n\
\x20  CLAUDIO_API_BIND=<host:port>      Listen address (default: 127.0.0.1:8080).\n\
\x20  CLAUDIO_API_KEY=<key>             Require `Authorization: Bearer <key>` on requests.\n\
\x20  CLAUDIO_API_DEFAULT_MODEL=<m>     Model when the request omits one / isn't a Claude model (default: sonnet).\n\
\x20  CLAUDIO_API_CWD=<dir>             Backend working directory (default: system temp dir).\n\
\x20  CLAUDIO_API_MAX_CONCURRENCY=<n>   Max concurrent backend turns (default: 8).\n\
\x20  CLAUDIO_API_TIMEOUT_SECS=<n>      Per-turn timeout, seconds (default: 600).\n\
\x20  CLAUDIO_API_AGENTIC=false         Ignore client tools[] (chat-only; default: on).\n\
\x20  CLAUDIO_API_SETTING_SOURCES=<v>   Value for --setting-sources (default: project; empty = omit).\n\
\n\
\x20Environment variables — session pool (--api):\n\
\x20  CLAUDIO_API_MAX_SESSIONS=<n>      Max conversation mappings kept (default: 32).\n\
\x20  CLAUDIO_API_MAX_LIVE=<n>          Max live claude processes; idle ones are demoted (default: 6).\n\
\x20  CLAUDIO_API_SESSION_TTL=<n>       Drop an idle conversation mapping after n seconds (default: 600).\n\
\x20  CLAUDIO_API_REINJECT_TURNS=<n>    Re-inject the system prompt every n turns (default: 6).\n\
\n\
\x20Daemon commands:\n\
\x20  claudio daemon status             Show whether the daemon is running, its version and session count.\n\
\x20  claudio daemon stop               Send SIGTERM to the daemon; running sessions go dormant.\n\
\x20  claudio daemon restart            Stop then start the daemon (use after installing a new binary).\n\
\x20  claudio sessions                  List sessions as a table (id, state, name, cwd) for scripting.\n\
\n\
\x20Upgrade commands:\n\
\x20  claudio upgrade                   Download and install the latest release, then restart the daemon.\n\
\x20  claudio upgrade --check           Check for a newer release without installing it.\n\
\x20\n\
\x20  When a newer version is available, a quiet `↑ vX.Y.Z` indicator appears at the\n\
\x20  right of the TUI status bar. It is also mentioned in the help popup (Alt+h).\n\
\x20  The check is anonymous, at most once per 24 h, and skipped when\n\
\x20  CLAUDIO_NO_UPDATE_CHECK=1 or `[update] check = false` is set in config.toml.\n\
\n\
\x20Proxy commands (optional claude-proxy integration):\n\
\x20  claudio proxy login [URL]         Add or update a proxy profile (prompts for token).\n\
\x20  claudio proxy status              Show profiles, live stats and pool health.\n\
\x20  claudio proxy logout [NAME]       Remove a proxy profile.\n\
\x20  claudio proxy use NAME|none       Set (or clear) the default proxy profile.\n\
\n\
\x20Environment variables — proxy:\n\
\x20  CLAUDIO_PROXY_URL=<token>@<host>  Ephemeral proxy profile for this run.\n\
\x20                                    host may be: claude.example.net,\n\
\x20                                    https://claude.example.net, or host:port.\n\
\x20                                    http:// is only allowed for 127.0.0.1/localhost.\n\
\x20                                    When set, overrides any saved default profile.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn print_prompt_only() {
        let p = parse(&s(&["-p", "hello world"]));
        assert!(p.print_mode);
        assert_eq!(p.prompt.as_deref(), Some("hello world"));
        assert!(p.forward.is_empty());
        assert_eq!(p.output_format, OutputFormat::Text);
    }

    #[test]
    fn output_format_separate_and_inline() {
        let p = parse(&s(&["-p", "--output-format", "json", "go"]));
        assert_eq!(p.output_format, OutputFormat::Json);
        assert_eq!(p.prompt.as_deref(), Some("go"));
        assert!(p.forward.is_empty());
        let p = parse(&s(&["-p", "--output-format=stream-json", "go"]));
        assert_eq!(p.output_format, OutputFormat::StreamJson);
        assert_eq!(p.prompt.as_deref(), Some("go"));
    }

    #[test]
    fn value_flag_not_mistaken_for_prompt() {
        // --model takes a value; "opus" must not be read as the prompt.
        let p = parse(&s(&["-p", "--model", "opus", "go"]));
        assert_eq!(p.prompt.as_deref(), Some("go"));
        assert_eq!(p.forward, s(&["--model", "opus"]));
    }

    #[test]
    fn variadic_then_prompt() {
        let p = parse(&s(&["-p", "--allowedTools", "Bash", "Edit", "go"]));
        assert_eq!(p.prompt.as_deref(), Some("go"));
        assert_eq!(p.forward, s(&["--allowedTools", "Bash", "Edit"]));
    }

    #[test]
    fn dangerously_skip_forwarded() {
        let p = parse(&s(&["-p", "--dangerously-skip-permissions", "go"]));
        assert!(p
            .forward
            .contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(p.prompt.as_deref(), Some("go"));
    }

    #[test]
    fn session_id_captured_and_forwarded() {
        let p = parse(&s(&["-p", "--session-id", "abc", "go"]));
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.prompt.as_deref(), Some("go"));
        assert_eq!(p.forward, s(&["--session-id", "abc"]));
    }

    #[test]
    fn settings_captured_not_forwarded() {
        let p = parse(&s(&["-p", "--settings", "{\"x\":1}", "go"]));
        assert_eq!(p.user_settings.as_deref(), Some("{\"x\":1}"));
        assert!(!p.forward.iter().any(|a| a == "--settings"));
        assert_eq!(p.prompt.as_deref(), Some("go"));
    }

    #[test]
    fn breakers_dropped_with_warning() {
        let p = parse(&s(&["-p", "--bare", "--no-session-persistence", "go"]));
        assert_eq!(p.prompt.as_deref(), Some("go"));
        assert!(p.forward.is_empty());
        assert_eq!(p.warnings.len(), 2);
    }

    #[test]
    fn no_print_when_absent() {
        let p = parse(&s(&["--model", "opus", "hi"]));
        assert!(!p.print_mode);
        assert!(p.prompt.is_none());
    }

    #[test]
    fn double_dash_is_the_prompt() {
        let p = parse(&s(&["-p", "--model", "opus", "--", "--looks-like-flag"]));
        assert_eq!(p.prompt.as_deref(), Some("--looks-like-flag"));
        assert_eq!(p.forward, s(&["--model", "opus"]));
    }

    #[test]
    fn print_long_form() {
        let p = parse(&s(&["--print", "hello"]));
        assert!(p.print_mode);
        assert_eq!(p.prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn session_id_inline_eq_form() {
        let p = parse(&s(&["-p", "--session-id=my-uuid", "go"]));
        assert_eq!(p.session_id.as_deref(), Some("my-uuid"));
        // inline = form is forwarded as a single token
        assert!(p.forward.iter().any(|a| a.contains("my-uuid")));
        assert_eq!(p.prompt.as_deref(), Some("go"));
    }

    #[test]
    fn unknown_future_flag_both_tokens_forwarded() {
        // A flag we've never heard of: both the flag and the following token
        // are forwarded (each as a separate i+=1 pass). The last positional
        // ("go") is extracted as the prompt.
        let p = parse(&s(&["-p", "--future-flag", "somevalue", "go"]));
        assert_eq!(p.forward, s(&["--future-flag", "somevalue"]));
        assert_eq!(p.prompt.as_deref(), Some("go"));
    }

    #[test]
    fn multi_word_positional_joined_as_prompt() {
        let p = parse(&s(&["-p", "word1", "word2", "word3"]));
        // last positional is the prompt ("word3"); earlier positionals
        // are also forwarded (last_positional_index takes the last one)
        assert_eq!(p.prompt.as_deref(), Some("word3"));
    }

    #[test]
    fn double_dash_multi_word_prompt() {
        let p = parse(&s(&["-p", "--model", "opus", "--", "hello", "world"]));
        assert_eq!(p.prompt.as_deref(), Some("hello world"));
        assert_eq!(p.forward, s(&["--model", "opus"]));
    }

    #[test]
    fn output_format_unknown_warns_defaults_text() {
        let p = parse(&s(&["-p", "--output-format", "ndjson", "go"]));
        assert_eq!(p.output_format, OutputFormat::Text);
        assert!(p.warnings.iter().any(|w| w.contains("ndjson")));
    }

    #[test]
    fn input_format_non_text_warns() {
        let p = parse(&s(&["-p", "--input-format", "stream-json", "go"]));
        assert!(p.warnings.iter().any(|w| w.contains("stream-json")));
    }

    #[test]
    fn no_print_mode_owned_flags_still_consumed() {
        // The parser owns --output-format regardless of -p (it affects our
        // output formatting if -p is ever added to the same invocation).
        // In non-print mode main uses the ORIGINAL argv, not parsed.forward,
        // so the consumption doesn't affect the transparent passthrough path.
        let p = parse(&s(&["--model", "opus", "--output-format", "json", "hi"]));
        assert!(!p.print_mode);
        assert_eq!(p.output_format, OutputFormat::Json);
        // --model and its value forwarded, --output-format consumed, hi forwarded
        assert_eq!(p.forward, s(&["--model", "opus", "hi"]));
    }

    #[test]
    fn empty_args() {
        let p = parse(&[]);
        assert!(!p.print_mode);
        assert!(!p.api_mode);
        assert!(p.prompt.is_none());
        assert!(p.forward.is_empty());
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn api_flag_sets_api_mode_and_is_not_forwarded() {
        let p = parse(&s(&["--api"]));
        assert!(p.api_mode);
        assert!(!p.print_mode);
        assert!(p.forward.is_empty());
    }

    #[test]
    fn api_flag_does_not_consume_following_tokens() {
        // --api is a bare switch; nothing after it is treated as its value.
        let p = parse(&s(&["--api", "--model", "opus"]));
        assert!(p.api_mode);
        assert_eq!(p.forward, s(&["--model", "opus"]));
    }

    #[test]
    fn log_messages_flag_consumed() {
        let p = parse(&s(&["--api", "--log-messages"]));
        assert!(p.log_messages);
        assert!(p.log_messages_file.is_none());
        assert!(p.forward.is_empty());
    }

    #[test]
    fn log_messages_file_takes_value_separate_and_inline() {
        let p = parse(&s(&["--api", "--log-messages-file", "/tmp/flow.jsonl"]));
        assert_eq!(p.log_messages_file.as_deref(), Some("/tmp/flow.jsonl"));
        assert!(p.forward.is_empty());
        let p = parse(&s(&["--api", "--log-messages-file=/tmp/f.jsonl"]));
        assert_eq!(p.log_messages_file.as_deref(), Some("/tmp/f.jsonl"));
    }

    #[test]
    fn log_messages_file_value_not_mistaken_for_prompt() {
        let p = parse(&s(&["-p", "--log-messages-file", "/tmp/f.jsonl", "do it"]));
        assert_eq!(p.log_messages_file.as_deref(), Some("/tmp/f.jsonl"));
        assert_eq!(p.prompt.as_deref(), Some("do it"));
        assert!(p.forward.is_empty());
    }

    #[test]
    fn help_appendix_lists_flags_and_envs() {
        assert!(HELP_APPENDIX.contains("Claudio custom flags"));
        assert!(HELP_APPENDIX.contains("--log-messages"));
        assert!(HELP_APPENDIX.contains("--api"));
        assert!(HELP_APPENDIX.contains("CLAUDIO_API_BIND"));
        assert!(HELP_APPENDIX.contains("CLAUDIO_LOG_MESSAGES_FILE"));
    }
}
