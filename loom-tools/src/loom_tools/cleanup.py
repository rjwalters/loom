"""Log archival cleanup for Loom (post-daemon-brain).

This module is the post-daemon-brain replacement for ``daemon_cleanup.py``
(issue #3396, Phase 3.1.7 of epic #3372).  The daemon-event-driven cleanup
paths (``shepherd-complete``, ``daemon-startup``, ``daemon-shutdown``,
``periodic``, ``prune-sessions``) have been removed -- session rotation goes
away with the daemon brain (retired in Phase 3.2).

What remains is the log-archival logic that operates on ``.loom/logs/``
(by delegating to ``archive-logs.sh``).  This piece ports cleanly because
it does not read ``daemon-state.json`` at all.

CLI surface::

    loom-cleanup logs                     # archive task outputs + prune old
    loom-cleanup logs --dry-run           # preview archival/pruning
    loom-cleanup logs --prune-only        # skip archival, only prune
    loom-cleanup logs --retention-days N  # override retention window

Exit codes:
    0 - Success
    1 - Error
"""

from __future__ import annotations

import argparse
import os
import pathlib
import subprocess
import sys
from dataclasses import dataclass

from loom_tools.common.config import env_bool, env_int
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root


# ---------------------------------------------------------------------------
# Configuration defaults (overridable via environment)
# ---------------------------------------------------------------------------
CLEANUP_ENABLED_DEFAULT = True
ARCHIVE_LOGS_DEFAULT = True
RETENTION_DAYS_DEFAULT = 7


@dataclass
class CleanupConfig:
    """Runtime configuration for log archival, resolved from env vars."""

    cleanup_enabled: bool
    archive_logs: bool
    retention_days: int


def load_config() -> CleanupConfig:
    """Build a :class:`CleanupConfig` from environment variables."""
    return CleanupConfig(
        cleanup_enabled=env_bool("LOOM_CLEANUP_ENABLED", CLEANUP_ENABLED_DEFAULT),
        archive_logs=env_bool("LOOM_ARCHIVE_LOGS", ARCHIVE_LOGS_DEFAULT),
        retention_days=env_int("LOOM_RETENTION_DAYS", RETENTION_DAYS_DEFAULT),
    )


# ---------------------------------------------------------------------------
# Log archival
# ---------------------------------------------------------------------------


def _find_archive_logs_script(repo_root: pathlib.Path) -> pathlib.Path | None:
    """Locate ``archive-logs.sh`` in either ``scripts/`` or ``.loom/scripts/``."""
    for candidate in (
        repo_root / "scripts" / "archive-logs.sh",
        repo_root / ".loom" / "scripts" / "archive-logs.sh",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def run_archive_logs(
    repo_root: pathlib.Path,
    *,
    dry_run: bool = False,
    prune_only: bool = False,
    retention_days: int | None = None,
) -> int:
    """Delegate to ``archive-logs.sh``.

    Returns the subprocess exit code (0 on success).  Failures are logged
    but do not raise; callers can decide whether to propagate non-zero exits.
    """
    script = _find_archive_logs_script(repo_root)
    if script is None:
        log_warning("archive-logs.sh not found in scripts/ or .loom/scripts/")
        return 1

    cmd: list[str] = [str(script)]
    if dry_run:
        cmd.append("--dry-run")
    if prune_only:
        cmd.append("--prune-only")
    if retention_days is not None:
        cmd.extend(["--retention-days", str(retention_days)])

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=60,
            cwd=repo_root,
        )
    except subprocess.TimeoutExpired:
        log_warning("archive-logs.sh timed out after 60s")
        return 1
    except Exception as exc:
        log_warning(f"archive-logs.sh execution failed: {exc}")
        return 1

    if result.stdout:
        for line in result.stdout.splitlines():
            log_info(line)
    if result.returncode != 0:
        stderr = (result.stderr or "").strip()
        log_warning(
            f"archive-logs.sh exited {result.returncode}"
            f"{': ' + stderr if stderr else ''}"
        )
    return result.returncode


def handle_logs(
    repo_root: pathlib.Path,
    config: CleanupConfig,
    *,
    dry_run: bool = False,
    prune_only: bool = False,
    retention_days: int | None = None,
) -> int:
    """Run the log-archival cleanup path.

    Equivalent to the surviving slice of the legacy daemon-cleanup events
    (the ``_run_archive_logs`` call) -- archival of task outputs followed
    by retention-based pruning of older archives.
    """
    log_info("Log Archival Cleanup")

    if not config.archive_logs and not prune_only:
        log_info("Log archival disabled (LOOM_ARCHIVE_LOGS=0); skipping")
        return 0

    effective_retention = (
        retention_days if retention_days is not None else config.retention_days
    )

    rc = run_archive_logs(
        repo_root,
        dry_run=dry_run,
        prune_only=prune_only,
        retention_days=effective_retention,
    )
    if rc == 0:
        log_success("Log archival complete")
    return rc


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the ``loom-cleanup`` CLI."""
    parser = argparse.ArgumentParser(
        prog="loom-cleanup",
        description=(
            "Loom cleanup utilities (post-daemon-brain).  Currently exposes "
            "log archival; daemon event-driven cleanup was removed in #3396."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Commands:
  logs                          Archive task outputs and prune old archives

Environment Variables:
  LOOM_CLEANUP_ENABLED   Enable/disable cleanup entirely (default: true)
  LOOM_ARCHIVE_LOGS      Archive logs before pruning (default: true)
  LOOM_RETENTION_DAYS    Days to retain archives (default: 7)

Examples:
  loom-cleanup logs
  loom-cleanup logs --dry-run
  loom-cleanup logs --prune-only --retention-days 14
""",
    )

    subparsers = parser.add_subparsers(dest="command", required=True)

    logs_parser = subparsers.add_parser(
        "logs",
        help="Archive task outputs and prune old archives",
    )
    logs_parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be archived/pruned without making changes",
    )
    logs_parser.add_argument(
        "--prune-only",
        action="store_true",
        help="Skip new archival; only prune archives older than retention",
    )
    logs_parser.add_argument(
        "--retention-days",
        type=int,
        default=None,
        metavar="N",
        help="Override LOOM_RETENTION_DAYS (default: 7)",
    )

    args = parser.parse_args(argv)

    config = load_config()
    if not config.cleanup_enabled:
        log_info(
            "Cleanup disabled "
            f"(LOOM_CLEANUP_ENABLED={os.environ.get('LOOM_CLEANUP_ENABLED')})"
        )
        return 0

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    try:
        if args.command == "logs":
            return handle_logs(
                repo_root,
                config,
                dry_run=args.dry_run,
                prune_only=args.prune_only,
                retention_days=args.retention_days,
            )
    except Exception as exc:
        log_error(f"Cleanup failed: {exc}")
        return 1

    # argparse with required=True should never let us reach here.
    return 1


if __name__ == "__main__":
    sys.exit(main())
