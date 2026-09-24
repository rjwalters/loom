//! `loom-daemon tmpfs-scratch-gc` handler (Issue #8512) — CLI front-end for
//! [`loom_daemon::tmpfs_reclaim`]. Purely file-based; does not require a
//! running daemon (mirrors `TargetDirGc`).
//!
//! The handler resolves the **same** settings the daemon's own reaper pass
//! resolves — `autonomous.tmpfsScratchGc.*` from the repo the command is run
//! in, under the usual `env > config > default` precedence — so a manual run
//! can never act on a broader name-pattern set than the configured automatic
//! one. An explicit `--staleness-secs` is the one deliberate exception: a flag
//! the operator typed wins over everything.

use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use serde_json::json;

use loom_daemon::tmpfs_reclaim::{self, TmpfsReclaimReport};

pub(crate) fn handle_tmpfs_scratch_gc_command(
    staleness_secs: Option<u64>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = tmpfs_reclaim::read_tmpfs_reclaim_config(&repo_root);
    let staleness_secs =
        staleness_secs.unwrap_or_else(|| tmpfs_reclaim::resolve_staleness_secs(&config));
    let name_patterns = tmpfs_reclaim::resolve_name_patterns(&config);

    let report = tmpfs_reclaim::run(staleness_secs, &name_patterns, dry_run, Utc::now());

    if json {
        print_json(&report, &name_patterns);
    } else {
        print_human(&report, &name_patterns);
    }
    Ok(())
}

fn print_human(report: &TmpfsReclaimReport, name_patterns: &[String]) {
    println!("Staleness:      {} second(s)", report.staleness_secs);
    println!("Name patterns:  {}", name_patterns.join(", "));
    println!(
        "Mode:           {}",
        if report.dry_run {
            "DRY RUN"
        } else {
            "REAL RUN"
        }
    );
    println!();

    let Some(plan) = &report.plan else {
        println!("{}", report.reason());
        return;
    };

    if report.dry_run {
        println!("Would remove:   {} candidate(s)", plan.remove.len());
        println!("Would reclaim:  {}", tmpfs_reclaim::human_size(report.bytes_reclaimable()));
        println!("Would keep:     {} candidate(s)", plan.keep.len());
        for candidate in &plan.remove {
            println!(
                "  - {} ({})",
                candidate.path.display(),
                tmpfs_reclaim::human_size(candidate.size_bytes)
            );
        }
        println!();
        println!("{}", plan.summary());
        println!();
        println!("Re-run without --dry-run to actually remove these directories.");
    } else {
        println!("Removed:        {} candidate(s)", report.removed.len());
        println!("Reclaimed:      {}", tmpfs_reclaim::human_size(report.bytes_reclaimed()));
        println!("Kept:           {} candidate(s)", plan.keep.len());
        for candidate in &report.removed {
            println!(
                "  - {} ({})",
                candidate.path.display(),
                tmpfs_reclaim::human_size(candidate.size_bytes)
            );
        }
        let failed = plan.remove.len().saturating_sub(report.removed.len());
        if failed > 0 {
            println!("Failed:         {failed} candidate(s) could not be removed — see daemon log");
        }
    }
}

fn print_json(report: &TmpfsReclaimReport, name_patterns: &[String]) {
    let paths = |candidates: &[tmpfs_reclaim::ScratchDirCandidate]| {
        candidates
            .iter()
            .map(|c| {
                json!({
                    "path": c.path.display().to_string(),
                    "mount_point": c.mount_point.display().to_string(),
                    "size_bytes": c.size_bytes,
                    "has_open_handle": c.has_open_handle,
                })
            })
            .collect::<Vec<_>>()
    };

    let value = json!({
        "staleness_secs": report.staleness_secs,
        "name_patterns": name_patterns,
        "dry_run": report.dry_run,
        "planned_remove": report.plan.as_ref().map(|p| paths(&p.remove)).unwrap_or_default(),
        "planned_remove_count": report.plan.as_ref().map_or(0, |p| p.remove.len()),
        "planned_keep_count": report.plan.as_ref().map_or(0, |p| p.keep.len()),
        "bytes_reclaimable": report.bytes_reclaimable(),
        "removed": paths(&report.removed),
        "removed_count": report.removed.len(),
        "bytes_reclaimed": report.bytes_reclaimed(),
        "reason": report.reason(),
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
}
