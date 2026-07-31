//! Miscellaneous `loom-daemon` subcommand handlers with no obvious sibling
//! grouping (Issue #4712 — split out of `main.rs`): `init`,
//! `update-gitignore`, `validate`, `sweep-outcomes`.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};

use loom_daemon::role_validation;

/// Handle `loom-daemon init [PATH]`: initialize (or, for the Loom source repo
/// itself, validate) a Loom workspace. Extracted from `handle_cli_command`'s
/// `Commands::Init` match arm (Issue #4712).
pub(crate) fn run_init(
    workspace: String,
    defaults: String,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    let workspace_path = std::path::Path::new(&workspace);
    let absolute_workspace = if workspace_path.is_absolute() {
        workspace_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace_path)
    };

    let workspace_str = absolute_workspace
        .to_str()
        .ok_or_else(|| anyhow!("Invalid workspace path"))?;

    if dry_run {
        println!("Dry run mode - no changes will be made\n");
        println!("Would initialize Loom workspace:");
        println!("  Workspace: {workspace_str}");
        println!("  Defaults:  {defaults}");
        println!("  Force:     {force}");
        println!("\nActions that would be performed:");
        println!("  1. Validate {workspace_str} is a git repository");
        println!("  2. Copy .loom/ configuration from {defaults}");
        println!("  3. Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .github/)");
        println!("  4. Update .gitignore with Loom ephemeral patterns");
        return Ok(());
    }

    println!("Initializing Loom workspace...");
    println!("  Workspace: {workspace_str}");
    println!("  Defaults:  {defaults}");

    match loom_daemon::init::initialize_workspace(workspace_str, &defaults, force) {
        Ok(report) => {
            if report.is_self_install {
                println!("\nLoom source repository detected!");
                println!("\nMode: Validation only (self-installation)");
                println!("\nValidating configuration...");

                if let Some(ref validation) = report.validation {
                    println!(
                        "  .loom/roles/    - {} role definitions found",
                        validation.roles_found.len()
                    );
                    println!(
                        "  .loom/scripts/  - {} scripts found",
                        validation.scripts_found.len()
                    );
                    println!(
                        "  .claude/commands/loom/ - {} slash commands found",
                        validation.commands_found.len()
                    );

                    if validation.has_claude_md {
                        println!("  CLAUDE.md       - Present");
                    } else {
                        println!("  CLAUDE.md       - Missing");
                    }

                    // AGENTS.md is not mandatory (see ValidationReport::has_agents_md),
                    // so its absence is informational only, never an issue.
                    if validation.has_agents_md {
                        println!("  AGENTS.md       - Present");
                    } else {
                        println!("  AGENTS.md       - Not present (optional)");
                    }

                    if validation.has_labels_yml {
                        println!("  .github/labels.yml - Present");
                    } else {
                        println!("  .github/labels.yml - Missing");
                    }

                    if validation.issues.is_empty() {
                        println!("\nLoom source repository is properly configured");
                    } else {
                        println!("\nIssues found:");
                        for issue in &validation.issues {
                            println!("  - {issue}");
                        }
                    }

                    println!("\nRoles found: {}", validation.roles_found.join(", "));
                }

                println!("\nSelf-installation skips file copying to prevent data loss.");
                println!("   The Loom repo's .loom/ directory IS the source of truth.");
                println!("\nTo use Loom orchestration:");
                println!("  - Open Claude Code terminals with /loom:builder, /loom:judge, etc.");
                println!("  - Or start the daemon: ./.loom/scripts/cli/loom-daemon-start.sh");

                return Ok(());
            }

            println!("\nLoom workspace initialized successfully!");
            println!("\nFiles installed:");
            println!("  .loom/          - Configuration directory");
            println!("  .loom/config.json - Terminal configuration");
            println!("  .loom/roles/    - Agent role definitions");
            println!("  CLAUDE.md       - AI context documentation (Claude Code)");
            println!("  AGENTS.md       - AI context documentation (OpenAI Codex)");
            println!("  .claude/        - Claude Code configuration");
            println!("  .github/        - GitHub labels and issue templates");
            println!("  .gitignore      - Updated with Loom patterns");

            if !report.added.is_empty()
                || !report.preserved.is_empty()
                || !report.removed.is_empty()
            {
                println!();
                if !report.added.is_empty() {
                    println!("Files added ({}):", report.added.len());
                    for file in &report.added {
                        println!("  + {file}");
                    }
                }
                if !report.preserved.is_empty() {
                    println!("\nFiles preserved ({}):", report.preserved.len());
                    for file in &report.preserved {
                        println!("  = {file}");
                    }
                    println!("\n  Preserved files were not overwritten. To update them,");
                    println!("     delete them and run install again, or use --force.");
                }
                if !report.updated.is_empty() {
                    println!("\nFiles updated ({}):", report.updated.len());
                    for file in &report.updated {
                        println!("  ~ {file}");
                    }
                }
                if !report.removed.is_empty() {
                    println!("\nFiles removed ({}):", report.removed.len());
                    for file in &report.removed {
                        println!("  - {file}");
                    }
                }
                if !report.verification_failures.is_empty() {
                    eprintln!(
                        "\nUnexpected file divergence ({}):",
                        report.verification_failures.len()
                    );
                    for failure in &report.verification_failures {
                        eprintln!("  {failure}");
                    }
                    eprintln!("\n  These files were copied from defaults but their installed");
                    eprintln!("  contents differ from the source. This is informational only —");
                    eprintln!("  installation completed. Inspect the listed files to confirm");
                    eprintln!("  they look correct.");
                }
            }

            println!("\nNext steps:");
            println!(
                "  1. Commit the changes: git add -A && git commit -m 'Add Loom configuration'"
            );
            println!("  2. Choose your workflow:");
            println!("     Manual Mode (recommended to start):");
            println!("       cd {workspace_str} && claude");
            println!("       Then use /loom:builder, /loom:judge, or other role commands");
            println!("     Daemon Mode (autonomous orchestration):");
            println!("       cd {workspace_str} && ./.loom/scripts/cli/loom-daemon-start.sh");
            println!("       Then in Claude Code: /loom:loom");
            Ok(())
        }
        Err(e) => {
            eprintln!("\nFailed to initialize workspace: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle CLI commands (init, stats, validate modes)
/// Handle `loom-daemon update-gitignore [PATH]` (Issue #4280).
///
/// Rewrites only the marker-delimited Loom-managed `.gitignore` block, converging
/// it on the current `EPHEMERAL_PATTERNS` set. This is the standalone refresh
/// entry point `resync-installed.sh` calls so existing consumer installs pick up
/// newly-ignored runtime paths without a full `init`. Idempotent: a workspace
/// already carrying the current block is a byte-for-byte no-op.
pub(crate) fn handle_update_gitignore_command(workspace: &str) -> Result<()> {
    let workspace_path = std::path::Path::new(workspace);
    let absolute_workspace = if workspace_path.is_absolute() {
        workspace_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace_path)
    };

    loom_daemon::init::update_gitignore(&absolute_workspace)
        .map_err(|e| anyhow!("Failed to update .gitignore: {e}"))?;

    println!(
        "Refreshed the Loom-managed .gitignore block in {}",
        absolute_workspace.display()
    );
    Ok(())
}

pub(crate) fn handle_validate_command(
    workspace: &str,
    format: &str,
    strict: bool,
    verbose: bool,
) -> Result<()> {
    use loom_daemon::config_resolver;
    use role_validation::{format_validation_result, validate_from_config, ValidationMode};

    let workspace_path = std::path::Path::new(workspace);
    let absolute_workspace = if workspace_path.is_absolute() {
        workspace_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace_path)
    };

    // #4059: resolve the effective config through the full tier chain rather
    // than reading the legacy `.loom/config.json` directly. Under tiering,
    // "the legacy file is absent" no longer implies "there is no config" — a
    // repo may configure `terminals` entirely from `.loom-project/project.json`
    // or `.loom-local/local.json`.
    let config = config_resolver::resolve_effective_config(&absolute_workspace);

    // The tiers searched, in precedence order — named in every "not found" error
    // (Finding 5: both text and json branches).
    let mut searched_tiers: Vec<String> = Vec::new();
    if let Some(defaults) = config_resolver::private_defaults_path() {
        searched_tiers.push(defaults.display().to_string());
    }
    searched_tiers.push(
        absolute_workspace
            .join(config_resolver::LEGACY_CONFIG_REL)
            .display()
            .to_string(),
    );
    searched_tiers.push(
        absolute_workspace
            .join(config_resolver::PROJECT_CONFIG_REL)
            .display()
            .to_string(),
    );
    searched_tiers.push(
        absolute_workspace
            .join(config_resolver::LOCAL_CONFIG_REL)
            .display()
            .to_string(),
    );

    // Retargeted precondition (#4059) — stated verbatim for #4062 to mirror:
    //   "A workspace is validatable iff resolve_effective_config(workspace)
    //    yields a `terminals` array with at least one element; otherwise the
    //    command exits 1, naming every tier it searched."
    let has_terminals = config
        .get("terminals")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|arr| !arr.is_empty());

    if !has_terminals {
        if format == "json" {
            let err = serde_json::json!({
                "error": "No Loom config with a non-empty terminals array found in any tier",
                "searched": searched_tiers,
            });
            println!("{}", serde_json::to_string(&err)?);
        } else {
            eprintln!(
                "Error: No Loom config with a non-empty `terminals` array found in any tier."
            );
            eprintln!("\nSearched (lowest to highest precedence):");
            for tier in &searched_tiers {
                eprintln!("  - {tier}");
            }
            eprintln!("\nMake sure you're in a Loom workspace or specify the path:");
            eprintln!("  loom-daemon validate /path/to/workspace");
        }
        std::process::exit(1);
    }

    let mode = if strict {
        ValidationMode::Strict
    } else {
        ValidationMode::Warn
    };

    let result = validate_from_config(&config, mode);

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        if verbose {
            println!("\nValidating role configuration...");
            println!("  Workspace: {}", absolute_workspace.display());
            println!();
        }

        let output = format_validation_result(&result, verbose);
        if !output.is_empty() {
            print!("{output}");
        }

        if result.warnings.is_empty() && result.errors.is_empty() {
            println!("All role dependencies are satisfied.");
        }
    }

    if !result.errors.is_empty() {
        std::process::exit(1);
    } else if !result.warnings.is_empty() && strict {
        std::process::exit(2);
    }

    Ok(())
}

/// `loom-daemon sweep-outcomes` — local inspection path for the durable
/// `sweep.outcome` telemetry journal (Issue #4704, absorbs #4137). Purely
/// file-based, like `calibrate` — no running daemon required. With no
/// `--records` flag, prints the "success rate and median duration by model"
/// summary #4137 AC4 asked for; `--records` lists individual outcome records
/// instead (newest first), both respecting `--model`/`--result`/`--limit`.
pub(crate) fn handle_sweep_outcomes_command(
    workspace: &str,
    model_filter: Option<&str>,
    result_filter: Option<&str>,
    records_mode: bool,
    limit: Option<usize>,
    json: bool,
) -> Result<()> {
    use loom_daemon::sweep_outcomes;
    use loom_daemon::telemetry;
    use loom_daemon::worktree_ops::repo;

    let repo_root = repo::resolve_repo_root(workspace)?;
    let path = sweep_outcomes::default_outcome_telemetry_path(&repo_root);

    let result_filter: Option<telemetry::SweepResult> =
        result_filter.map(parse_sweep_result_arg).transpose()?;

    let mut entries: Vec<(DateTime<Utc>, telemetry::SweepOutcomeRecord)> =
        sweep_outcomes::read_all_outcome_telemetry(&path)
            .into_iter()
            .filter_map(|envelope| match envelope.record {
                telemetry::TelemetryRecord::SweepOutcome(record) => {
                    Some((envelope.emitted_at, record))
                }
                _ => None,
            })
            .filter(|(_, record)| {
                let model_key = record.model.as_deref().unwrap_or("default");
                let model_ok = model_filter.is_none() || model_filter == Some(model_key);
                let result_ok = result_filter.is_none() || result_filter == Some(record.result);
                model_ok && result_ok
            })
            .collect();

    // Newest first — `emitted_at` is the daemon-stamped envelope time, not
    // file position, so this is correct even across a rotation boundary.
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    if let Some(limit) = limit {
        entries.truncate(limit);
    }

    if records_mode {
        if json {
            let records: Vec<&telemetry::SweepOutcomeRecord> =
                entries.iter().map(|(_, r)| r).collect();
            println!("{}", serde_json::to_string_pretty(&records)?);
        } else if entries.is_empty() {
            println!("No sweep.outcome telemetry records found at {}", path.display());
        } else {
            println!(
                "{:<8} {:<26} {:<10} {:<10} {:>10}  {:<6} EMITTED_AT",
                "ISSUE", "SWEEP_ID", "MODEL", "RESULT", "DURATION", "PR"
            );
            for (emitted_at, record) in &entries {
                let model = record.model.as_deref().unwrap_or("default");
                let pr = record
                    .pr_number
                    .map_or_else(|| "-".to_string(), |n| format!("#{n}"));
                println!(
                    "{:<8} {:<26} {:<10} {:<10} {:>9}s  {:<6} {}",
                    record.issue,
                    record.sweep_id,
                    model,
                    format!("{:?}", record.result).to_lowercase(),
                    record.total_duration_sec,
                    pr,
                    emitted_at.to_rfc3339(),
                );
            }
        }
        return Ok(());
    }

    let records: Vec<telemetry::SweepOutcomeRecord> = entries.into_iter().map(|(_, r)| r).collect();
    let summary = sweep_outcomes::summarize_by_model(&records);
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else if summary.is_empty() {
        println!("No sweep.outcome telemetry records found at {}", path.display());
    } else {
        println!("{:<12} {:>8} {:>10} {:>14}", "MODEL", "TOTAL", "SUCCESS%", "MEDIAN_SEC");
        for group in &summary {
            println!(
                "{:<12} {:>8} {:>9.1}% {:>14}",
                group.model,
                group.total,
                group.success_rate * 100.0,
                group.median_duration_sec
            );
        }
    }
    Ok(())
}

/// Parse `loom-daemon sweep-outcomes --result <VALUE>` into a
/// [`loom_daemon::telemetry::SweepResult`]. Case-insensitive; accepts both
/// `cancelled` and `canceled` spellings.
fn parse_sweep_result_arg(s: &str) -> Result<loom_daemon::telemetry::SweepResult> {
    use loom_daemon::telemetry::SweepResult;
    match s.to_ascii_lowercase().as_str() {
        "success" => Ok(SweepResult::Success),
        "failure" => Ok(SweepResult::Failure),
        "cancelled" | "canceled" => Ok(SweepResult::Cancelled),
        "blocked" => Ok(SweepResult::Blocked),
        other => bail!(
            "unknown --result value {other:?} (expected one of: success, failure, cancelled, blocked)"
        ),
    }
}
