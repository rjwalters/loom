//! `loom-daemon clean` / `cleanup` / `recover-orphans` handlers (Issue
//! #4712 — split out of `main.rs`). Native ports of `loom-clean` /
//! `loom-cleanup` / `loom-recover-orphans` (Issue #4272).

use anyhow::Result;

use loom_daemon::worktree_ops::clean;

use crate::CleanupAction;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_clean_command(
    workspace: &str,
    dry_run: bool,
    deep: bool,
    force: bool,
    safe: bool,
    grace_period: i64,
    worktrees_only: bool,
    branches_only: bool,
    tmux_only: bool,
    daemon: bool,
    aggressive: bool,
    aggressive_min_age: u64,
) -> Result<()> {
    use loom_daemon::worktree_ops::aggressive as agg;
    use loom_daemon::worktree_ops::repo;

    let repo_root = repo::resolve_repo_root(workspace)?;

    if daemon {
        println!();
        println!("========================================");
        println!("  Loom Crash Recovery");
        if dry_run {
            println!("  (DRY RUN MODE)");
        }
        println!("========================================");
        println!();
        clean::clean_daemon_crash_state(&repo_root, dry_run);
        if dry_run {
            println!("Dry run complete - no changes made");
        } else {
            println!("Crash recovery complete!");
        }
        println!();
        return Ok(());
    }

    if aggressive {
        println!();
        println!("========================================");
        println!("  Loom Aggressive Worktree Cleanup");
        if dry_run {
            println!("  (DRY RUN MODE)");
        }
        println!("========================================");
        println!();
        eprintln!(
            "Aggressive mode overrides .loom-in-use markers and process-table guards. Respects \
             open PRs, active shepherds, the .loom-managed sentinel, uncommitted changes, and \
             reachability from origin/main."
        );
        println!();

        let stats = agg::clean_aggressive(&repo_root, dry_run, force, aggressive_min_age);
        agg::print_aggressive_summary(&stats, dry_run);
        println!("{}", clean::completion_line("Aggressive cleanup", dry_run, stats.errors));
        if dry_run {
            println!("Run without --dry-run to perform cleanup");
        }
        println!();
        std::process::exit(clean::exit_code(stats.errors));
    }

    let opts = clean::CleanOptions {
        dry_run,
        deep,
        force,
        safe,
        grace_period_secs: grace_period,
        worktrees_only,
        branches_only,
        tmux_only,
        // The interactive CLI keeps its historical behavior: an operator who
        // typed the command is the authority on a sentinel-less worktree. Only
        // the unattended reaper (#4876) requires the `.loom-managed` sentinel.
        require_managed_sentinel: false,
    };
    let exit_code = clean::run_clean(&repo_root, &opts);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

pub(crate) fn handle_cleanup_command(action: CleanupAction) -> Result<()> {
    use loom_daemon::worktree_ops::{logs, repo};
    match action {
        CleanupAction::Logs {
            workspace,
            dry_run,
            prune_only,
            retention_days,
        } => {
            let repo_root = repo::resolve_repo_root(&workspace)?;
            let rc = logs::handle_logs(&repo_root, dry_run, prune_only, retention_days);
            if rc != 0 {
                std::process::exit(rc);
            }
            Ok(())
        }
    }
}

pub(crate) fn handle_recover_orphans_command(
    workspace: &str,
    recover: bool,
    json: bool,
    verbose: bool,
) -> Result<()> {
    use loom_daemon::worktree_ops::{orphan_recovery as orphans, repo};

    let repo_root = repo::resolve_repo_root(workspace)?;

    if !json {
        println!("Orphaned Spawn-Loop Task Detection & Recovery");
        if !recover {
            println!("DRY RUN - No changes will be made");
            println!("Use --recover to actually perform recovery");
        }
    }

    let result = orphans::run_orphan_recovery(&repo_root, recover, verbose);

    if json {
        println!("{}", orphans::format_result_json(&result));
    } else {
        println!("{}", orphans::format_result_human(&result));
    }

    if !result.orphaned.is_empty() && !recover {
        std::process::exit(2);
    }
    Ok(())
}
