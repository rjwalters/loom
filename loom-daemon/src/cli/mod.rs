//! `loom-daemon` CLI subcommand handlers (Issue #4712).
//!
//! `main.rs` keeps only the clap `Cli`/`Commands` derive tree and the thin
//! dispatch match (`handle_cli_command`); every subcommand's handler body
//! lives in one of these submodules, grouped by family. `daemon_service`
//! (a sibling of this module, not nested under it) owns the daemon's own
//! bootstrap/service-loop body, which is not a CLI subcommand handler.

pub(crate) mod accounts;
pub(crate) mod cancel;
pub(crate) mod cleanup_ops;
pub(crate) mod common;
pub(crate) mod dep_classify;
pub(crate) mod dep_recheck;
pub(crate) mod dispatch;
pub(crate) mod dispatch_backoff;
mod duplicate_scan;
pub(crate) mod fleet_experiment;
pub(crate) mod health;
pub(crate) mod inflight;
pub(crate) mod lease_ensure;
pub(crate) mod legacy_script_cmds;
mod merge_pr_labels;
mod merge_pr_refs;
mod merge_pr_stale_checks;
pub(crate) mod misc_cmds;
pub(crate) mod noop_cooldown;
pub(crate) mod peer_claims_cmd;
pub(crate) mod quarantine;
pub(crate) mod release_fetch;
pub(crate) mod release_resolve;
pub(crate) mod restart;
pub(crate) mod retry_classify;
pub(crate) mod script_ports;
pub(crate) mod serve_cmd;
mod shell_budget;
mod skip_labels;
pub(crate) mod stashes;
pub(crate) mod stats;
pub(crate) mod status;
pub(crate) mod status_render;
pub(crate) mod sweep_experiment;
pub(crate) mod sweep_outcomes_cli;
pub(crate) mod telemetry;
pub(crate) mod tokens;
pub(crate) mod tokens_weekly_points;
pub(crate) mod transcript_ingest_cli;
pub(crate) mod usage_report_cli;
pub(crate) mod watch;
mod watchdog;
pub(crate) mod workspace_fleet;
pub(crate) mod worktree_lock;
mod worktree_state;
pub(crate) mod worktree_wip;
