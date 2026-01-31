"""Daemon cleanup integration - event-driven cleanup for the Loom daemon.

Ported from daemon-cleanup.sh. Provides lifecycle cleanup triggered by
daemon events:

- ``shepherd-complete <issue>`` -- cleanup after shepherd finishes an issue
- ``daemon-startup``           -- cleanup stale artifacts from previous session
- ``daemon-shutdown``          -- archive logs and finalize state before exit
- ``periodic``                 -- conservative periodic cleanup
- ``prune-sessions``           -- prune old daemon state session archives

Exit codes:
    0 - Success
    1 - Error
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
import time
from dataclasses import dataclass
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import now_utc, parse_iso_timestamp


# ---------------------------------------------------------------------------
# Configuration defaults (overridable via environment)
# ---------------------------------------------------------------------------
CLEANUP_ENABLED_DEFAULT = True
ARCHIVE_LOGS_DEFAULT = True
RETENTION_DAYS_DEFAULT = 7
GRACE_PERIOD_DEFAULT = 600  # seconds
MAX_ARCHIVED_SESSIONS_DEFAULT = 10
PROGRESS_STALE_HOURS_DEFAULT = 24
STARTUP_CLEANUP_MAX_FILES_DEFAULT = 20
STARTUP_CLEANUP_TIMEOUT_DEFAULT = 30  # seconds


@dataclass
class CleanupConfig:
    """Runtime configuration for daemon cleanup, resolved from env vars."""

    cleanup_enabled: bool
    archive_logs: bool
    retention_days: int
    grace_period: int  # seconds
    max_archived_sessions: int
    progress_stale_hours: int
    startup_cleanup_max_files: int
    startup_cleanup_timeout: int  # seconds


def load_config() -> CleanupConfig:
    """Build a :class:`CleanupConfig` from environment variables."""
    return CleanupConfig(
        cleanup_enabled=os.environ.get("LOOM_CLEANUP_ENABLED", "true").lower()
        == "true",
        archive_logs=os.environ.get("LOOM_ARCHIVE_LOGS", "true").lower() == "true",
        retention_days=_env_int("LOOM_RETENTION_DAYS", RETENTION_DAYS_DEFAULT),
        grace_period=_env_int("LOOM_GRACE_PERIOD", GRACE_PERIOD_DEFAULT),
        max_archived_sessions=_env_int(
            "LOOM_MAX_ARCHIVED_SESSIONS", MAX_ARCHIVED_SESSIONS_DEFAULT
        ),
        progress_stale_hours=_env_int(
            "LOOM_PROGRESS_STALE_HOURS", PROGRESS_STALE_HOURS_DEFAULT
        ),
        startup_cleanup_max_files=_env_int(
            "LOOM_STARTUP_CLEANUP_MAX_FILES", STARTUP_CLEANUP_MAX_FILES_DEFAULT
        ),
        startup_cleanup_timeout=_env_int(
            "LOOM_STARTUP_CLEANUP_TIMEOUT", STARTUP_CLEANUP_TIMEOUT_DEFAULT
        ),
    )


def _env_int(name: str, default: int) -> int:
    val = os.environ.get(name)
    if val is not None:
        try:
            return int(val)
        except ValueError:
            pass
    return default


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def has_active_shepherds(daemon_state: dict[str, Any]) -> bool:
    """Return ``True`` if any shepherd in *daemon_state* has a non-null issue."""
    shepherds = daemon_state.get("shepherds", {})
    for entry in shepherds.values():
        if isinstance(entry, dict) and entry.get("issue") is not None:
            return True
    return False


def update_cleanup_timestamp(
    state_path: pathlib.Path,
    event: str,
) -> None:
    """Record the last cleanup event in the daemon state file."""
    data = read_json_file(state_path)
    if not isinstance(data, dict):
        return

    ts = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    cleanup = data.get("cleanup")
    if not isinstance(cleanup, dict):
        cleanup = {
            "lastRun": None,
            "lastCleaned": [],
            "pendingCleanup": [],
            "errors": [],
        }

    cleanup["lastRun"] = ts
    cleanup["lastEvent"] = event
    data["cleanup"] = cleanup
    write_json_file(state_path, data)


def _run_archive_logs(
    repo_root: pathlib.Path,
    *,
    dry_run: bool = False,
    prune_only: bool = False,
    retention_days: int | None = None,
) -> None:
    """Delegate to archive-logs.sh (not ported in this issue)."""
    script = repo_root / "scripts" / "archive-logs.sh"
    if not script.exists():
        script = repo_root / ".loom" / "scripts" / "archive-logs.sh"
    if not script.exists():
        log_warning("archive-logs.sh not found")
        return

    cmd: list[str] = [str(script)]
    if dry_run:
        cmd.append("--dry-run")
    if prune_only:
        cmd.append("--prune-only")
    if retention_days is not None:
        cmd.extend(["--retention-days", str(retention_days)])

    try:
        subprocess.run(cmd, capture_output=True, timeout=60, cwd=repo_root)
    except Exception:
        log_warning("archive-logs.sh execution failed")


def _find_loom_clean(repo_root: pathlib.Path) -> str | None:
    """Locate the ``loom-clean`` executable."""
    import shutil

    venv_path = repo_root / "loom-tools" / ".venv" / "bin" / "loom-clean"
    if venv_path.is_file() and os.access(venv_path, os.X_OK):
        return str(venv_path)
    if shutil.which("loom-clean"):
        return "loom-clean"
    return None


def _run_loom_clean(
    repo_root: pathlib.Path,
    *,
    dry_run: bool = False,
    grace_period: int | None = None,
) -> None:
    """Run ``loom-clean --safe --worktrees-only --force``."""
    loom_clean = _find_loom_clean(repo_root)
    if loom_clean is None:
        log_warning("loom-clean not found (install loom-tools)")
        return

    # Always include --force for non-interactive daemon operation
    cmd = [loom_clean, "--safe", "--worktrees-only", "--force"]
    if dry_run:
        cmd.append("--dry-run")
    if grace_period is not None:
        cmd.extend(["--grace-period", str(grace_period)])

    try:
        subprocess.run(cmd, capture_output=True, timeout=120, cwd=repo_root)
    except Exception:
        log_warning("loom-clean execution failed")


def _run_orphan_recovery(
    repo_root: pathlib.Path,
    *,
    recover: bool = False,
    verbose: bool = False,
) -> None:
    """Delegate to the Python orphan recovery module."""
    try:
        from loom_tools.orphan_recovery import run_orphan_recovery

        run_orphan_recovery(repo_root, recover=recover, verbose=verbose)
    except ImportError:
        # Fall back to the shell script
        script = repo_root / "scripts" / "recover-orphaned-shepherds.sh"
        if not script.exists():
            script = repo_root / ".loom" / "scripts" / "recover-orphaned-shepherds.sh"
        if script.exists() and os.access(script, os.X_OK):
            cmd: list[str] = [str(script)]
            if recover:
                cmd.append("--recover")
            if verbose:
                cmd.append("--verbose")
            try:
                subprocess.run(cmd, capture_output=True, timeout=60, cwd=repo_root)
            except Exception:
                log_warning("Orphaned shepherd recovery failed")
        else:
            log_warning("Orphan recovery not available")


# ---------------------------------------------------------------------------
# Progress file cleanup helpers
# ---------------------------------------------------------------------------


def cleanup_progress_file(
    progress_dir: pathlib.Path,
    issue_num: int,
    *,
    dry_run: bool = False,
) -> None:
    """Delete completed progress files for a given issue."""
    if not progress_dir.is_dir():
        return

    for progress_file in progress_dir.glob("shepherd-*.json"):
        data = read_json_file(progress_file)
        if not isinstance(data, dict):
            continue

        file_issue = data.get("issue", 0)
        file_status = data.get("status", "working")

        if file_issue == issue_num:
            if file_status == "completed":
                if dry_run:
                    log_info(
                        f"[DRY-RUN] Would delete progress file: {progress_file.name}"
                    )
                else:
                    progress_file.unlink(missing_ok=True)
                    log_info(f"Deleted progress file: {progress_file.name}")
            else:
                log_info(
                    f"Progress file for issue #{issue_num} has status "
                    f"'{file_status}', not cleaning"
                )


def _batch_check_issue_states(issue_numbers: list[int]) -> dict[int, str]:
    """Check multiple issue states using a single GraphQL query.

    Returns a dict mapping issue numbers to their state ("OPEN" or "CLOSED").
    Issues that couldn't be fetched are mapped to "unknown".
    """
    if not issue_numbers:
        return {}

    from loom_tools.common.github import gh_run

    # Build GraphQL query for batch issue state lookup
    # Query format: issue0: issue(number: N) { state } for each issue
    query_parts = []
    for i, issue_num in enumerate(issue_numbers):
        query_parts.append(f"issue{i}: issue(number: {issue_num}) {{ state }}")

    query = """
    query($owner: String!, $repo: String!) {
      repository(owner: $owner, name: $repo) {
        %s
      }
    }
    """ % "\n        ".join(query_parts)

    # Get repo owner/name from gh
    try:
        repo_result = gh_run(
            [
                "repo",
                "view",
                "--json",
                "owner,name",
                "--jq",
                '.owner.login + "/" + .name',
            ],
            check=False,
        )
        if repo_result.returncode != 0 or not repo_result.stdout.strip():
            return {n: "unknown" for n in issue_numbers}

        owner, repo = repo_result.stdout.strip().split("/", 1)
    except Exception:
        return {n: "unknown" for n in issue_numbers}

    # Execute GraphQL query
    try:
        result = gh_run(
            [
                "api",
                "graphql",
                "-f",
                f"query={query}",
                "-f",
                f"owner={owner}",
                "-f",
                f"repo={repo}",
            ],
            check=False,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return {n: "unknown" for n in issue_numbers}

        response = json.loads(result.stdout)
        repo_data = response.get("data", {}).get("repository", {})

        states: dict[int, str] = {}
        for i, issue_num in enumerate(issue_numbers):
            issue_data = repo_data.get(f"issue{i}")
            if issue_data and "state" in issue_data:
                states[issue_num] = issue_data["state"]
            else:
                states[issue_num] = "unknown"
        return states
    except Exception:
        return {n: "unknown" for n in issue_numbers}


def cleanup_stale_progress_files(
    repo_root: pathlib.Path,
    stale_hours: int,
    *,
    dry_run: bool = False,
    max_files: int | None = None,
    timeout_seconds: int | None = None,
) -> None:
    """Delete progress files that are stale (old heartbeats or non-working).

    Optimizations over the original implementation:
    1. File age filtering: Deletes files older than stale_hours without GitHub queries
    2. Batch GitHub queries: Uses GraphQL to check multiple issue states in one request
    3. Max files limit: Processes at most max_files per call to avoid startup delays
    4. Timeout: Aborts cleanup if it exceeds timeout_seconds
    """
    progress_dir = repo_root / ".loom" / "progress"
    if not progress_dir.is_dir():
        return

    now = now_utc()
    stale_threshold = stale_hours * 3600
    start_time = time.monotonic()

    log_info(f"Cleaning stale progress files (older than {stale_hours}h)...")

    # Phase 1: Categorize files by cleanup action needed
    files_to_delete: list[tuple[pathlib.Path, str]] = []  # (path, reason)
    files_needing_github_check: list[tuple[pathlib.Path, int]] = []  # (path, issue_num)
    files_processed = 0

    all_progress_files = list(progress_dir.glob("shepherd-*.json"))
    if max_files is not None:
        log_info(
            f"Processing up to {max_files} of {len(all_progress_files)} progress files"
        )

    for progress_file in all_progress_files:
        # Check timeout
        if timeout_seconds is not None:
            elapsed = time.monotonic() - start_time
            if elapsed >= timeout_seconds:
                remaining = len(all_progress_files) - files_processed
                log_warning(
                    f"Cleanup timeout ({timeout_seconds}s) reached after {files_processed} files. "
                    f"{remaining} files deferred to next cleanup cycle."
                )
                break

        # Check max files limit
        if max_files is not None and files_processed >= max_files:
            remaining = len(all_progress_files) - files_processed
            log_info(
                f"Max files limit ({max_files}) reached. {remaining} files deferred to next cleanup cycle."
            )
            break

        files_processed += 1

        # Check file age first (fast, no parsing needed)
        try:
            file_age_seconds = time.time() - progress_file.stat().st_mtime
            if file_age_seconds >= stale_threshold:
                files_to_delete.append(
                    (progress_file, f"file older than {stale_hours}h")
                )
                continue
        except OSError:
            continue

        # Parse file to check status and heartbeat
        data = read_json_file(progress_file)
        if not isinstance(data, dict):
            continue

        status = data.get("status", "working")
        last_heartbeat = data.get("last_heartbeat", "")

        if status != "working":
            # Non-working (completed/errored/blocked) -- clean immediately
            files_to_delete.append((progress_file, f"status: {status}"))
            continue

        # Working file: check heartbeat freshness
        if last_heartbeat:
            try:
                hb_dt = parse_iso_timestamp(last_heartbeat)
                age_seconds = int((now - hb_dt).total_seconds())
                if age_seconds < stale_threshold:
                    continue  # Fresh, skip
            except (ValueError, OverflowError):
                pass

        # Stale working file -- need to check if issue is closed via GitHub
        file_issue = data.get("issue", 0)
        if file_issue:
            files_needing_github_check.append((progress_file, file_issue))

    # Phase 2: Batch check issue states via GitHub GraphQL API
    if files_needing_github_check:
        # Check timeout before making API call
        if timeout_seconds is not None:
            elapsed = time.monotonic() - start_time
            if elapsed >= timeout_seconds:
                log_warning(
                    f"Cleanup timeout ({timeout_seconds}s) reached before GitHub check. "
                    f"{len(files_needing_github_check)} files deferred."
                )
                files_needing_github_check = []

        if files_needing_github_check:
            log_info(
                f"Checking {len(files_needing_github_check)} issue states via GitHub API..."
            )
            issue_numbers = [issue for _, issue in files_needing_github_check]
            issue_states = _batch_check_issue_states(issue_numbers)

            for progress_file, file_issue in files_needing_github_check:
                state = issue_states.get(file_issue, "unknown")
                if state == "CLOSED":
                    files_to_delete.append(
                        (progress_file, f"issue #{file_issue} closed")
                    )

    # Phase 3: Delete files
    deleted_count = 0
    for progress_file, reason in files_to_delete:
        if dry_run:
            log_info(f"[DRY-RUN] Would delete: {progress_file.name} ({reason})")
        else:
            try:
                progress_file.unlink(missing_ok=True)
                log_info(f"Deleted: {progress_file.name} ({reason})")
                deleted_count += 1
            except OSError as e:
                log_warning(f"Failed to delete {progress_file.name}: {e}")

    if not dry_run:
        elapsed = time.monotonic() - start_time
        log_info(f"Progress file cleanup: {deleted_count} deleted in {elapsed:.1f}s")


# ---------------------------------------------------------------------------
# Event handlers
# ---------------------------------------------------------------------------


def handle_shepherd_complete(
    repo_root: pathlib.Path,
    issue_number: int,
    config: CleanupConfig,
    *,
    dry_run: bool = False,
) -> None:
    """Cleanup after a shepherd finishes an issue."""
    from loom_tools.common.github import gh_run

    log_info(f"Shepherd Complete Cleanup: Issue #{issue_number}")

    # Check if PR is merged
    branch_name = f"feature/issue-{issue_number}"
    try:
        result = gh_run(
            [
                "pr",
                "list",
                "--head",
                branch_name,
                "--state",
                "all",
                "--json",
                "state,mergedAt",
                "--jq",
                ".[0] // empty",
            ],
            check=False,
        )
        pr_info = result.stdout.strip()
    except Exception:
        pr_info = ""

    if not pr_info:
        log_info(f"No PR found for issue #{issue_number}, skipping cleanup")
        return

    try:
        pr_data = json.loads(pr_info)
        merged_at = pr_data.get("mergedAt")
    except (json.JSONDecodeError, AttributeError):
        merged_at = None

    state_path = repo_root / ".loom" / "daemon-state.json"

    if not merged_at:
        log_info("PR not merged yet, scheduling for later cleanup")
        if not dry_run and state_path.exists():
            data = read_json_file(state_path)
            if isinstance(data, dict):
                cleanup = data.get("cleanup", {})
                if not isinstance(cleanup, dict):
                    cleanup = {}
                pending = cleanup.get("pendingCleanup", [])
                item = f"issue-{issue_number}"
                if item not in pending:
                    pending.append(item)
                cleanup["pendingCleanup"] = pending
                data["cleanup"] = cleanup
                write_json_file(state_path, data)
        return

    # Archive logs
    if config.archive_logs:
        log_info(f"Archiving logs for issue #{issue_number}...")
        _run_archive_logs(repo_root, dry_run=dry_run)

    # Clean up worktree
    worktree_path = repo_root / ".loom" / "worktrees" / f"issue-{issue_number}"
    if worktree_path.is_dir():
        log_info(f"Cleaning worktree for issue #{issue_number}...")
        _run_loom_clean(repo_root, dry_run=dry_run, grace_period=config.grace_period)

    # Clean up progress file
    progress_dir = repo_root / ".loom" / "progress"
    cleanup_progress_file(progress_dir, issue_number, dry_run=dry_run)

    if not dry_run:
        update_cleanup_timestamp(state_path, "shepherd-complete")

    log_success(f"Shepherd complete cleanup finished for issue #{issue_number}")


def handle_daemon_startup(
    repo_root: pathlib.Path,
    config: CleanupConfig,
    *,
    dry_run: bool = False,
) -> None:
    """Cleanup stale artifacts from a previous daemon session."""
    log_info("Daemon Startup Cleanup")

    state_path = repo_root / ".loom" / "daemon-state.json"

    # 1. Orphaned shepherd recovery (critical, run first)
    log_info("Checking for orphaned shepherds from previous session...")
    _run_orphan_recovery(repo_root, recover=not dry_run, verbose=True)

    # 2. Archive orphaned task outputs
    if config.archive_logs:
        log_info("Archiving orphaned task outputs...")
        _run_archive_logs(repo_root, dry_run=dry_run)

    # 3. Process pending cleanups from previous session
    if state_path.exists():
        data = read_json_file(state_path)
        if isinstance(data, dict):
            cleanup = data.get("cleanup", {})
            pending = (
                cleanup.get("pendingCleanup", []) if isinstance(cleanup, dict) else []
            )
            if pending:
                log_info("Processing pending cleanups from previous session...")
                for item in list(pending):
                    log_info(f"  Processing: {item}")
                    if not dry_run:
                        pending.remove(item)
                if not dry_run:
                    cleanup["pendingCleanup"] = pending
                    data["cleanup"] = cleanup
                    write_json_file(state_path, data)

    # 4. Clean stale worktrees
    log_info("Cleaning stale worktrees...")
    _run_loom_clean(repo_root, dry_run=dry_run)

    # 5. Prune old archives
    log_info("Pruning old archives...")
    _run_archive_logs(
        repo_root,
        dry_run=dry_run,
        prune_only=True,
        retention_days=config.retention_days,
    )

    # 6. Cleanup stale progress files (with startup-specific limits)
    cleanup_stale_progress_files(
        repo_root,
        config.progress_stale_hours,
        dry_run=dry_run,
        max_files=config.startup_cleanup_max_files,
        timeout_seconds=config.startup_cleanup_timeout,
    )

    if not dry_run:
        update_cleanup_timestamp(state_path, "daemon-startup")

    log_success("Daemon startup cleanup complete")


def handle_daemon_shutdown(
    repo_root: pathlib.Path,
    config: CleanupConfig,
    *,
    dry_run: bool = False,
) -> None:
    """Archive logs and finalize state before daemon exits."""
    log_info("Daemon Shutdown Cleanup")

    state_path = repo_root / ".loom" / "daemon-state.json"

    # 1. Archive all current task outputs
    if config.archive_logs:
        log_info("Archiving task outputs...")
        _run_archive_logs(repo_root, dry_run=dry_run)

    # 2. Finalize daemon-state.json
    if state_path.exists():
        log_info("Finalizing daemon-state.json...")
        stopped_at = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

        if dry_run:
            log_info(
                f"[DRY-RUN] Would set running=false, stopped_at={stopped_at}, "
                "reset support roles and shepherds to idle"
            )
        else:
            data = read_json_file(state_path)
            if isinstance(data, dict):
                data["running"] = False
                data["stopped_at"] = stopped_at

                # Reset support roles
                support_roles = data.get("support_roles")
                if isinstance(support_roles, dict):
                    for role_entry in support_roles.values():
                        if isinstance(role_entry, dict):
                            role_entry["status"] = "idle"
                            role_entry["task_id"] = None
                            role_entry["last_completed"] = stopped_at

                # Reset shepherds
                shepherds = data.get("shepherds")
                if isinstance(shepherds, dict):
                    for shepherd_entry in shepherds.values():
                        if isinstance(shepherd_entry, dict):
                            shepherd_entry["status"] = "idle"
                            shepherd_entry["issue"] = None
                            shepherd_entry["task_id"] = None
                            shepherd_entry["idle_since"] = stopped_at
                            shepherd_entry["idle_reason"] = "shutdown_signal"

                write_json_file(state_path, data)
                log_success(
                    f"daemon-state.json finalized (running=false, stopped_at={stopped_at})"
                )

    # 3. Run session reflection if available
    for script_dir in [repo_root / "scripts", repo_root / ".loom" / "scripts"]:
        reflection = script_dir / "session-reflection.sh"
        if reflection.exists() and os.access(reflection, os.X_OK):
            log_info("Running session reflection...")
            cmd = [str(reflection)]
            if dry_run:
                cmd.append("--dry-run")
            try:
                result = subprocess.run(
                    cmd, capture_output=True, timeout=60, cwd=repo_root
                )
                if result.returncode != 0:
                    stderr = result.stderr.decode("utf-8", errors="replace").strip()
                    log_warning(
                        f"session-reflection.sh failed (exit {result.returncode})"
                        f"{': ' + stderr if stderr else ''}"
                    )
            except subprocess.TimeoutExpired:
                log_warning("session-reflection.sh timed out after 60s")
            except Exception as exc:
                log_warning(f"session-reflection.sh error: {exc}")
            break

    if not dry_run:
        update_cleanup_timestamp(state_path, "daemon-shutdown")

    log_success("Daemon shutdown cleanup complete")


def handle_periodic(
    repo_root: pathlib.Path,
    config: CleanupConfig,
    *,
    dry_run: bool = False,
) -> None:
    """Conservative periodic cleanup (respects active shepherds)."""
    log_info("Periodic Cleanup")

    state_path = repo_root / ".loom" / "daemon-state.json"
    daemon_state = read_json_file(state_path) if state_path.exists() else {}
    if not isinstance(daemon_state, dict):
        daemon_state = {}

    active = has_active_shepherds(daemon_state)
    if active:
        log_info("Active shepherds detected - running conservative cleanup only")

    # Archive task outputs (safe even with active shepherds)
    if config.archive_logs:
        log_info("Archiving task outputs...")
        _run_archive_logs(repo_root, dry_run=dry_run)

    # Only clean worktrees if no active shepherds
    if not active:
        log_info("No active shepherds - running full worktree cleanup...")
        _run_loom_clean(repo_root, dry_run=dry_run)
    else:
        log_info("Skipping worktree cleanup (active shepherds)")

    # Prune old archives
    log_info("Pruning old archives...")
    _run_archive_logs(
        repo_root,
        dry_run=dry_run,
        prune_only=True,
        retention_days=config.retention_days,
    )

    # Cleanup stale progress files
    cleanup_stale_progress_files(
        repo_root, config.progress_stale_hours, dry_run=dry_run
    )

    if not dry_run:
        update_cleanup_timestamp(state_path, "periodic")

    log_success("Periodic cleanup complete")


def handle_prune_sessions(
    repo_root: pathlib.Path,
    config: CleanupConfig,
    *,
    dry_run: bool = False,
) -> None:
    """Prune old daemon state session archives."""
    log_info("Prune Session Archives")

    loom_dir = repo_root / ".loom"
    archives = sorted(loom_dir.glob("[0-9][0-9]-daemon-state.json"))

    if not archives:
        log_info("No archived sessions found")
        return

    log_info(
        f"Found {len(archives)} archived session(s) "
        f"(max: {config.max_archived_sessions})"
    )

    to_delete = len(archives) - config.max_archived_sessions
    if to_delete <= 0:
        log_info("No pruning needed (under limit)")
        return

    log_info(f"Pruning {to_delete} oldest session(s)...")

    for archive in archives[:to_delete]:
        if dry_run:
            log_info(f"[DRY-RUN] Would delete: {archive.name}")
        else:
            archive.unlink(missing_ok=True)
            log_info(f"Deleted: {archive.name}")

    state_path = repo_root / ".loom" / "daemon-state.json"
    if not dry_run:
        update_cleanup_timestamp(state_path, "prune-sessions")

    log_success("Session pruning complete")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

VALID_EVENTS = [
    "shepherd-complete",
    "daemon-startup",
    "daemon-shutdown",
    "periodic",
    "prune-sessions",
]


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the daemon cleanup CLI."""
    parser = argparse.ArgumentParser(
        description="Daemon cleanup integration - event-driven cleanup for the Loom daemon",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Events:
  shepherd-complete <issue>   Cleanup after shepherd finishes an issue
  daemon-startup              Cleanup stale artifacts from previous session
  daemon-shutdown             Archive logs and cleanup before exit
  periodic                    Conservative periodic cleanup
  prune-sessions              Prune old daemon state session archives

Environment Variables:
  LOOM_CLEANUP_ENABLED            Enable/disable cleanup (default: true)
  LOOM_ARCHIVE_LOGS               Archive logs before deletion (default: true)
  LOOM_RETENTION_DAYS             Days to retain archives (default: 7)
  LOOM_GRACE_PERIOD               Seconds after PR merge before cleanup (default: 600)
  LOOM_STARTUP_CLEANUP_MAX_FILES  Max files to process at startup (default: 20)
  LOOM_STARTUP_CLEANUP_TIMEOUT    Timeout in seconds for startup cleanup (default: 30)

Examples:
  # After shepherd completes issue #123
  loom-daemon-cleanup shepherd-complete 123

  # On daemon startup
  loom-daemon-cleanup daemon-startup

  # Preview periodic cleanup
  loom-daemon-cleanup periodic --dry-run
""",
    )

    parser.add_argument(
        "event",
        choices=VALID_EVENTS,
        help="Event type to handle",
    )
    parser.add_argument(
        "issue_number",
        nargs="?",
        type=int,
        default=None,
        help="Issue number (required for shepherd-complete)",
    )
    parser.add_argument(
        "--issue",
        type=int,
        default=None,
        help="Issue number (alternative to positional argument)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be cleaned without making changes",
    )

    args = parser.parse_args(argv)

    config = load_config()
    if not config.cleanup_enabled:
        log_info(
            f"Cleanup disabled (LOOM_CLEANUP_ENABLED={os.environ.get('LOOM_CLEANUP_ENABLED')})"
        )
        return 0

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    # Resolve issue number from either positional or --issue flag
    issue_number = args.issue_number if args.issue_number is not None else args.issue

    try:
        if args.event == "shepherd-complete":
            if issue_number is None:
                log_error("Issue number required for shepherd-complete event")
                return 1
            handle_shepherd_complete(
                repo_root, issue_number, config, dry_run=args.dry_run
            )
        elif args.event == "daemon-startup":
            handle_daemon_startup(repo_root, config, dry_run=args.dry_run)
        elif args.event == "daemon-shutdown":
            handle_daemon_shutdown(repo_root, config, dry_run=args.dry_run)
        elif args.event == "periodic":
            handle_periodic(repo_root, config, dry_run=args.dry_run)
        elif args.event == "prune-sessions":
            handle_prune_sessions(repo_root, config, dry_run=args.dry_run)
    except Exception as exc:
        log_error(f"Cleanup failed: {exc}")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
