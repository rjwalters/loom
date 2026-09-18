//! Sub-actions for `loom-daemon sweep-experiment` (issue #4275).
//!
//! Split out of `main.rs` (#8081) so a new sub-action — like `Fleet`
//! (#8055 phases 1-2) — costs `main.rs` nothing: that file is frozen by the
//! file-size ratchet (`.loom/docs/file-size-policy.md`), the same reason
//! `cli::script_ports` exists.

use clap::Subcommand;

// `Record` carries the full outcome-chain field set, so it is much larger
// than the other variants; boxing it would only add an allocation to a
// once-per-invocation CLI parse.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub(crate) enum SweepExperimentAction {
    /// Print the effective tri-state mode (after the canary guardrail).
    ResolveMode {
        #[arg(long, value_name = "PATH")]
        config: Option<String>,
    },

    /// Print the deterministic per-issue arm + forced model.
    AssignArm {
        #[arg(long, value_name = "N")]
        issue: i64,

        #[arg(long, value_name = "TIER")]
        complexity: Option<String>,

        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,

        /// Print the concrete model ID the arm's alias resolves to (#3982)
        /// instead of the bare alias.
        #[arg(long)]
        resolve: bool,

        #[arg(long, value_name = "PATH")]
        config: Option<String>,
    },

    /// Print the loud startup banner naming mode + arm.
    Banner {
        #[arg(long, value_name = "N")]
        issue: i64,

        #[arg(long, value_name = "TIER")]
        complexity: Option<String>,

        #[arg(long, value_name = "PATH")]
        config: Option<String>,
    },

    /// Append one JSONL outcome-chain record.
    Record {
        #[arg(long, value_name = "N")]
        issue: i64,

        #[arg(long)]
        phase: String,

        #[arg(long)]
        role: String,

        #[arg(long)]
        model: Option<String>,

        #[arg(long, default_value = "observe")]
        mode: String,

        #[arg(long)]
        arm: Option<String>,

        #[arg(long, default_value_t = 1)]
        attempt: i64,

        #[arg(long)]
        complexity: Option<String>,

        #[arg(long)]
        verdict: Option<String>,

        #[arg(long = "cycle-count", default_value_t = 0)]
        cycle_count: i64,

        #[arg(long)]
        pr: Option<i64>,

        #[arg(long)]
        effort: Option<String>,

        #[arg(long = "agent-id")]
        agent_id: Option<String>,

        #[arg(long)]
        transcript: Option<String>,

        #[arg(long = "in-tok")]
        in_tok: Option<i64>,

        #[arg(long = "out-tok")]
        out_tok: Option<i64>,

        #[arg(long = "token-fidelity", default_value = "none")]
        token_fidelity: String,

        #[arg(long = "stats-file")]
        stats_file: Option<String>,

        #[arg(long)]
        quiet: bool,
    },

    /// Fleet-wide, repo-stratified model A/B: `plan`, `start`, `stop`
    /// (#8055 phases 1-2). Flattened in from `cli::fleet_experiment` so they
    /// are direct sub-actions of `sweep-experiment` while `main.rs` — frozen
    /// by the file-size ratchet — carries one variant for all three.
    #[command(flatten)]
    Fleet(super::fleet_experiment::FleetExperimentAction),

    /// Aggregate the stats store into the per-arm #3718 inequality inputs.
    Harvest {
        #[arg(long = "stats-file")]
        stats_file: Option<String>,

        #[arg(long = "archive-dir")]
        archive_dir: Option<String>,

        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
    },
}
