//! CLI subcommands: `claudio proxy login/status/logout/use`.
//!
//! These commands run synchronously (blocking tokio runtime) before the rest
//! of the TUI starts. They are dispatched in `main.rs` before `cli::parse`.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use super::api;
use super::profile::{self, Profile};

// ── Dispatch ──────────────────────────────────────────────────────────────────

/// Handle `claudio proxy <args>`. Returns `Some(code)` when the subcommand
/// was handled, `None` when `args` is not a proxy subcommand.
pub fn dispatch(args: &[String]) -> Option<ExitCode> {
    // args[0] must be "proxy".
    if args.first().map(String::as_str) != Some("proxy") {
        return None;
    }
    let sub = args.get(1).map(String::as_str).unwrap_or("--help");
    let rest = if args.len() > 2 { &args[2..] } else { &[] };

    let code = match sub {
        "login" => cmd_login(rest),
        "status" => cmd_status(),
        "logout" => cmd_logout(rest),
        "use" => cmd_use(rest),
        "-h" | "--help" | "help" => {
            print!("{}", PROXY_HELP);
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("claudio proxy: unknown subcommand '{other}'");
            eprintln!("{PROXY_HELP}");
            ExitCode::FAILURE
        }
    };
    Some(code)
}

const PROXY_HELP: &str = "\
claudio proxy — manage claude-proxy profiles

USAGE
  claudio proxy login [URL]         Add or update a proxy profile
  claudio proxy status              Show profiles and live stats
  claudio proxy logout [NAME]       Remove a proxy profile
  claudio proxy use NAME|none       Set the default profile

OPTIONS (login)
  --name NAME    Profile name (default: first DNS label of the host)
  --unverified   Save the profile even if token verification fails
";

// ── login ─────────────────────────────────────────────────────────────────────

fn cmd_login(args: &[String]) -> ExitCode {
    // Parse flags.
    let mut name_override: Option<String> = None;
    let mut url_arg: Option<String> = None;
    let mut unverified = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                name_override = args.get(i).cloned();
            }
            "--unverified" => {
                unverified = true;
            }
            a if !a.starts_with('-') => {
                url_arg = Some(a.to_owned());
            }
            other => {
                eprintln!("claudio proxy login: unknown flag '{other}'");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }

    // Get URL.
    let url_raw = match url_arg {
        Some(u) => u,
        None => {
            eprint!("Proxy URL (e.g. https://claude.example.net): ");
            let _ = io::stderr().flush();
            let mut line = String::new();
            if io::stdin().lock().read_line(&mut line).is_err() || line.trim().is_empty() {
                eprintln!("claudio proxy login: no URL provided");
                return ExitCode::FAILURE;
            }
            line.trim().to_owned()
        }
    };

    // Reject TOKEN@HOST format: tokens are never accepted as arguments.
    // Tokens must come from the interactive prompt, stdin pipe, or CLAUDIO_PROXY_URL.
    if url_raw.contains('@') {
        eprintln!("claudio proxy login: tokens are never accepted as arguments;");
        eprintln!("  use the interactive prompt, pipe it on stdin, or set CLAUDIO_PROXY_URL");
        return ExitCode::FAILURE;
    }

    // Normalize host string to a proper URL.
    let url = match profile::normalize_url(&url_raw) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("claudio proxy login: {e}");
            return ExitCode::FAILURE;
        }
    };

    let token = read_token_from_tty_or_stdin();
    if token.is_empty() {
        eprintln!("claudio proxy login: no token provided");
        return ExitCode::FAILURE;
    }

    let name = name_override.unwrap_or_else(|| profile::name_from_url(&url));

    // Validate with the proxy.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let verified = rt.block_on(async {
        match api::check_root(&url, &token).await {
            Ok(true) => {
                println!("✓  Proxy responded at {url}");
                true
            }
            Ok(false) => {
                // Endpoint doesn't exist — try /v1/models to at least check auth.
                eprintln!("notice: /v1/claudio not found; proxy may not support the Claudio API");
                eprintln!("        Checking auth with /v1/models …");
                match api::check_models_fallback(&url, &token).await {
                    Ok(()) => {
                        println!("✓  Token accepted at {url} (no Claudio API, stats unavailable)");
                        true
                    }
                    Err(e) => {
                        eprintln!("claudio proxy login: {e}");
                        false
                    }
                }
            }
            Err(e) => {
                eprintln!("claudio proxy login: could not reach {url}: {e}");
                false
            }
        }
    });

    if !verified {
        if unverified {
            eprintln!("warning: token NOT verified — saving profile as unverified");
        } else {
            return ExitCode::FAILURE;
        }
    }

    // Save.
    let mut sec = match profile::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("claudio proxy login: could not read config.toml: {e}");
            return ExitCode::FAILURE;
        }
    };
    let is_first = sec.profiles.is_empty();
    store_login(&mut sec, &name, url.clone(), token);
    if is_first || sec.default.is_none() {
        sec.default = Some(name.clone());
        println!("  Set '{name}' as default proxy.");
    }
    if let Err(e) = profile::save(&sec) {
        eprintln!("claudio proxy login: could not save config.toml: {e}");
        return ExitCode::FAILURE;
    }
    println!("  Saved profile '{name}'.");
    ExitCode::SUCCESS
}

/// Store a logged-in profile, replacing any profile of that name. A re-login
/// keeps the profile's `first_party` mode: only the URL and token change.
fn store_login(sec: &mut profile::ProxySection, name: &str, url: String, token: String) {
    let first_party = sec.profiles.get(name).is_some_and(|p| p.first_party);
    sec.profiles.insert(
        name.to_owned(),
        Profile {
            url,
            token,
            first_party,
        },
    );
}

/// Read a token with no-echo (raw TTY or plain stdin).
fn read_token_from_tty_or_stdin() -> String {
    use std::io::IsTerminal;

    eprint!("Token: ");
    let _ = io::stderr().flush();

    if io::stdin().is_terminal() {
        // Use libc termios to suppress echo.
        read_token_no_echo()
    } else {
        let mut line = String::new();
        let _ = io::stdin().lock().read_line(&mut line);
        eprintln!(); // mimic the newline the terminal would print.
        line.trim().to_owned()
    }
}

// ── No-echo token reader ───────────────────────────────────────────────────────
//
// Uses libc termios to disable echo. Portable across Linux and macOS via
// MaybeUninit + tcgetattr (no struct literal with platform-specific fields).
// An RAII guard restores the terminal on every exit path. A temporary SIGINT
// handler restores the terminal before the process is terminated by Ctrl-C.

use std::sync::atomic::{AtomicI32, Ordering};

/// File descriptor saved for the SIGINT handler. -1 = handler not installed.
static SIGINT_FD: AtomicI32 = AtomicI32::new(-1);

/// Saved termios bytes for the SIGINT handler.
struct TermCell(std::cell::UnsafeCell<std::mem::MaybeUninit<libc::termios>>);
// SAFETY: written by the main thread before the handler is installed (Release
// store on SIGINT_FD), read only by the handler (Acquire load). No concurrent
// writes after initialization.
unsafe impl Sync for TermCell {}
static SIGINT_SAVED: TermCell =
    TermCell(std::cell::UnsafeCell::new(std::mem::MaybeUninit::uninit()));

/// SIGINT handler: restore terminal and re-raise with the default handler.
extern "C" fn on_sigint(_: libc::c_int) {
    let fd = SIGINT_FD.load(Ordering::Acquire);
    if fd >= 0 {
        // SAFETY: initialized before SIGINT_FD was set to a non-negative value.
        unsafe {
            let saved = (*SIGINT_SAVED.0.get()).assume_init_ref();
            libc::tcsetattr(fd, libc::TCSANOW, saved);
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

/// RAII guard: restores termios and the previous SIGINT handler on drop.
struct NoEchoGuard {
    fd: i32,
    saved: libc::termios,
    old_sigint: libc::sighandler_t,
}

impl Drop for NoEchoGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
            libc::signal(libc::SIGINT, self.old_sigint);
        }
        SIGINT_FD.store(-1, Ordering::Release);
    }
}

/// Read a line from stdin with echo disabled using libc termios.
///
/// Aborts the process (via `eprintln!` + `process::exit`) if echo cannot be
/// disabled — we never fall back to echoing a secret token.
fn read_token_no_echo() -> String {
    use std::mem::MaybeUninit;
    use std::os::unix::io::AsRawFd;

    let stdin_fd = io::stdin().as_raw_fd();

    // Fetch the current termios using MaybeUninit — avoids naming any
    // platform-specific struct fields (c_line exists on Linux but not macOS).
    let mut saved = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr writes into the pointer on success.
    if unsafe { libc::tcgetattr(stdin_fd, saved.as_mut_ptr()) } != 0 {
        eprintln!("\nclaudio proxy login: cannot query terminal state; aborting");
        std::process::exit(1);
    }
    // SAFETY: tcgetattr succeeded, so saved is fully initialized.
    let saved = unsafe { saved.assume_init() };

    // Store for the signal handler before installing it.
    unsafe {
        (*SIGINT_SAVED.0.get()).write(saved);
    }
    SIGINT_FD.store(stdin_fd, Ordering::Release);

    // Install temporary SIGINT handler.
    // SAFETY: on_sigint is a valid extern "C" fn.
    let old_sigint =
        unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };

    // Disable echo (keep ICANON so read_line still works line-by-line).
    let mut raw = saved;
    raw.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw) } != 0 {
        // Restore signal handler — the RAII guard hasn't been created yet.
        unsafe {
            libc::signal(libc::SIGINT, old_sigint);
        }
        SIGINT_FD.store(-1, Ordering::Release);
        eprintln!("\nclaudio proxy login: cannot disable echo; aborting");
        std::process::exit(1);
    }

    // RAII guard: restores termios and signal handler on every exit path
    // (normal return, panic, early return, etc.).
    let _guard = NoEchoGuard {
        fd: stdin_fd,
        saved,
        old_sigint,
    };

    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    eprintln!(); // newline after the hidden input

    line.trim().to_owned()
}

// ── status ────────────────────────────────────────────────────────────────────

fn cmd_status() -> ExitCode {
    let sec = match profile::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("claudio proxy: could not read config.toml: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Also consider the env-var ephemeral profile.
    let env_profile = profile::from_env();

    let default_name = env_profile
        .as_ref()
        .map(|(n, _)| n.to_string())
        .or_else(|| sec.default.clone());

    if sec.profiles.is_empty() && env_profile.is_none() {
        println!("No proxy profiles configured.");
        println!("Run `claudio proxy login` or set CLAUDIO_PROXY_URL.");
        return ExitCode::SUCCESS;
    }

    println!("Proxy profiles:");
    // Show env profile first, then saved ones.
    if let Some((n, p)) = &env_profile {
        let marker = if default_name.as_deref() == Some(n) {
            " (default, env)"
        } else {
            " (env)"
        };
        println!("  {n}{marker}  {}  {}", p.url, p.masked_token());
    }
    for (n, p) in &sec.profiles {
        let marker = if default_name.as_deref() == Some(n.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("  {n}{marker}  {}  {}", p.url, p.masked_token());
    }

    // Live stats for the default profile.
    let active_profile = env_profile
        .as_ref()
        .map(|(_, p)| p)
        .or_else(|| sec.default.as_ref().and_then(|n| sec.profiles.get(n)));

    if let Some(p) = active_profile {
        println!();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(print_live_status(p));
    }

    ExitCode::SUCCESS
}

async fn print_live_status(p: &Profile) {
    // Stats.
    match api::fetch_stats(&p.url, &p.token, "24h").await {
        Ok(s) => {
            let total_tok = s.totals.input_tokens + s.totals.output_tokens;
            println!(
                "Stats ({}): {} req  {}  {} tok total",
                s.period,
                s.totals.requests,
                s.user_name,
                fmt_tokens(total_tok),
            );
            if !s.by_model.is_empty() {
                println!("  By model:");
                for m in &s.by_model {
                    println!(
                        "    {:50}  {} req  {} out tok",
                        m.model,
                        m.requests,
                        fmt_tokens(m.output_tokens)
                    );
                }
            }
            if let Some(lim) = &s.limit {
                println!(
                    "  Limit: {:.0}% of {} out-tok/{}s",
                    lim.used_pct * 100.0,
                    fmt_tokens(lim.output_tokens),
                    lim.window_seconds,
                );
                if lim.blocked {
                    println!("  BLOCKED until {:?}", lim.blocked_until);
                }
            }
        }
        Err(e) => eprintln!("  Stats unavailable: {e}"),
    }

    // Pool health.
    match api::fetch_pool_health(&p.url, &p.token).await {
        Ok(h) => {
            println!("Pool: {}", h.overall().label());
            for prov in &h.providers {
                println!("  {:20} {}", prov.name, prov.status);
            }
        }
        Err(e) => eprintln!("  Pool health unavailable: {e}"),
    }
}

/// Format a token count for display (e.g. `1.2M`, `340k`, `42`).
/// Made `pub` so other modules (e.g. the TUI) can reuse this formatter.
pub fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ── logout ────────────────────────────────────────────────────────────────────

fn cmd_logout(args: &[String]) -> ExitCode {
    let mut sec = match profile::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("claudio proxy: could not read config.toml: {e}");
            return ExitCode::FAILURE;
        }
    };

    let name = match args.first() {
        Some(n) => n.clone(),
        None => match &sec.default {
            Some(d) => d.clone(),
            None => {
                eprintln!("claudio proxy logout: no profile name given and no default set");
                return ExitCode::FAILURE;
            }
        },
    };

    if sec.profiles.remove(&name).is_none() {
        eprintln!("claudio proxy logout: profile '{name}' not found");
        return ExitCode::FAILURE;
    }

    // If we removed the default, clear it (or pick another one).
    if sec.default.as_deref() == Some(&name) {
        sec.default = sec.profiles.keys().next().cloned();
    }

    if let Err(e) = profile::save(&sec) {
        eprintln!("claudio proxy logout: could not save config.toml: {e}");
        return ExitCode::FAILURE;
    }
    println!("Removed profile '{name}'.");
    ExitCode::SUCCESS
}

// ── use ───────────────────────────────────────────────────────────────────────

fn cmd_use(args: &[String]) -> ExitCode {
    let name = match args.first() {
        Some(n) => n.clone(),
        None => {
            eprintln!("claudio proxy use: missing NAME or 'none'");
            return ExitCode::FAILURE;
        }
    };

    let mut sec = match profile::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("claudio proxy: could not read config.toml: {e}");
            return ExitCode::FAILURE;
        }
    };

    if name == "none" {
        sec.default = None;
        println!("Default proxy cleared (sessions start without a proxy).");
    } else {
        if !sec.profiles.contains_key(&name) {
            eprintln!("claudio proxy use: profile '{name}' not found");
            return ExitCode::FAILURE;
        }
        sec.default = Some(name.clone());
        println!("Default proxy set to '{name}'.");
    }

    if let Err(e) = profile::save(&sec) {
        eprintln!("claudio proxy use: could not save config.toml: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relogin_keeps_first_party_mode() {
        let mut sec = profile::ProxySection::default();
        sec.profiles.insert(
            "work".into(),
            Profile {
                url: "https://old.example.net".into(),
                token: "old".into(),
                first_party: true,
            },
        );
        store_login(
            &mut sec,
            "work",
            "https://new.example.net".into(),
            "new".into(),
        );
        let p = &sec.profiles["work"];
        assert_eq!(p.url, "https://new.example.net");
        assert_eq!(p.token, "new");
        assert!(p.first_party);
    }

    #[test]
    fn new_profile_is_gateway() {
        let mut sec = profile::ProxySection::default();
        store_login(&mut sec, "work", "https://a.example.net".into(), "t".into());
        assert!(!sec.profiles["work"].first_party);
    }
}
