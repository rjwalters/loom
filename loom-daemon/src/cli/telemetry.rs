//! Token-cost telemetry subcommands: what a fleet's agents actually spent.
//!
//! Five members: `usage` (what the Anthropic OAuth API reports for the
//! current credential, live), `ingest-transcripts` (Issue #8059: persist
//! what the local transcripts record into `activity.db`), `usage-report`
//! (Issue #8062: the role/model/repo/day cost breakdown over the rows
//! ingestion writes), `opencode-usage` (Issue #8507: the same question asked
//! of OpenCode's own session store), and `archive-transcripts` (Issue #8494:
//! a verified `.tar.zst` backstop for the *raw* transcripts, which
//! ingestion's derived data does not cover).
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

#[path = "telemetry_export.rs"]
mod telemetry_export;
#[path = "telemetry_fixture.rs"]
mod telemetry_fixture;
#[path = "telemetry_live.rs"]
mod telemetry_live;
#[path = "telemetry_overhead.rs"]
mod telemetry_overhead;

#[derive(clap::Subcommand)]
pub(crate) enum TelemetryCommand {
    /// Plan or explicitly execute two bounded Pi/OpenCode GLM smoke runs.
    TelemetryLiveCanary(telemetry_live::LiveArgs),
    /// Write deterministic synthetic telemetry and an independent-query manifest.
    TelemetryFixture(telemetry_fixture::FixtureArgs),
    /// Measure what lifecycle instrumentation costs on a representative run.
    TelemetryOverhead(telemetry_overhead::OverheadArgs),
    /// Send a bounded JSONL fixture to an explicit OTLP Collector endpoint.
    TelemetryExport(telemetry_export::TelemetryExportArgs),
    /// Report transport capabilities of this installed binary without reading configuration.
    TelemetryCapabilities {
        /// Fail when this artifact cannot export OTLP.
        #[arg(long)]
        require_otlp: bool,
    },

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

    /// Token/cost usage report, grouped by role, model, repo, or day
    /// (Issue #8062).
    ///
    /// Reads only the already-ingested `resource_usage` table — the same
    /// data `ingest-transcripts` populates — and sums each row's
    /// already-priced `cost_usd` rather than re-deriving a second pricing
    /// table. `loom-daemon usage-report --since 7d --by role`, not
    /// `loom-daemon usage report`: `usage` stays a leaf command (its
    /// `--status` flag, unchanged) so `check-usage.sh`'s contract is never at
    /// risk of a parse ambiguity — see `usage_report_cli`'s module doc.
    UsageReport(super::usage_report_cli::UsageReportArgs),

    /// Per-model token usage from OpenCode's own session store (Issue #8507).
    ///
    /// The backfill and verification path for non-Claude runtimes: the same
    /// reader a sweep's `tokens_by_model` now uses, pointed at any directory
    /// and window. Read-only — one query, naming `session` alone, never the
    /// `credential`/`account` tables in the same file.
    OpencodeUsage(super::opencode_usage_cli::OpencodeUsageArgs),

    /// Per-model token usage from Codex's own rollout session store (Issue
    /// #8594).
    ///
    /// The Codex sibling of `opencode-usage`, with the same two jobs
    /// (backfill, and verifying what a sweep's `tokens_by_model` will carry).
    /// Read-only — the reader's whole file-open surface refuses any path
    /// outside `sessions/**/rollout-*.jsonl`, so `$CODEX_HOME`'s `auth.json`,
    /// `config.toml` and `history.jsonl` are unreachable from it.
    CodexUsage(super::codex_usage_cli::CodexUsageArgs),

    /// Per-model token usage from the Pi `--mode json` event stream a launch
    /// log captured (Issue #8594).
    ///
    /// The Pi sibling of `opencode-usage`/`codex-usage` (backfill, and
    /// verifying what a sweep's `tokens_by_model` carries). Read-only — the
    /// reader opens only `.loom/logs/sweep-issue-<N>.log` / `role-<role>.log`,
    /// never Pi's agent directory or its `auth.json`.
    PiUsage(super::pi_usage_cli::PiUsageArgs),

    /// Roll raw Claude Code transcripts into a verified, incremental
    /// `.tar.zst` archive before Claude Code's `cleanupPeriodDays` fuse
    /// deletes them (#8494, split from #8477's item 5).
    ///
    /// #8477 (`ingest-transcripts`, above) preserves the *derived* token/cost
    /// data, not the raw transcripts themselves. This covers those: excludes
    /// `~/.claude/projects/<project>/memory/` (persistent agent memory, never
    /// a transcript), records a manifest (path/size/mtime/sha256) per
    /// archived file, reads the archive back to confirm it matches before
    /// recording anything, and skips a transcript already archived with a
    /// matching size/mtime on a later run. Opt-in and operator-driven — never
    /// started automatically.
    ArchiveTranscripts(super::transcript_archive_cli::ArchiveTranscriptsArgs),

    // Issue #8824 — help text lives on `CiTelemetryArgs` itself.
    CiTelemetry(super::ci_telemetry_cli::CiTelemetryArgs),
}

impl TelemetryCommand {
    /// Run the selected telemetry subcommand.
    ///
    /// # Errors
    ///
    /// Propagates the subcommand's own failure. The `usage` arm never returns:
    /// it exits with the process code `check-usage.sh` branches on.
    pub(crate) async fn run(self) -> Result<()> {
        match self {
            TelemetryCommand::TelemetryLiveCanary(args) => args.run(),
            TelemetryCommand::TelemetryFixture(args) => args.run(),
            TelemetryCommand::TelemetryOverhead(args) => args.run(),
            TelemetryCommand::TelemetryExport(args) => args.run().await,
            TelemetryCommand::TelemetryCapabilities { require_otlp } => {
                println!(
                    "{}",
                    serde_json::json!({"otlp": cfg!(feature = "otlp"), "otlp_protocol": "http/json"})
                );
                anyhow::ensure!(
                    !require_otlp || cfg!(feature = "otlp"),
                    "this binary was built without the otlp feature"
                );
                Ok(())
            }

            TelemetryCommand::Usage(args) => args.run(),
            TelemetryCommand::IngestTranscripts(args) => args.run(),
            TelemetryCommand::UsageReport(args) => args.run(),
            TelemetryCommand::OpencodeUsage(args) => args.run(),
            TelemetryCommand::CodexUsage(args) => args.run(),
            TelemetryCommand::PiUsage(args) => args.run(),
            TelemetryCommand::ArchiveTranscripts(args) => args.run(),
            TelemetryCommand::CiTelemetry(args) => args.run(),
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
