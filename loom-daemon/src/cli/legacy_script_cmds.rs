//! Native ports of retired `loom_tools` script-helper CLIs (epic #4081
//! Phase 3 family 5, issue #4275) — `strip-ansi`, `resolve-model`,
//! `checkpoint`, `sweep-experiment` (Issue #4712 — split out of `main.rs`).
//! Each backs one `defaults/scripts/*.sh` entry point whose Python
//! implementation was deleted; flags, stdout shapes, and exit codes are
//! unchanged from the Python CLIs they replace.

use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;

use loom_daemon::script_helpers;

use crate::CheckpointAction;

use super::sweep_experiment::SweepExperimentAction;

/// `loom-daemon strip-ansi [--file PATH]` — backs `strip-ansi.sh`.
pub(crate) fn handle_strip_ansi_command(file: Option<&str>) -> Result<()> {
    use script_helpers::log_filter;

    if let Some(path) = file {
        print!("{}", log_filter::clean_file(Path::new(path)));
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // A broken downstream pipe is the normal way a pipe-pane filter ends; the
    // Python swallowed BrokenPipeError, so do the same rather than surfacing a
    // crash in every agent log.
    match log_filter::filter_stream(stdin.lock(), &mut stdout) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `loom-daemon resolve-model` — backs `resolve-model.sh`.
///
/// Exit codes: `0` on success, `2` (clap) when no model was supplied, and `3`
/// with NO output when `--task-alias` / `--downgrade` has no mapping so the
/// caller keeps its own precedence chain.
pub(crate) fn handle_resolve_model_command(
    model: &str,
    config: Option<&str>,
    generation: bool,
    task_alias: bool,
    downgrade: bool,
) -> Result<()> {
    use script_helpers::model_tiers;

    let cfg = model_tiers::load_config(config.map(Path::new));

    // Credit-exhaustion downgrade mode (#5687). Checked before --task-alias:
    // --downgrade already ends at a Task-passable alias, so passing both is a
    // caller shorthand for "degrade, then step one rung down", not a conflict.
    if downgrade {
        let cheaper = model_tiers::downgrade_task_alias(model);
        if cheaper.is_empty() {
            std::process::exit(3);
        }
        println!("{cheaper}");
        return Ok(());
    }

    // Task-tool degradation mode (#4282).
    if task_alias {
        let alias = model_tiers::task_alias_of(model);
        if alias.is_empty() {
            std::process::exit(3);
        }
        println!("{alias}");
        return Ok(());
    }

    if generation {
        match model_tiers::generation_of(model, &cfg) {
            Some(g) => println!("{g}"),
            // The Python printed an empty line for an unrecognized model.
            None => println!(),
        }
    } else {
        println!("{}", model_tiers::resolve_model(model, &cfg));
    }
    Ok(())
}

/// The worktree a checkpoint command targets: `--worktree` when given, else the
/// current directory (matching the Python default).
fn checkpoint_worktree(worktree: Option<&str>) -> PathBuf {
    worktree.map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        |w| std::fs::canonicalize(w).unwrap_or_else(|_| PathBuf::from(w)),
    )
}

/// `loom-daemon checkpoint <write|read|clear|stages>` — backs `checkpoint.sh`.
pub(crate) fn handle_checkpoint_command(action: CheckpointAction) -> Result<()> {
    use script_helpers::checkpoints;

    match action {
        CheckpointAction::Stages { json } => {
            if json {
                println!("{}", checkpoints::stages_value());
            } else {
                println!("{}", checkpoints::stages_text());
            }
            Ok(())
        }
        CheckpointAction::Write {
            worktree,
            stage,
            issue,
            files_changed,
            test_command,
            test_result,
            test_output_summary,
            commit_sha,
            pr_number,
            quiet,
        } => {
            let details = checkpoints::CheckpointDetails {
                files_changed: files_changed.unwrap_or(0),
                test_command: test_command.unwrap_or_default(),
                test_result: test_result.unwrap_or_default(),
                test_output_summary: test_output_summary.unwrap_or_default(),
                commit_sha: commit_sha.unwrap_or_default(),
                pr_number,
            };
            let ok = checkpoints::write_checkpoint(
                &checkpoint_worktree(worktree.as_deref()),
                &stage,
                issue.unwrap_or(0),
                details,
                quiet,
            );
            std::process::exit(i32::from(!ok));
        }
        CheckpointAction::Read { worktree, json } => {
            let path = checkpoint_worktree(worktree.as_deref());
            match checkpoints::read_checkpoint(&path) {
                None => {
                    if json {
                        println!("{}", serde_json::json!({"checkpoint": null, "exists": false}));
                    } else {
                        script_helpers::log_warning(&format!(
                            "No checkpoint found in {}",
                            path.display()
                        ));
                    }
                }
                Some(cp) => {
                    // #5403: resolve whether the checkpoint's issue is closed
                    // on the forge, so a stale checkpoint (e.g. surviving in
                    // the primary checkout past its issue's closure) is
                    // distinguishable from a live one. `issue == 0` (no issue
                    // recorded — issue #4275's original schema allows this)
                    // skips the forge call entirely and fails open, same as a
                    // lookup failure would. Reuses `worktree_ops::gh`'s
                    // REST-backed lookup — the same one `worktree_reaper.rs`
                    // already uses for this purpose — rather than adding a
                    // new forge call path.
                    let issue_status = u32::try_from(cp.issue).ok().filter(|n| *n > 0).map_or(
                        checkpoints::IssueStatus::Unknown,
                        |n| {
                            checkpoints::IssueStatus::from_gh_state(
                                &loom_daemon::worktree_ops::gh::issue_state_rest(&path, n),
                            )
                        },
                    );
                    let now = chrono::Utc::now();
                    if json {
                        println!("{}", checkpoints::read_json(&cp, issue_status, now));
                    } else {
                        println!("{}", checkpoints::read_text(&cp, issue_status, now));
                    }
                }
            }
            Ok(())
        }
        CheckpointAction::Clear { worktree, quiet } => {
            let ok =
                checkpoints::clear_checkpoint(&checkpoint_worktree(worktree.as_deref()), quiet);
            std::process::exit(i32::from(!ok));
        }
    }
}

/// `loom-daemon sweep-experiment <subcommand>` — backs `sweep-experiment.sh`.
#[allow(clippy::too_many_lines)]
pub(crate) fn handle_sweep_experiment_command(action: SweepExperimentAction) -> Result<()> {
    use script_helpers::{model_tiers, sweep_experiment as se, sweep_experiment_arms as arms};

    let env_mode = std::env::var("LOOM_MODEL_EXPERIMENT").ok();
    let env_canary = std::env::var("LOOM_MODEL_EXPERIMENT_CANARY").ok();
    let env_fraction = std::env::var(arms::BUDGET_FRACTION_ENV).ok();

    /// Resolve the roster + budget fraction from an already-loaded config,
    /// printing every warning on stderr (the `sweep-experiment` convention).
    /// Absent config, this is the built-in A/B pair at fraction 1.0 — i.e.
    /// byte-for-byte the pre-#9122 behavior.
    fn arm_plan(
        cfg: &serde_json::Value,
        env_fraction: Option<&str>,
    ) -> (script_helpers::sweep_experiment_arms::ExperimentArms, f64) {
        use script_helpers::sweep_experiment_arms as arms;
        let (roster, mut warnings) = arms::ExperimentArms::load(cfg);
        let (fraction, fraction_warnings) = arms::resolve_budget_fraction(env_fraction, cfg);
        warnings.extend(fraction_warnings);
        for w in warnings {
            eprintln!("[sweep-experiment] WARNING: {w}");
        }
        (roster, fraction)
    }

    match action {
        SweepExperimentAction::ResolveMode { config } => {
            let cfg = model_tiers::load_config(config.as_deref().map(Path::new));
            let (mode, warnings) = se::resolve_effective_mode_default(
                env_mode.as_deref(),
                env_canary.as_deref(),
                &cfg,
                None,
            );
            for w in warnings {
                eprintln!("[sweep-experiment] WARNING: {w}");
            }
            println!("{mode}");
            Ok(())
        }
        SweepExperimentAction::AssignArm {
            issue,
            format,
            resolve,
            config,
        } => {
            // The config is now loaded UNCONDITIONALLY (#9122), not only under
            // `--resolve`: the arm roster and the budget fraction both live in
            // it. An unconfigured repo still yields the built-in A/B pair at
            // fraction 1.0, so the output is unchanged.
            let cfg = model_tiers::load_config(config.as_deref().map(Path::new));
            let (roster, fraction) = arm_plan(&cfg, env_fraction.as_deref());
            let assigned = arms::in_experiment(issue, fraction).then(|| roster.assign(issue));
            // The default prints the logical alias (Arm A -> `opus`), which the
            // arm identity and the shell test key off. `--resolve` prints the
            // concrete ID the #3982 tier map resolves that alias to.
            let model = assigned.map(|a| {
                if resolve {
                    a.resolved_model(&cfg)
                } else {
                    a.model.clone()
                }
            });
            let arm = assigned.map(|a| a.id.as_str());
            if format == "json" {
                println!(
                    "{}",
                    serde_json::json!({
                        "issue": issue,
                        "arm": arm,
                        "model": model,
                        "in_experiment": arm.is_some(),
                    })
                );
            } else {
                // Sampled OUT of the experiment => `none -`: no arm, no forced
                // model, caller proceeds with unmodified tier-3
                // resolution (the `observe`-mode null-arm convention).
                println!("{} {}", arm.unwrap_or("none"), model.as_deref().unwrap_or("-"));
            }
            Ok(())
        }
        SweepExperimentAction::Banner { issue, config } => {
            let cfg = model_tiers::load_config(config.as_deref().map(Path::new));
            let (raw_mode, _) = se::resolve_raw_mode(env_mode.as_deref(), &cfg);
            let (mode, warnings) = se::resolve_effective_mode_default(
                env_mode.as_deref(),
                env_canary.as_deref(),
                &cfg,
                None,
            );
            for w in warnings {
                eprintln!("[sweep-experiment] WARNING: {w}");
            }
            let mut arm: Option<String> = None;
            let mut model = String::new();
            let mut canary_source: Option<String> = None;
            if mode == "experiment" {
                let (roster, fraction) = arm_plan(&cfg, env_fraction.as_deref());
                let (_ok, source, _w) = se::evaluate_canary_default(env_canary.as_deref(), None);
                canary_source = source.map(|s| s.label().to_string());
                if !arms::in_experiment(issue, fraction) {
                    // Sampled out by the budget cap (#9122): no arm, no
                    // forcing, its own loud banner.
                    println!(
                        "{}",
                        arms::format_budget_skip_banner(issue, fraction, canary_source.as_deref())
                    );
                    return Ok(());
                }
                let assigned = roster.assign(issue);
                arm = Some(assigned.id.clone());
                model = assigned.model.clone();
            } else if raw_mode == "experiment" {
                // Requested experiment but downgraded to observe — canary
                // unconfirmed.
                canary_source = Some("unconfirmed".to_string());
            }
            println!(
                "{}",
                se::format_banner(
                    &mode,
                    issue,
                    arm.as_deref(),
                    if model.is_empty() {
                        None
                    } else {
                        Some(model.as_str())
                    },
                    canary_source.as_deref(),
                )
            );
            Ok(())
        }
        SweepExperimentAction::Record {
            issue,
            phase,
            role,
            model,
            mode,
            arm,
            attempt,
            story_points,
            verdict,
            cycle_count,
            pr,
            effort,
            agent_id,
            transcript,
            in_tok,
            out_tok,
            token_fidelity,
            stats_file,
            quiet,
        } => {
            let record = se::build_record(
                &se::RecordFields {
                    issue,
                    phase: &phase,
                    role: &role,
                    model: model.as_deref(),
                    mode: &mode,
                    arm: arm.as_deref(),
                    attempt,
                    story_points,
                    judge_verdict: verdict.as_deref(),
                    cycle_count,
                    pr,
                    effort: effort.as_deref(),
                    agent_id: agent_id.as_deref(),
                    transcript: transcript.as_deref(),
                    in_tok,
                    out_tok,
                    token_fidelity: &token_fidelity,
                },
                &script_helpers::now_iso(),
            );
            se::append_record(&record, stats_file.as_deref())?;
            if !quiet {
                println!("{record}");
            }
            Ok(())
        }
        SweepExperimentAction::Fleet(action) => action.run(),
        SweepExperimentAction::Harvest {
            stats_file,
            archive_dir,
            format,
        } => {
            let raw = archive_dir.or_else(|| std::env::var("LOOM_TRANSCRIPT_ARCHIVE").ok());
            let archive = se::normalize_archive_dir(raw.as_deref());
            let report = se::harvest(stats_file.as_deref(), archive.as_deref());
            if format == "json" {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", se::format_harvest_text(&report));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::whole_cli_parse::try_parse_cli;
    use crate::Commands;

    /// #11665: `resolve-model` keeps its alias / `--task-alias` /
    /// `--downgrade` / `--generation` surface unchanged.
    #[test]
    fn resolve_model_keeps_its_alias_surface() {
        for argv in [
            &["loom-daemon", "resolve-model", "opus"][..],
            &[
                "loom-daemon",
                "resolve-model",
                "claude-opus-5",
                "--task-alias",
            ],
            &["loom-daemon", "resolve-model", "opus", "--downgrade"],
            &[
                "loom-daemon",
                "resolve-model",
                "opus",
                "--generation",
                "--config",
                "x.json",
            ],
        ] {
            let cli = try_parse_cli(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(matches!(cli.command, Some(Commands::ResolveModel { .. })), "{argv:?}");
        }
    }

    /// #11665: tier-2.5 routing and the Jev tier shadow are deleted, so their
    /// entry points are unknown to clap rather than silently accepted.
    #[test]
    fn tier_routing_and_jev_tier_entry_points_are_rejected() {
        for argv in [
            &["loom-daemon", "resolve-model", "--tier", "routine"][..],
            &[
                "loom-daemon",
                "resolve-model",
                "opus",
                "--runtime",
                "claude",
            ],
            &["loom-daemon", "resolve-model"],
            &["loom-daemon", "jev-tier", "1"],
        ] {
            assert!(try_parse_cli(argv).is_err(), "{argv:?} must be rejected");
        }
        // The separate Champion merge-risk shadow stays (#8545).
        assert!(try_parse_cli(&["loom-daemon", "jev-merge-risk", "1"]).is_ok());
    }
}
