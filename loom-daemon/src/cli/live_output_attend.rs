//! `loom-daemon live-output-attend` (Issue #10116): publish `session.output`
//! for an agent started from an attended Claude Code session.
//!
//! `lease ensure` already does this at every Builder and Doctor claim step, so
//! this verb is for any other entry point that knows its issue. Run it from
//! inside a subagent's own tool call: it finds that agent's transcript by the
//! running command (through this process's parent shells), detaches a
//! tailer, and returns at once. A top-level session is followed only for a
//! turn a `/loom:<role>` command naming the issue opened; an inline claim by
//! an operator's main agent is refused with a reason (#10129).
//!
//! **Always exits 0.** With live output not configured it does nothing and
//! says why in one stderr line, also kept in
//! `.loom/logs/live-output-attended/last-start.log` (#10125). The logic lives in
//! `loom_daemon::observability::session_output::attended`.

use std::path::PathBuf;

use loom_daemon::observability::session_output::attended::{
    self, AttendEnv, StartRequest, DEFAULT_IDLE_EXIT_SECS, DEFAULT_MAX_AGE_SECS,
};

#[derive(clap::Args)]
pub(crate) struct LiveOutputAttendArgs {
    /// The issue this agent is working on.
    #[arg(long)]
    issue: u32,
    /// The Loom role. Defaults to the subagent's `loom-<role>` type.
    #[arg(long)]
    role: Option<String>,
    /// The session's durable pid; the run ends when it exits. Defaults to
    /// `$LOOM_AGENT_SESSION_PID`, then `$CLAUDE_PID`.
    #[arg(long, value_name = "PID")]
    watch_pid: Option<u32>,
    /// Any directory inside the claim's checkout.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Read this transcript instead of locating the calling agent's own. It
    /// should be a subagent's: a top-level transcript is refused unless the
    /// detached `--foreground` tailer finds its claim turn was opened by a
    /// `/loom:<role>` command naming the issue.
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// Byte offset in `--transcript` where this issue's lines begin (the
    /// claim line). Defaults to the transcript's current end.
    #[arg(long, value_name = "BYTES", hide = true)]
    from_offset: Option<u64>,
    /// Stop following after this many seconds.
    #[arg(long, default_value_t = DEFAULT_MAX_AGE_SECS)]
    max_age: u64,
    /// Treat the run as finished once its transcript is idle this long.
    #[arg(long, default_value_t = DEFAULT_IDLE_EXIT_SECS)]
    idle_exit: u64,
    /// Follow the run in this process (what the detached tailer runs).
    #[arg(long, hide = true)]
    foreground: bool,
}

impl LiveOutputAttendArgs {
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        let env = AttendEnv::from_process();
        let request = StartRequest {
            issue: self.issue,
            role: self.role,
            watch_pid: self.watch_pid.or_else(attended::session_pid_from_env),
            workspace: self.workspace,
            transcript: self.transcript,
            from_offset: self.from_offset,
            max_age_secs: self.max_age,
            idle_exit_secs: self.idle_exit,
        };
        let outcome = if self.foreground {
            attended::run_foreground(&request, &env).await
        } else {
            attended::start(&request, &env)
        };
        eprintln!("live-output-attend: {}", outcome.describe(self.issue));
        Ok(())
    }
}
