//! `loom-daemon ci-telemetry` (Issue #8824) — GitHub Actions run/job
//! telemetry for one forge org. Reached through the flattened
//! [`super::telemetry::TelemetryCommand`], so it is top-level.
//!
//! The logic lives in [`loom_daemon::ci_telemetry`]; this file is argument
//! parsing, exit codes and rendering only.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use chrono::Utc;

use loom_daemon::ci_telemetry::{
    self, api::GhCliApi, export, journal::Journal, ledger::Ledger, poll, state,
};

/// Exit code for a cycle that did not run because it must wait: another
/// cycle holds the lock, or the org is inside a rate-limit backoff
/// (`EX_TEMPFAIL`).
pub(crate) const EXIT_TEMPFAIL: i32 = 75;

/// Capture GitHub Actions runs/jobs of an org as telemetry (phase 1, #8824).
///
/// `--once` runs one poll cycle: discovers the org's repos (ETag-cached),
/// lists each repo's runs created since its watermark, and for every
/// completed, not-yet-recorded run attempt records one `ci.run` plus one
/// `ci.job` per job — each exactly once, ever (the ledger at
/// `.loom/state/ci-telemetry/seen.jsonl` is the commit point). Records are
/// written to `.loom/logs/ci-telemetry.jsonl` regardless of exporter
/// configuration; a daemon with `observability` enabled exports them.
///
/// Exit codes for `--once`: 0 = cycle completed cleanly; 1 = failed (the
/// named reason is printed — discovery/io failure, or one or more repos
/// failed); 75 = skipped, must wait (busy lock, or org-wide rate-limit
/// backoff).
///
/// With `autonomous.ciTelemetry.logCaptureEnabled` (#8825) each completed
/// job's full log is additionally captured as chunked `ci.job.log` records,
/// capped per job by `logCaptureMaxBytes` (default 5 MiB) and redacted at the
/// OTLP gateway, not here.
///
/// `status` reports ledger size, per-repo watermarks, records
/// emitted/exported, log capture (done/pending/failed per repo, last fetch
/// age), and health: never-polled / ok (+age) / stale / failing (+last error).
///
/// The periodic daemon poller is `autonomous.ciTelemetry.enabled` (default
/// false); `--once` runs regardless of that flag. See
/// `.loom/docs/ci-observability.md`.
#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct CiTelemetryArgs {
    #[command(subcommand)]
    action: Option<CiTelemetryAction>,

    /// Run exactly one poll cycle over the configured org, then exit.
    #[arg(long)]
    once: bool,

    /// Org to poll for this run (default: `autonomous.ciTelemetry.org`,
    /// `$LOOM_CI_TELEMETRY_ORG`, else `2amlogic`).
    #[arg(long, value_name = "ORG")]
    org: Option<String>,

    /// Loom workspace whose `.loom/` holds the ledger/journal (default: the
    /// repository enclosing the current directory).
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

#[derive(clap::Subcommand)]
enum CiTelemetryAction {
    /// Ledger size, per-repo watermarks, records emitted/exported, health.
    Status {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,

        /// Loom workspace (default: the repository enclosing the cwd).
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
    },
}

fn resolve_root(workspace: Option<&Path>) -> Result<PathBuf> {
    let found = match workspace {
        Some(path) => loom_daemon::repo_root::find_repo_root(path),
        None => loom_daemon::repo_root::find_repo_root_from_cwd(),
    };
    found.ok_or_else(|| {
        anyhow!("not inside a Loom workspace (no enclosing repo with a .loom/ directory)")
    })
}

impl CiTelemetryArgs {
    pub(crate) fn run(self) -> Result<()> {
        match self.action {
            Some(CiTelemetryAction::Status { json, workspace }) => {
                let root = resolve_root(workspace.as_deref())?;
                status(&root, json)
            }
            None if self.once => {
                let root = resolve_root(self.workspace.as_deref())?;
                std::process::exit(once(&root, self.org));
            }
            None => Err(anyhow!(
                "nothing to do: pass --once to run one poll cycle, or `status` (see --help)"
            )),
        }
    }
}

fn once(root: &Path, org: Option<String>) -> i32 {
    let mut resolved = ci_telemetry::resolve(&ci_telemetry::read_config(root));
    if let Some(org) = org {
        resolved.org = org;
    }
    for refusal in &resolved.refused_exclusions {
        eprintln!(
            "ci-telemetry: warning — excludedRepos entry {refusal} (the repo is still polled)"
        );
    }
    let ctx = poll::CycleContext::new(root, &resolved);
    match poll::run_cycle(&ctx, &GhCliApi::from_env()) {
        Ok(report) if report.repo_errors.is_empty() => {
            println!("ci-telemetry: ok — org {}: {}", resolved.org, report.summary());
            0
        }
        Ok(report) => {
            eprintln!("ci-telemetry: failed — org {}: {}", resolved.org, report.summary());
            1
        }
        Err(
            error @ (poll::CycleError::Busy
            | poll::CycleError::BackingOff { .. }
            | poll::CycleError::RateLimited { .. }),
        ) => {
            eprintln!("ci-telemetry: skipped — {error}");
            EXIT_TEMPFAIL
        }
        Err(error) => {
            eprintln!("ci-telemetry: failed — {error}");
            1
        }
    }
}

fn status(root: &Path, json: bool) -> Result<()> {
    let resolved = ci_telemetry::resolve(&ci_telemetry::read_config(root));
    let dir = ci_telemetry::state_dir(root);
    let poll_status = state::load_status(&dir);
    let health = state::classify(&poll_status, Utc::now(), resolved.interval_secs);
    let ledger = Ledger::open_read_only(dir.join("seen.jsonl"))?;
    let ledger_bytes = std::fs::metadata(ledger.path()).map_or(0, |m| m.len());
    let counts = Journal::reader(ci_telemetry::journal_path(root)).counts()?;
    let cursor = export::load_cursor(root);
    let pending_export = export::pending_count(root);
    let gate = ci_telemetry::log_capture_gate(&resolved);
    let log_counts = ledger.log_counts();
    let log_counts_by_repo = ledger.log_counts_by_repo();
    let last_log_failure = ledger.last_log_failure();
    let last_log_fetch_age = poll_status
        .last_log_fetch_at
        .map(|at| (Utc::now() - at).num_seconds().max(0));

    let health_json = match &health {
        state::Health::NeverPolled => serde_json::json!({ "state": "never-polled" }),
        state::Health::Ok { age_secs } => {
            serde_json::json!({ "state": "ok", "last_ok_age_secs": age_secs })
        }
        state::Health::Stale { age_secs } => {
            serde_json::json!({ "state": "stale", "last_ok_age_secs": age_secs })
        }
        state::Health::Failing {
            since,
            error,
            consecutive_failures,
            backoff_until,
        } => serde_json::json!({
            "state": "failing",
            "last_ok_at": since,
            "last_error": error,
            "consecutive_failures": consecutive_failures,
            "backoff_until": backoff_until,
        }),
        state::Health::Refused {
            reason,
            no_captain_declared,
            since,
        } => serde_json::json!({
            "state": "refused",
            "reason": reason,
            "no_captain_declared": no_captain_declared,
            "since": since,
        }),
    };
    if json {
        let value = serde_json::json!({
            "org": resolved.org,
            "daemon_poller_enabled": resolved.enabled,
            "interval_secs": resolved.interval_secs,
            "log_capture": {
                "state": gate.as_str(),
                "max_bytes_per_job": resolved.log_capture_max_bytes,
                "excluded_repos": resolved.log_capture_excluded_repos,
                "done": log_counts.done,
                "pending": log_counts.pending,
                "failed": log_counts.failed,
                "by_repo": log_counts_by_repo,
                "last_fetch_at": poll_status.last_log_fetch_at,
                "last_fetch_age_secs": last_log_fetch_age,
                "last_failure": last_log_failure.as_ref().map(|(repo, job_id, attempts, error)| {
                    serde_json::json!({
                        "repo": repo,
                        "job_id": job_id,
                        "attempts": attempts,
                        "error": error,
                    })
                }),
                "chunks_emitted": counts.job_log_chunks,
            },
            "excluded_repos": resolved.excluded_repos,
            "refused_exclusions": resolved.refused_exclusions,
            "health": health_json,
            "last_attempt_at": poll_status.last_attempt_at,
            "last_ok_at": poll_status.last_ok_at,
            "last_cycle": poll_status.last_cycle,
            "ledger": {
                "path": ledger.path(),
                "units": ledger.unit_count(),
                "bytes": ledger_bytes,
                "pending_units": ledger.pending().len(),
                "watermarks": ledger.watermarks(),
            },
            "journal": {
                "path": ci_telemetry::journal_path(root),
                "runs_emitted": counts.runs,
                "jobs_emitted": counts.jobs,
                "envelopes": counts.envelopes,
            },
            "export": {
                "exported": cursor.exported,
                "pending": pending_export,
            },
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!("ci-telemetry — org {}", resolved.org);
    let health_line = match &health {
        state::Health::NeverPolled => "never polled on this host".to_string(),
        state::Health::Ok { age_secs } => format!("ok — last successful poll {age_secs}s ago"),
        state::Health::Stale { age_secs } => format!(
            "STALE — last successful poll {age_secs}s ago (more than 3x the {}s interval)",
            resolved.interval_secs
        ),
        state::Health::Failing {
            since,
            error,
            consecutive_failures,
            backoff_until,
        } => {
            let mut line = format!(
                "FAILING — {consecutive_failures} consecutive failure(s); last ok: {}; last error: {error}",
                since.map_or_else(|| "never".to_string(), |at| at.to_rfc3339())
            );
            if let Some(until) = backoff_until {
                line.push_str(&format!("; backing off until {}", until.to_rfc3339()));
            }
            line
        }
        state::Health::Refused {
            reason,
            no_captain_declared,
            since,
        } => format!(
            "{} since {} — {reason}",
            if *no_captain_declared {
                "REFUSED (no fleet.captain declared: the daemon poller runs on no host)"
            } else {
                "refused (another host is the fleet captain)"
            },
            since.to_rfc3339()
        ),
    };
    println!("  health:         {health_line}");
    println!(
        "  daemon poller:  {} (interval {}s)",
        if resolved.enabled {
            "enabled"
        } else {
            "disabled (autonomous.ciTelemetry.enabled=false)"
        },
        resolved.interval_secs
    );
    if gate.is_on() {
        println!(
            "  log capture:    on (cap {} bytes/job) — {} job(s) done, {} pending, {} FAILED",
            resolved.log_capture_max_bytes, log_counts.done, log_counts.pending, log_counts.failed
        );
        println!(
            "  last log fetch: {}",
            last_log_fetch_age
                .map_or_else(|| "never on this host".to_string(), |age| format!("{age}s ago"),)
        );
        for (repo, counts) in &log_counts_by_repo {
            println!(
                "    {repo}: {} done, {} pending, {} failed",
                counts.done, counts.pending, counts.failed
            );
        }
        if let Some((repo, job_id, attempts, error)) = &last_log_failure {
            println!("  last log error: {repo} job {job_id} after {attempts} attempt(s): {error}");
        }
        for exclusion in &resolved.log_capture_excluded_repos {
            println!(
                "  logs excluded:  {} — policy exception, reason: {} (records/metrics still captured)",
                exclusion.repo, exclusion.reason
            );
        }
    } else {
        println!("  log capture:    off (autonomous.ciTelemetry.logCaptureEnabled=false)");
    }
    for exclusion in &resolved.excluded_repos {
        println!(
            "  excluded:       {} — policy exception, reason: {}",
            exclusion.repo, exclusion.reason
        );
    }
    for refusal in &resolved.refused_exclusions {
        println!("  REFUSED:        excludedRepos entry {refusal} (still polled)");
    }
    println!(
        "  ledger:         {} unit(s), {} byte(s), {} pending — {}",
        ledger.unit_count(),
        ledger_bytes,
        ledger.pending().len(),
        ledger.path().display()
    );
    println!(
        "  emitted:        {} run(s), {} job(s), {} log chunk(s) in {}",
        counts.runs,
        counts.jobs,
        counts.job_log_chunks,
        ci_telemetry::journal_path(root).display()
    );
    println!(
        "  exported:       {} envelope(s) offered, {pending_export} pending",
        cursor.exported
    );
    if ledger.watermarks().is_empty() {
        println!("  watermarks:     (none)");
    } else {
        println!("  watermarks:");
        for (repo, at) in ledger.watermarks() {
            println!("    {repo}: {}", at.to_rfc3339());
        }
    }
    Ok(())
}
