//! `loom-daemon sweep-experiment plan | start | stop` — the fleet-wide,
//! repo-stratified model A/B (Issue #8055, phases 1–2 of 6).
//!
//! These are **sub-actions on the existing `sweep-experiment` verb**, not a new
//! top-level `experiment` verb. `sweep-experiment` already owns the arm
//! vocabulary (`assign_arm`, `arm_model`, `resolved_arm_model`) for the
//! per-issue mode; a second verb speaking the same vocabulary is how the two
//! would drift on what "an arm" means. They are flattened in from this module
//! (rather than declared in `main.rs`) because `main.rs` is frozen by the
//! file-size ratchet — the same reason `cli::script_ports` exists.
//!
//! All logic lives in [`loom_daemon::script_helpers::fleet_experiment`]; this
//! file is argument parsing and rendering only.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use loom_daemon::script_helpers::fleet_experiment as fx;
use loom_daemon::script_helpers::fleet_experiment::lifecycle;

/// Default stratification: pair repos of similar recent throughput and similar
/// kind, so an arm difference is not confounded by one arm drawing all the busy
/// Rust repos.
const DEFAULT_STRATIFY: &str = "merges14d,kind";

#[derive(clap::Subcommand)]
pub(crate) enum FleetExperimentAction {
    /// Assign every registered workspace to an arm, deterministically. Writes
    /// nothing except the optional --out plan file (#8055 phase 1).
    Plan {
        /// Comma-separated arms. An arm's name IS the model alias written into
        /// the overlay, e.g. `--arms opus,sonnet`.
        #[arg(long, value_name = "A,B", default_value = "opus,sonnet")]
        arms: String,

        /// Comma-separated stratification dimensions: `merges14d` (merged PRs
        /// in the last 14 days, median split; falls back to an alphabetical
        /// split when any repo cannot be measured) and `kind` (build-manifest
        /// heuristic: rust/node/python/go/shell/docs). `none` disables
        /// stratification.
        #[arg(long, value_name = "DIMS", default_value = DEFAULT_STRATIFY)]
        stratify: String,

        /// Assignment seed. The same seed, workspace set and strata always
        /// produce the same plan.
        #[arg(long, value_name = "N", default_value_t = 0)]
        seed: u64,

        /// Write the plan document here. `start` consumes this file.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,

        /// Skip the `gh` merge-count query entirely (the `merges14d` dimension
        /// degrades to its alphabetical fallback).
        #[arg(long)]
        offline: bool,

        /// Print the plan document instead of the table.
        #[arg(long)]
        json: bool,
    },

    /// Write the plan's overlays into each workspace's `.loom-local/local.json`
    /// and record the experiment under `~/.loom/experiments/` (#8055 phase 2).
    Start {
        /// The plan file produced by `plan --out`.
        #[arg(long, value_name = "PATH")]
        plan: PathBuf,

        /// Advisory end date (recorded, never enforced — `stop` ends it).
        #[arg(long, value_name = "DATE")]
        until: Option<String>,

        /// Start even though a workspace's token pool is currently held.
        #[arg(long = "allow-exhausted-pool")]
        allow_exhausted_pool: bool,
    },

    /// Reverse exactly the overlay keys `start` wrote (#8055 phase 2).
    Stop {
        /// The experiment id (`exp-YYYYMMDD-xxxxxxxx`).
        #[arg(long, value_name = "ID")]
        id: String,

        /// Also revert workspaces whose written values were hand-edited.
        #[arg(long)]
        force: bool,
    },
}

impl FleetExperimentAction {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            FleetExperimentAction::Plan {
                arms,
                stratify,
                seed,
                out,
                offline,
                json,
            } => run_plan(&arms, &stratify, seed, out.as_deref(), offline, json),
            FleetExperimentAction::Start {
                plan,
                until,
                allow_exhausted_pool,
            } => run_start(&plan, until.as_deref(), allow_exhausted_pool),
            FleetExperimentAction::Stop { id, force } => run_stop(&id, force),
        }
    }
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn run_plan(
    arms_raw: &str,
    stratify_raw: &str,
    seed: u64,
    out: Option<&Path>,
    offline: bool,
    json: bool,
) -> Result<()> {
    let arms = split_list(arms_raw);
    let dims = if stratify_raw.trim().eq_ignore_ascii_case("none") {
        Vec::new()
    } else {
        split_list(stratify_raw)
    };

    let registry = loom_daemon::workspace_registry::WorkspaceRegistry::load_default()
        .context("reading the workspace registry")?;
    let roots: Vec<PathBuf> = registry.workspaces.iter().map(|w| w.root.clone()).collect();
    if roots.is_empty() {
        bail!(
            "no workspaces are registered — `loom-daemon workspace add <path>` first, or point \
             LOOM_WORKSPACES_PATH at the registry you meant"
        );
    }

    let now = chrono::Utc::now();
    let (inputs, notes) = fx::collect_inputs(&roots, offline, now);
    for note in &notes {
        eprintln!("[sweep-experiment plan] {note}");
    }
    let plan = fx::build_plan(&inputs, &arms, &dims, seed, now)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print!("{}", fx::format_plan_table(&plan));
    }

    if let Some(path) = out {
        loom_daemon::script_helpers::write_json_file(path, &serde_json::to_value(&plan)?)
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "[sweep-experiment plan] wrote {} — start it with:\n  loom-daemon sweep-experiment \
             start --plan {}",
            path.display(),
            path.display()
        );
    } else {
        eprintln!(
            "[sweep-experiment plan] nothing written (pass --out <file> to keep this plan; \
             `start` needs the file)"
        );
    }
    Ok(())
}

fn run_start(plan_path: &Path, until: Option<&str>, allow_exhausted_pool: bool) -> Result<()> {
    let text = std::fs::read_to_string(plan_path)
        .with_context(|| format!("reading plan {}", plan_path.display()))?;
    let plan: fx::Plan = serde_json::from_str(&text)
        .with_context(|| format!("parsing plan {}", plan_path.display()))?;

    let state_dir = lifecycle::state_dir()?;
    let report = lifecycle::start(lifecycle::StartOptions {
        plan: &plan,
        until,
        allow_exhausted_pool,
        state_dir: &state_dir,
        now: chrono::Utc::now(),
        pool_held: lifecycle::live_pool_held,
    })?;

    for warning in &report.warnings {
        eprintln!("[sweep-experiment start] WARNING: {warning}");
    }
    println!("started {} on {} workspace(s)", plan.experiment_id, report.started.len());
    for (root, arm) in &report.started {
        println!("  {arm:8}  {}", root.display());
    }
    println!("state: {}", report.state_file.display());
    println!("stop with: loom-daemon sweep-experiment stop --id {}", plan.experiment_id);
    Ok(())
}

fn run_stop(id: &str, force: bool) -> Result<()> {
    let state_dir = lifecycle::state_dir()?;
    let (report, _state) = lifecycle::stop(&state_dir, id, force, chrono::Utc::now())?;

    for path in &report.reverted {
        println!("reverted  {}", path.display());
    }
    for path in &report.deleted {
        println!("removed   {}", path.display());
    }
    for note in &report.skipped {
        eprintln!("[sweep-experiment stop] skipped {note}");
    }
    for note in &report.drifted {
        eprintln!("[sweep-experiment stop] DRIFT {note}");
    }
    if report.drifted.is_empty() {
        println!("stopped {id}");
        Ok(())
    } else {
        bail!(
            "{id}: {} workspace(s) were hand-edited and were left in place; re-run with --force to \
             revert them anyway",
            report.drifted.len()
        )
    }
}
