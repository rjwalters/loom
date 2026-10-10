//! `loom-daemon` CLI subcommand handlers (Issue #4712).
//!
//! `main.rs` keeps only the clap `Cli`/`Commands` derive tree and the thin
//! dispatch match (`handle_cli_command`); every subcommand's handler body
//! lives in one of these submodules, grouped by family. `daemon_service`
//! (a sibling of this module, not nested under it) owns the daemon's own
//! bootstrap/service-loop body, which is not a CLI subcommand handler.

// The bin test binary gets the same pre-`main` live-`gh` guard as the lib
// test binary (#10138). `main.rs` is size-frozen, so it is mounted here.
#[cfg(test)]
#[path = "../live_gh_guard.rs"]
mod live_gh_guard;

// Whole-`Cli` parsing for tests, on an 8 MiB thread like the binary's main
// thread (#10616). `main.rs` is size-frozen, so it is mounted here.
#[cfg(test)]
pub(crate) mod whole_cli_parse;

pub(crate) mod accounts;
pub(crate) mod accounts_args;
pub(crate) mod accounts_session;
mod agent_skills;
pub(crate) mod api_keys;
pub(crate) mod attend_hook;
pub(crate) mod cancel;
pub(crate) mod cargo_target_dir;
mod check_guard_wiring;
mod check_renovate_labels;
pub(crate) mod ci_telemetry_cli;
pub(crate) mod cleanup_ops;
pub(crate) mod codex_hooks;
pub(crate) mod codex_sandbox_noop_cli;
pub(crate) mod codex_usage_cli;
pub(crate) mod common;
mod daemon_start;
mod daemon_update;
pub(crate) mod dep_classify;
pub(crate) mod dep_recheck;
pub(crate) mod dispatch;
pub(crate) mod dispatch_backoff;
mod duplicate_closed_search;
mod duplicate_scan;
mod fleet_captain_cmd;
pub(crate) mod fleet_config;
mod fleet_config_reload;
pub(crate) mod fleet_experiment;
pub(crate) mod fleet_send;
pub(crate) mod forge_action;
mod forge_calls_cmd;
mod forge_egress_cmd;
mod forge_identity_cmd;
pub(crate) mod forge_inventory_cmd;
mod forge_merge_queue_cmd;
mod forge_parent_cmd;
pub(crate) mod forge_probe_cmd;
mod forge_verdict_cmd;
mod git_blob_lines;
mod guard_mcp_tools;
mod guards_status;
pub(crate) mod health;
pub(crate) mod host;
pub(crate) mod inflight;
mod install_binary;
pub(crate) mod install_compat_cli;
mod label_duplicates;
pub(crate) mod labels_cmd;
pub(crate) mod lease_co_occupancy;
pub(crate) mod lease_ensure;
pub(crate) mod lease_renewer;
pub(crate) mod legacy_script_cmds;
pub(crate) mod merge_group_ci_cmd;
mod merge_pr_chain_lock;
mod merge_pr_check_runs_rollup;
mod merge_pr_check_runs_streak;
mod merge_pr_checks_failure;
mod merge_pr_ci_result;
mod merge_pr_cleanup_paths;
mod merge_pr_closed_building;
mod merge_pr_closed_by_merge;
mod merge_pr_consolidate;
mod merge_pr_delete_branch;
mod merge_pr_dirty_guard;
mod merge_pr_discovered_worktree;
mod merge_pr_head_sync;
mod merge_pr_hold_state;
mod merge_pr_issue_close_gate;
mod merge_pr_labels;
mod merge_pr_loom_pr_guard;
mod merge_pr_loom_pr_override_comment;
mod merge_pr_mergeable_recheck;
mod merge_pr_partial_comment;
mod merge_pr_partial_conflict;
mod merge_pr_partial_reset;
mod merge_pr_poll_wait;
mod merge_pr_reconcile;
mod merge_pr_redate;
mod merge_pr_redate_report;
mod merge_pr_refs;
mod merge_pr_remove_gate;
mod merge_pr_response;
mod merge_pr_retarget_children;
mod merge_pr_retries_used;
mod merge_pr_revalidate_head;
mod merge_pr_sequence;
mod merge_pr_stacked_children;
mod merge_pr_stale_checks;
mod merge_pr_tree_checks;
mod merge_pr_usage;
mod merge_pr_version_policy;
mod merge_pr_workflow_scope;
mod merge_pr_worktree_preserve;
mod merge_pr_worktree_teardown;
mod merge_pr_worktrees;
mod merge_pr_zero_checks;
pub(crate) mod misc_cmds;
pub(crate) mod noop_cooldown;
mod notify_cleared_blockers;
pub(crate) mod opencode_usage_cli;
pub(crate) mod operator_decision;
pub(crate) mod overlap_replay;
mod park_record;
pub(crate) mod peer_claims_cmd;
pub(crate) mod pi_usage_cli;
mod points_marker_check;
mod pr_latency_cmd;
mod pr_latency_render;
pub(crate) mod preflight;
pub(crate) mod premise_check;
pub(crate) mod provenance;
pub(crate) mod quarantine;
pub(crate) mod ready_queue_cmd;
pub(crate) mod reconcile_stack;
pub(crate) mod release_explain;
pub(crate) mod release_fetch;
pub(crate) mod release_resolve;
mod release_stale_blocked;
pub(crate) mod restart;
mod resync_payload_cmd;
mod resync_pin_cmd;
pub(crate) mod retry_classify;
pub(crate) mod role_tool_policy;
pub(crate) mod roll_pause_cli;
mod runtime_launch_cmd;
pub(crate) mod script_ports;
mod secret_scan_cmd;
pub(crate) mod serve_cmd;
mod shell_budget;
mod skip_labels;
mod stale_blocked;
pub(crate) mod stashes;
pub(crate) mod stats;
pub(crate) mod status;
pub(crate) mod status_render;
mod sweep_checkpoint;
pub(crate) mod sweep_experiment;
pub(crate) mod sweep_outcomes_cli;
pub(crate) mod target_dir_gc;
pub(crate) mod telemetry;
pub(crate) mod tmpfs_scratch_gc;
pub(crate) mod tokens;
pub(crate) mod tokens_weekly_points;
pub(crate) mod transcript_archive_cli;
pub(crate) mod transcript_ingest_cli;
pub(crate) mod usage_report_cli;
pub(crate) mod watch;
mod watchdog;
pub(crate) mod workspace_fleet;
pub(crate) mod worktree_base;
pub(crate) mod worktree_branch_conflict;
pub(crate) mod worktree_branch_reuse;
pub(crate) mod worktree_check;
pub(crate) mod worktree_cleanup;
pub(crate) mod worktree_closed_pr_branch;
pub(crate) mod worktree_existing;
pub(crate) mod worktree_link;
pub(crate) mod worktree_lock;
pub(crate) mod worktree_open_pr;
pub(crate) mod worktree_remove;
pub(crate) mod worktree_reset;
mod worktree_sparse;
pub(crate) mod worktree_stale_ref;
mod worktree_state;
pub(crate) mod worktree_submodules;
pub(crate) mod worktree_upstream;
pub(crate) mod worktree_wip;

pub(crate) mod pr_queue;
