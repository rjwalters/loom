//! `loom-daemon usage-record` (Issue #9303): journal one role attempt's token
//! usage into the issue's story trace. `usage-record`, not `usage record`, so
//! `usage` stays a leaf command (the `usage-report` precedent).
//!
//! **Always exits 0.** The sweep prompt runs it after every checkpoint write;
//! a telemetry problem must never fail, or even slow, a sweep. The logic lives
//! in `loom_daemon::observability::runtime_usage::record`.

use std::path::PathBuf;

use loom_daemon::observability::runtime_usage::record::{record, Outcome, Request};

#[derive(clap::Args)]
pub(crate) struct UsageRecordArgs {
    /// The issue the attempt worked on (selects its story trace).
    #[arg(long)]
    issue: u32,
    /// The role (`curator`, `builder`, `judge`, `doctor`, `merge`, …).
    #[arg(long)]
    role: String,
    /// The attempt number, when the sweep tracks one.
    #[arg(long)]
    attempt: Option<u32>,
    /// The role subagent's agent id (Task-result metadata); its
    /// `agent-<id>.jsonl` transcript is read.
    #[arg(long)]
    agent_id: Option<String>,
    /// The in-session sweep's run id (`$RUN_ID`, as passed to checkpoints).
    #[arg(long)]
    task_id: Option<String>,
    /// Read this transcript instead of resolving `--agent-id`.
    #[arg(long)]
    transcript: Option<PathBuf>,
}

impl UsageRecordArgs {
    pub(crate) fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let request = Request {
            issue: self.issue,
            role: self.role,
            attempt: self.attempt,
            agent_id: self.agent_id,
            task_id: self.task_id,
            transcript: self.transcript,
        };
        let summary = match record(&cwd, &request) {
            Outcome::Recorded(n) => serde_json::json!({"recorded": n}),
            Outcome::Skipped(why) => serde_json::json!({"recorded": 0, "skipped": why}),
            Outcome::Failed(error) => serde_json::json!({"recorded": 0, "error": error}),
        };
        println!("{summary}");
        Ok(())
    }
}
