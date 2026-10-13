//! Everything claudio knows about Claude Code itself.
//!
//! Sub-modules:
//!
//! - [`hooks`] — hook event constants, settings builder, per-spawn token
//!   generator, and the short-lived relay process.
//! - [`projects`] — locating project directories, listing sessions, and
//!   reading transcript metadata.
//! - [`update`] — version parsing, release-channel lookup and the daily
//!   check behind "keep claude up to date".
//! - [`state`] — pure state machine that maps hook events onto
//!   [`proto::SessionEvent`]s.

pub mod hooks;
pub mod projects;
pub mod state;
pub mod update;

use std::process::{Command, ExitCode};

/// Variables a running claude sets for its own children. A process started
/// from inside a claude session inherits them, and a session spawned with them
/// believes it is a child: it turns off transcript saving (breaking `--resume`)
/// and talks to the parent's messaging socket. User settings are deliberately
/// kept, except that a first-party proxy session removes an inherited
/// `CLAUDE_CODE_USE_GATEWAY` (see `proxy::env::env_diff`).
pub const SESSION_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

/// The real `claude` binary: `$CLAUDIO_CLAUDE_PATH`, else `claude` on `PATH`.
pub fn binary() -> String {
    std::env::var("CLAUDIO_CLAUDE_PATH").unwrap_or_else(|_| "claude".into())
}

/// Replace this process with `cmd`. On Unix this is a true `execvp` (transparent
/// signals, exit code, TTY). On other platforms we spawn, wait, and propagate
/// the exit code.
pub fn exec(mut cmd: Command) -> ExitCode {
    let program = cmd.get_program().to_string_lossy().into_owned();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        // exec only returns on failure.
        eprintln!("claudio: could not exec '{program}': {err}");
        ExitCode::from(127)
    }

    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(e) => {
                eprintln!("claudio: could not run '{program}': {e}");
                ExitCode::from(127)
            }
        }
    }
}
