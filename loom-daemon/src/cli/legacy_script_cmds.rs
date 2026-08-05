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

use crate::{CheckpointAction, SweepExperimentAction};

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

/// `loom-daemon resolve-model` — backs `resolve-model.sh` / `resolve-tier-model.sh`.
///
/// Exit codes mirror the retired Python CLI exactly: `0` on success, `2` when
/// neither a model nor `--tier` was supplied, and `3` with NO output when
/// `--tier` / `--task-alias` has no mapping so the caller keeps its own
/// precedence chain.
pub(crate) fn handle_resolve_model_command(
    model: Option<&str>,
    config: Option<&str>,
    generation: bool,
    task_alias: bool,
    tier: Option<&str>,
    runtime: &str,
) -> Result<()> {
    use script_helpers::model_tiers;

    let cfg = model_tiers::load_config(config.map(Path::new));

    // Complexity-tier mode (#4238). Checked before the positional model, which
    // the Python parser also treated as optional in this mode.
    if let Some(tier) = tier {
        let env_override = std::env::var("LOOM_SWEEP_OPTIMIZATION").ok();
        let resolved =
            model_tiers::resolve_tier_model(Some(tier), runtime, &cfg, env_override.as_deref());
        if resolved.is_empty() {
            std::process::exit(3);
        }
        println!("{resolved}");
        return Ok(());
    }

    let Some(model) = model else {
        eprintln!("loom-daemon resolve-model: error: a model argument or --tier is required");
        std::process::exit(2);
    };

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
    use script_helpers::{model_tiers, sweep_experiment as se};

    let env_mode = std::env::var("LOOM_MODEL_EXPERIMENT").ok();
    let env_canary = std::env::var("LOOM_MODEL_EXPERIMENT_CANARY").ok();

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
            complexity,
            format,
            resolve,
            config,
        } => {
            let arm = se::assign_arm(issue, complexity.as_deref());
            // The default prints the logical alias (Arm A -> `opus`), which the
            // arm identity and the shell test key off. `--resolve` prints the
            // concrete ID the #3982 tier map resolves that alias to.
            let model = if resolve {
                let cfg = model_tiers::load_config(config.as_deref().map(Path::new));
                se::resolved_arm_model(arm, &cfg)
            } else {
                se::arm_model(arm)
            };
            if format == "json" {
                println!(
                    "{}",
                    serde_json::json!({
                        "issue": issue,
                        "complexity": se::normalize_complexity(complexity.as_deref()),
                        "arm": arm,
                        "model": model,
                    })
                );
            } else {
                println!("{arm} {model}");
            }
            Ok(())
        }
        SweepExperimentAction::Banner {
            issue,
            complexity,
            config,
        } => {
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
            let mut arm: Option<&str> = None;
            let mut model = String::new();
            let mut canary_source: Option<String> = None;
            if mode == "experiment" {
                let assigned = se::assign_arm(issue, complexity.as_deref());
                arm = Some(assigned);
                model = se::arm_model(assigned);
                let (_ok, source, _w) = se::evaluate_canary_default(env_canary.as_deref(), None);
                canary_source = source.map(|s| s.label().to_string());
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
                    arm,
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
            complexity,
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
                    complexity: complexity.as_deref(),
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
