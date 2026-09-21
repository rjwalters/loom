//! `loom-daemon target-dir-gc` handler (Issue #8459) — CLI front-end for
//! [`loom_daemon::target_dir_gc`]. Purely file-based; does not require a
//! running daemon (mirrors `Clean`/`Cleanup`/`RecoverOrphans`).

use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde_json::json;

use loom_daemon::target_dir_gc::{self, GcReport};

pub(crate) fn handle_target_dir_gc_command(
    target_dir: &str,
    threshold_days: u64,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let target_dir = Path::new(target_dir);
    let report = target_dir_gc::run(target_dir, threshold_days, dry_run, Utc::now());

    if json {
        print_json(&report);
    } else {
        print_human(&report);
    }
    Ok(())
}

fn print_human(report: &GcReport) {
    println!("Target dir:     {}", report.target_dir.display());
    println!("Threshold:      {} day(s)", report.threshold_days);
    println!(
        "Mode:           {}",
        if report.dry_run {
            "DRY RUN"
        } else {
            "REAL RUN"
        }
    );
    println!();

    if report.build_in_progress {
        println!("DEFERRED: {}", report.reason());
        return;
    }

    if report.dry_run {
        println!("Would remove:   {} candidate(s)", report.plan.remove.len());
        println!("Would reclaim:  {}", human_bytes(report.bytes_reclaimable()));
        println!("Would keep:     {} candidate(s)", report.plan.keep.len());
        println!();
        println!("{}", report.plan.summary());
        println!();
        println!("Re-run without --dry-run to actually remove these entries.");
    } else {
        println!("Removed:        {} candidate(s)", report.removed.len());
        println!("Reclaimed:      {}", human_bytes(report.bytes_reclaimed()));
        println!("Kept:           {} candidate(s)", report.plan.keep.len());
        let failed = report
            .plan
            .remove
            .len()
            .saturating_sub(report.removed.len());
        if failed > 0 {
            println!("Failed:         {failed} candidate(s) could not be removed — see daemon log");
        }
    }
}

fn print_json(report: &GcReport) {
    let value = json!({
        "target_dir": report.target_dir,
        "threshold_days": report.threshold_days,
        "dry_run": report.dry_run,
        "build_in_progress": report.build_in_progress,
        "planned_remove_count": report.plan.remove.len(),
        "planned_keep_count": report.plan.keep.len(),
        "bytes_reclaimable": report.bytes_reclaimable(),
        "removed_count": report.removed.len(),
        "bytes_reclaimed": report.bytes_reclaimed(),
        "reason": report.reason(),
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
}

fn human_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", b / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", b / 1024.0)
    } else {
        format!("{bytes}B")
    }
}
