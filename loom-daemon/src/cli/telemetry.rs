//! Token-cost telemetry subcommands: what a fleet's agents actually spent.
//!
//! Two members today — `usage` (what the Anthropic OAuth API reports for the
//! current credential, live) and `ingest-transcripts` (Issue #8059: persist
//! what the local transcripts record into `activity.db`) — and one expected
//! (#8062's report over the rows ingestion writes).
//!
//! They are gathered into one **flattened** enum, exactly as
//! [`super::script_ports`] is and for the same second reason: `main.rs` is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen, so a subcommand
//! that costs it a variant-with-args each time cannot be added at all.
//! Flattening keeps every subcommand top-level on the CLI (`loom-daemon usage`,
//! `loom-daemon ingest-transcripts` — neither gains a `telemetry` prefix) while
//! `main.rs` holds a single variant and a single dispatch arm for the family.
//!
//! `usage` moved here from `main.rs` unchanged — same name, same `--status`
//! flag, same `std::process::exit` contract that `check-usage.sh` branches on.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::script_helpers;

#[derive(clap::Subcommand)]
pub(crate) enum TelemetryCommand {
    /// Query Claude API usage via the Anthropic OAuth API (native port of
    /// `loom_tools.common.usage`, #4275). Backs `check-usage.sh`.
    ///
    /// Exits 1 when the payload carries an `error` key (no Keychain token, API
    /// failure, or not inside a Loom repo) — the historical contract.
    Usage(UsageArgs),

    /// Ingest Claude Code transcript token usage into `activity.db` (#8059).
    ///
    /// `resource_usage` had one writer — the managed-terminal IPC path — which
    /// a dispatched `claude -p` sweep never traverses, so on a dispatch-driven
    /// host the cost tables and every view over them were permanently empty.
    /// This reads the transcripts those sweeps already wrote, dedupes each
    /// message's streamed chunks on `message.id`, skips `<synthetic>` models,
    /// attributes a role from the session's first user message, and writes one
    /// `resource_usage` row per (model, UTC day) per transcript.
    ///
    /// Safe to re-run: an unchanged transcript is skipped, and a transcript
    /// that has grown has its rows replaced rather than appended to.
    IngestTranscripts(super::transcript_ingest_cli::IngestTranscriptsArgs),
}

impl TelemetryCommand {
    /// Run the selected telemetry subcommand.
    ///
    /// # Errors
    ///
    /// Propagates the subcommand's own failure. The `usage` arm never returns:
    /// it exits with the process code `check-usage.sh` branches on.
    pub(crate) fn run(self) -> Result<()> {
        match self {
            TelemetryCommand::Usage(args) => args.run(),
            TelemetryCommand::IngestTranscripts(args) => args.run(),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct UsageArgs {
    /// Print a human-readable status block instead of JSON.
    #[arg(long)]
    pub status: bool,
}

impl UsageArgs {
    /// Never returns — exits with `script_helpers::usage`'s own code.
    fn run(self) -> Result<()> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        std::process::exit(script_helpers::usage::run(self.status, &cwd));
    }
}
