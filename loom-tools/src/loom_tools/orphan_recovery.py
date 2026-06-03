"""Orphaned task detection and recovery (spawn-loop edition).

Detects and recovers orphaned state that occurs when:

- An issue carries the ``loom:building`` label but no spawn-loop task is
  tracking it (untracked building issue).
- A spawn-loop task entry has a stale ``last_heartbeat`` and a dead PID
  (loop crash or unresponsive tick — see #3411).

This module was ported from the daemon-state edition in Phase 3.1.6
(epic #3372, tracker #3378, issue #3395).  The pre-port version read
``.loom/daemon-state.json::shepherds`` plus ``.loom/progress/`` files and
``recent_failures``; all three of those state sources go away with the
daemon brain.

The new sources of truth are:

- ``.loom/spawn-loop-state.json::running`` (a flat list of live sweep tasks,
  written by ``defaults/scripts/spawn-loop.sh`` — see
  :mod:`loom_tools.models.spawn_loop_state`).
- ``gh issue list --label loom:building`` (unchanged).

Stuck-but-running detection lives in :mod:`loom_tools.stuck_detection` (2-min
heartbeat).  This module's heartbeat threshold is intentionally higher
(5 minutes by default) because orphan recovery is post-crash cleanup, not
real-time monitoring.

Exit codes:
    0 - No orphans detected
    1 - Error occurred
    2 - Orphans detected
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Any

from loom_tools.claim import has_valid_claim
from loom_tools.common.git import parse_porcelain_path
from loom_tools.common.github import get_repo_nwo, gh_issue_list, gh_run
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_spawn_loop_state
from loom_tools.common.time_utils import elapsed_seconds, format_duration, now_utc
from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

# Default heartbeat stale threshold (5 minutes for orphan recovery).
# Intentionally higher than stuck_detection's 120s because orphan recovery
# is post-crash cleanup, not real-time monitoring.
DEFAULT_HEARTBEAT_STALE_THRESHOLD = 300

# Grace period for recently-applied loom:building labels (10 minutes).
# Issues with loom:building added less than this many seconds ago are
# assumed to be actively worked on and skipped by orphan recovery.  This
# protects newly-claimed issues and manual sweeps from being incorrectly
# recovered before claims or heartbeats are established.
DEFAULT_LABEL_GRACE_PERIOD = 600

# Deduplication window for orphan recovery comments (5 minutes).
# If an "## Orphan Recovery" comment was posted within this window,
# skip posting another to avoid duplicate noise (see issue #2658).
ORPHAN_COMMENT_DEDUP_SECONDS = 300


@dataclass
class OrphanEntry:
    """A detected orphan."""

    type: str  # untracked_building | stale_heartbeat
    issue: int | None = None
    pid: int | None = None
    title: str | None = None
    reason: str = ""
    age_seconds: int | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"type": self.type, "reason": self.reason}
        if self.issue is not None:
            d["issue"] = self.issue
        if self.pid is not None:
            d["pid"] = self.pid
        if self.title is not None:
            d["title"] = self.title
        if self.age_seconds is not None:
            d["age_seconds"] = self.age_seconds
        return d


@dataclass
class RecoveryEntry:
    """A recovery action taken."""

    action: str  # reset_issue_label | cleanup_stale_worktree
    issue: int | None = None
    pid: int | None = None
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"action": self.action, "reason": self.reason}
        if self.issue is not None:
            d["issue"] = self.issue
        if self.pid is not None:
            d["pid"] = self.pid
        return d


@dataclass
class OrphanRecoveryResult:
    """Result of orphan detection and recovery."""

    orphaned: list[OrphanEntry] = field(default_factory=list)
    recovered: list[RecoveryEntry] = field(default_factory=list)
    recover_mode: bool = False

    @property
    def total_orphaned(self) -> int:
        return len(self.orphaned)

    @property
    def total_recovered(self) -> int:
        return len(self.recovered)

    def to_dict(self) -> dict[str, Any]:
        return {
            "orphaned": [o.to_dict() for o in self.orphaned],
            "recovered": [r.to_dict() for r in self.recovered],
            "total_orphaned": self.total_orphaned,
            "total_recovered": self.total_recovered,
            "recover_mode": self.recover_mode,
        }


def _get_heartbeat_stale_threshold() -> int:
    """Get heartbeat stale threshold from env var or default."""
    env_val = os.environ.get("LOOM_HEARTBEAT_STALE_THRESHOLD")
    if env_val is not None:
        try:
            return int(env_val)
        except ValueError:
            pass
    return DEFAULT_HEARTBEAT_STALE_THRESHOLD


def _get_label_grace_period() -> int:
    """Get label grace period from env var or default."""
    env_val = os.environ.get("LOOM_LABEL_GRACE_PERIOD")
    if env_val is not None:
        try:
            return int(env_val)
        except ValueError:
            pass
    return DEFAULT_LABEL_GRACE_PERIOD


def _pid_alive(pid: int) -> bool:
    """Return True if *pid* is a live process.

    Uses ``os.kill(pid, 0)`` which raises ``ProcessLookupError`` for dead
    PIDs and ``PermissionError`` for live PIDs we don't own (treated as
    alive — better to skip recovery than tear down somebody else's work).
    """
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        # Any other OSError (rare) — be conservative: assume alive.
        return True
    return True


def _get_building_label_age(issue: int) -> int | None:
    """Return seconds since the ``loom:building`` label was applied to *issue*.

    Queries the GitHub API for issue timeline events to find the most recent
    ``labeled`` event for ``loom:building``.  Returns ``None`` if the label
    event cannot be determined (API failure, no events, etc.).
    """
    nwo = get_repo_nwo()
    if not nwo:
        log_warning(
            f"Cannot determine label age for #{issue}: "
            "repo NWO not available"
        )
        return None

    try:
        result = gh_run(
            [
                "api",
                f"repos/{nwo}/issues/{issue}/events",
                "--jq",
                '[.[] | select(.event == "labeled" and .label.name == "loom:building")] | last | .created_at',
            ],
            check=False,
        )
    except Exception as exc:
        log_warning(
            f"Cannot determine label age for #{issue}: "
            f"API call failed ({exc})"
        )
        return None

    if result.returncode != 0:
        log_warning(
            f"Cannot determine label age for #{issue}: "
            f"gh returned exit code {result.returncode}"
        )
        return None

    timestamp = result.stdout.strip().strip('"')
    if not timestamp or timestamp == "null":
        log_warning(
            f"Cannot determine label age for #{issue}: "
            "no loom:building label events found"
        )
        return None

    try:
        return elapsed_seconds(timestamp)
    except (ValueError, OverflowError):
        log_warning(
            f"Cannot determine label age for #{issue}: "
            f"unparseable timestamp '{timestamp}'"
        )
        return None


def check_untracked_building(
    spawn_loop_state: SpawnLoopState,
    result: OrphanRecoveryResult,
    *,
    repo_root: pathlib.Path | None = None,
    label_grace_period: int = DEFAULT_LABEL_GRACE_PERIOD,
    verbose: bool = False,
) -> None:
    """Find ``loom:building`` issues without an active spawn-loop task.

    Cross-references ``gh issue list --label loom:building`` against the
    tracked issue set in ``.loom/spawn-loop-state.json::running``.  Issues
    with a valid file-based claim are skipped (CLI-driven sweeps may hold
    a claim without a spawn-loop entry).  Issues with a recently-applied
    ``loom:building`` label are also skipped (label-age grace period).
    """
    try:
        building_issues = gh_issue_list(labels=["loom:building"])
    except Exception as exc:
        log_error(f"Failed to list loom:building issues: {exc}")
        return

    if not building_issues:
        if verbose:
            log_info("No loom:building issues found")
        return

    tracked_issues: set[int] = {
        task.issue for task in spawn_loop_state.running if task.issue
    }

    for issue_data in building_issues:
        issue_num = issue_data.get("number", 0)
        issue_title = issue_data.get("title", "")

        if verbose:
            log_info(f"Checking issue #{issue_num}")

        if issue_num in tracked_issues:
            if verbose:
                log_info(f"  OK: tracked in spawn-loop-state")
            continue

        # File-based claim check (primary protection, no API call).
        # A CLI-driven sweep may hold a valid claim without a spawn-loop
        # entry, e.g. during a long builder subprocess.
        if repo_root is not None:
            if has_valid_claim(repo_root, issue_num):
                if verbose:
                    log_info(
                        f"  SKIPPED: #{issue_num} has a valid file-based claim"
                    )
                continue
            elif verbose:
                log_info(
                    f"  No valid file-based claim for #{issue_num}"
                )
        else:
            log_warning(
                f"  repo_root is None — skipping file-based claim check "
                f"for #{issue_num} (this may cause false positives)"
            )

        # Label-age grace period: skip issues where loom:building was
        # applied recently.  Protects newly-claimed issues from premature
        # orphan recovery before claims or heartbeats are established.
        if label_grace_period > 0:
            label_age = _get_building_label_age(issue_num)
            if label_age is not None and label_age < label_grace_period:
                if verbose:
                    log_info(
                        f"  SKIPPED: #{issue_num} label loom:building "
                        f"applied {label_age}s ago (grace period: "
                        f"{label_grace_period}s)"
                    )
                continue

        if verbose:
            log_warning(
                f"  ORPHANED: #{issue_num} has loom:building "
                "but no active spawn-loop task"
            )
        result.orphaned.append(
            OrphanEntry(
                type="untracked_building",
                issue=issue_num,
                title=issue_title,
                reason="no_spawn_loop_entry",
            )
        )


def check_stale_heartbeats(
    spawn_loop_state: SpawnLoopState,
    result: OrphanRecoveryResult,
    *,
    heartbeat_threshold: int = DEFAULT_HEARTBEAT_STALE_THRESHOLD,
    verbose: bool = False,
) -> None:
    """Flag spawn-loop tasks whose heartbeat is stale and PID is dead.

    The spawn loop refreshes ``last_heartbeat`` every tick for every live
    child PID (#3411).  A stale heartbeat therefore implies either:

    - The spawn loop itself crashed or hung (no ticks happening), or
    - The PID is gone but the state entry was not reaped (shouldn't happen,
      but defensive).

    Either way the entry is orphaned and should be cleaned up.  If the PID
    is still alive we skip the entry — the spawn loop may have just been
    paused / SIGSTOPped, and tearing down active work is the worst possible
    outcome.
    """
    for task in spawn_loop_state.running:
        if verbose:
            log_info(
                f"Checking task: issue=#{task.issue}, pid={task.pid}, "
                f"heartbeat={task.last_heartbeat or '<missing>'}"
            )

        hb = task.last_heartbeat
        if not hb:
            # No heartbeat is expected for pre-#3411 state files; nothing
            # to flag.  (stuck_detection.py handles missing-heartbeat
            # diagnostics on a faster cadence.)
            if verbose:
                log_info(
                    f"  Skipping issue #{task.issue}: no heartbeat field"
                )
            continue

        try:
            age = elapsed_seconds(hb)
        except (ValueError, OverflowError):
            if verbose:
                log_info(
                    f"  Skipping issue #{task.issue}: "
                    f"unparseable heartbeat '{hb}'"
                )
            continue

        if age <= heartbeat_threshold:
            if verbose:
                log_info(
                    f"  OK: issue #{task.issue} heartbeat {age}s old "
                    f"(threshold: {heartbeat_threshold}s)"
                )
            continue

        # Stale heartbeat — but skip if PID is still alive (loop paused,
        # not crashed).  Tearing down an active sweep is the worst case.
        if _pid_alive(task.pid):
            if verbose:
                log_info(
                    f"  Skipping issue #{task.issue}: heartbeat stale "
                    f"({age}s) but pid {task.pid} is alive"
                )
            continue

        if verbose:
            log_warning(
                f"  ORPHANED: issue #{task.issue} heartbeat "
                f"{age // 60}m old, pid {task.pid} dead"
            )
        result.orphaned.append(
            OrphanEntry(
                type="stale_heartbeat",
                issue=task.issue if task.issue else None,
                pid=task.pid,
                age_seconds=age,
                reason="heartbeat_stale",
            )
        )


def _cleanup_stale_worktree(repo_root: pathlib.Path, issue: int) -> bool:
    """Remove a stale worktree and its local/remote branches for an issue.

    A worktree is considered stale when it has zero commits ahead of main
    and no meaningful uncommitted changes (build artifacts are ignored).

    Returns True if cleanup was performed, False otherwise.
    """
    worktree_path = repo_root / ".loom" / "worktrees" / f"issue-{issue}"
    if not worktree_path.is_dir():
        return False

    # Check for commits ahead of main
    log_result = subprocess.run(
        ["git", "-C", str(worktree_path), "log", "--oneline", "origin/main..HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    if log_result.returncode != 0:
        log_warning(
            f"Cannot determine commit status for worktree issue-{issue}, "
            "skipping cleanup"
        )
        return False

    if log_result.stdout.strip():
        log_info(
            f"Worktree issue-{issue} has commits ahead of main, skipping cleanup"
        )
        return False

    # Check for meaningful uncommitted changes (ignore build artifacts)
    status_result = subprocess.run(
        ["git", "-C", str(worktree_path), "status", "--porcelain"],
        capture_output=True,
        text=True,
        check=False,
    )
    if status_result.returncode != 0:
        log_warning(
            f"Cannot determine status for worktree issue-{issue}, skipping cleanup"
        )
        return False

    build_artifact_patterns = (
        "node_modules",
        "pnpm-lock.yaml",
        ".venv",
        "target/",
        "Cargo.lock",
        "coverage/",
        ".loom-checkpoint",
        ".loom-in-use",
    )
    for line in status_result.stdout.strip().splitlines():
        filepath = parse_porcelain_path(line)
        if not any(pat in filepath for pat in build_artifact_patterns):
            log_info(
                f"Worktree issue-{issue} has meaningful uncommitted changes, "
                "skipping cleanup"
            )
            return False

    # Get branch name before removal
    branch_result = subprocess.run(
        ["git", "-C", str(worktree_path), "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    branch = branch_result.stdout.strip() if branch_result.returncode == 0 else ""

    # Remove worktree
    remove_result = subprocess.run(
        ["git", "worktree", "remove", str(worktree_path), "--force"],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
        check=False,
    )
    if remove_result.returncode != 0:
        log_warning(
            f"Failed to remove worktree issue-{issue}: "
            f"{remove_result.stderr.strip()}"
        )
        return False

    # Delete local branch (best-effort)
    if branch and branch != "main":
        subprocess.run(
            ["git", "-C", str(repo_root), "branch", "-D", branch],
            capture_output=True,
            check=False,
        )

    # Delete remote branch (best-effort)
    if branch and branch != "main":
        subprocess.run(
            ["git", "-C", str(repo_root), "push", "origin", "--delete", branch],
            capture_output=True,
            check=False,
        )

    log_info(
        f"Cleaned up stale worktree issue-{issue}"
        + (f" (branch {branch})" if branch else "")
    )
    return True


def _has_recent_orphan_comment(
    issue: int, dedup_seconds: int = ORPHAN_COMMENT_DEDUP_SECONDS
) -> bool:
    """Check if an orphan recovery comment was posted recently on this issue.

    Returns True if a comment starting with ``## Orphan Recovery`` was posted
    within *dedup_seconds*, preventing duplicate comments from concurrent or
    rapid-succession recovery runs (see issue #2658).
    """
    try:
        result = gh_run(
            [
                "issue", "view", str(issue),
                "--json", "comments",
                "--jq",
                '.comments | map(select(.body | startswith("## Orphan Recovery"))) '
                '| sort_by(.createdAt) | last | .createdAt // empty',
            ],
            check=False,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return False
        last_ts = result.stdout.strip()
        age = elapsed_seconds(last_ts)
        if age < dedup_seconds:
            log_info(
                f"Orphan recovery comment already posted on #{issue} "
                f"{age}s ago (dedup window: {dedup_seconds}s)"
            )
            return True
    except Exception:
        # If we can't check, allow the comment to be posted
        pass
    return False


def recover_issue(
    issue: int,
    reason: str,
    result: OrphanRecoveryResult,
    *,
    repo_root: pathlib.Path | None = None,
    label_grace_period: int = DEFAULT_LABEL_GRACE_PERIOD,
) -> None:
    """Recovery action: Reset issue labels from ``loom:building`` to ``loom:issue``.

    If ``repo_root`` is provided and a valid file-based claim exists for the
    issue, recovery is skipped to avoid disrupting an active sweep.

    A label-age grace period provides defense-in-depth: if the
    ``loom:building`` label was applied recently (within *label_grace_period*
    seconds), recovery is skipped regardless of claim state.
    """
    # Defense-in-depth: skip recovery if the label was recently applied.
    if label_grace_period > 0:
        label_age = _get_building_label_age(issue)
        if label_age is not None and label_age < label_grace_period:
            log_warning(
                f"Skipping recovery for issue #{issue}: "
                f"loom:building label applied {label_age}s ago "
                f"(grace period: {label_grace_period}s)"
            )
            return

    if repo_root is not None and has_valid_claim(repo_root, issue):
        log_warning(
            f"Skipping recovery for issue #{issue}: valid file-based claim exists"
        )
        return

    if repo_root is None:
        log_warning(
            f"repo_root is None for issue #{issue} recovery — "
            "cannot verify claims"
        )

    # Clean up stale worktree if present (0 commits ahead, no meaningful changes)
    worktree_cleaned = False
    if repo_root is not None:
        worktree_cleaned = _cleanup_stale_worktree(repo_root, issue)
        if worktree_cleaned:
            result.recovered.append(
                RecoveryEntry(
                    action="cleanup_stale_worktree",
                    issue=issue,
                    reason=reason,
                )
            )

    try:
        gh_run([
            "issue", "edit", str(issue),
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])
    except Exception as exc:
        log_warning(f"Failed to update labels for issue #{issue}: {exc}")
        return

    ts = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    actions = [
        "- Removed `loom:building` label",
        "- Added `loom:issue` label to return to ready queue",
    ]
    if worktree_cleaned:
        actions.append("- Cleaned up stale worktree and branches")

    comment = (
        "## Orphan Recovery\n\n"
        "This issue was automatically recovered from an orphaned state.\n\n"
        f"**Reason**: {reason}\n"
        "**What happened**:\n"
        "- The spawn-loop task that was working on this issue "
        "crashed or was terminated\n"
        "- The issue was left in `loom:building` state with no active worker\n\n"
        "**Action taken**:\n"
        + "\n".join(actions)
        + "\n\n"
        "This issue is now available for a new sweep to pick up.\n\n"
        "---\n"
        f"*Recovered by loom-recover-orphans at {ts}*"
    )

    if not _has_recent_orphan_comment(issue):
        try:
            gh_run(["issue", "comment", str(issue), "--body", comment])
        except Exception as exc:
            log_warning(f"Failed to add comment to issue #{issue}: {exc}")

    result.recovered.append(
        RecoveryEntry(
            action="reset_issue_label",
            issue=issue,
            reason=reason,
        )
    )

    log_success(f"Recovered issue #{issue}")


def run_orphan_recovery(
    repo_root: pathlib.Path,
    *,
    recover: bool = False,
    verbose: bool = False,
) -> OrphanRecoveryResult:
    """Run all orphan detection phases and optionally recover.

    Reads ``.loom/spawn-loop-state.json`` (Phase 1, #3374) and cross-checks
    against ``gh issue list --label loom:building``.

    Known invocation paths after Phase 3.1.6 (#3395):

    - CLI: ``./.loom/scripts/recover-orphaned-shepherds.sh [--recover]``
      (script is a thin stub delegating here).
    - Pre-Phase-3 daemon callers (``daemon_v2/iteration.py``,
      ``daemon_cleanup.py``) still call ``run_orphan_recovery`` with the
      same ``(repo_root, *, recover, verbose)`` signature; they retire as a
      unit in Phase 3.3.

    Returns an :class:`OrphanRecoveryResult` with all detected orphans and
    any recovery actions taken.
    """
    result = OrphanRecoveryResult(recover_mode=recover)
    heartbeat_threshold = _get_heartbeat_stale_threshold()
    label_grace_period = _get_label_grace_period()

    spawn_loop_state = read_spawn_loop_state(repo_root)

    if not spawn_loop_state.present and verbose:
        log_info(
            "No .loom/spawn-loop-state.json found — assuming no "
            "spawn-loop tasks. Proceeding to forge cross-check only."
        )
        # An absent state file means "nothing tracked locally"; the forge
        # cross-check still runs and may surface untracked-building orphans.

    # Phase A: cross-check loom:building issues against spawn-loop tasks.
    check_untracked_building(
        spawn_loop_state,
        result,
        repo_root=repo_root,
        label_grace_period=label_grace_period,
        verbose=verbose,
    )

    # Phase B: flag spawn-loop tasks with stale heartbeats whose PID is dead.
    check_stale_heartbeats(
        spawn_loop_state,
        result,
        heartbeat_threshold=heartbeat_threshold,
        verbose=verbose,
    )

    if not recover:
        return result

    # Perform recovery for detected orphans.  Both orphan types resolve to
    # the same recovery action: flip the issue label back to loom:issue so
    # a new sweep can pick it up.
    for orphan in list(result.orphaned):
        if orphan.issue:
            recover_issue(
                orphan.issue,
                orphan.reason,
                result,
                repo_root=repo_root,
                label_grace_period=label_grace_period,
            )

    return result


def format_result_json(result: OrphanRecoveryResult) -> str:
    """Format result as JSON string."""
    return json.dumps(result.to_dict(), indent=2)


def format_result_human(result: OrphanRecoveryResult) -> str:
    """Format result as human-readable text."""
    lines: list[str] = []

    if result.total_orphaned == 0:
        lines.append("No orphaned tasks found")
    else:
        lines.append(f"Found {result.total_orphaned} orphaned task(s)")
        lines.append("")

        for orphan in result.orphaned:
            if orphan.type == "untracked_building":
                lines.append(
                    f"  [{orphan.type}] #{orphan.issue}: "
                    f"{orphan.title or 'no title'} "
                    f"-- no active spawn-loop task"
                )
            elif orphan.type == "stale_heartbeat":
                age_str = format_duration(orphan.age_seconds or 0)
                lines.append(
                    f"  [{orphan.type}] issue #{orphan.issue} "
                    f"(pid {orphan.pid}): heartbeat stale ({age_str})"
                )

        if result.recover_mode:
            lines.append("")
            lines.append(f"Recovered {result.total_recovered} item(s)")
        else:
            lines.append("")
            lines.append("Run with --recover to fix these issues")

    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Main entry point for orphan recovery CLI."""
    parser = argparse.ArgumentParser(
        description="Detect and recover orphaned spawn-loop task state",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Exit codes:
    0 - No orphans detected
    1 - Error occurred
    2 - Orphans detected

Orphan types:
    untracked_building  - Issue has loom:building but no spawn-loop task
    stale_heartbeat     - Spawn-loop task heartbeat is stale and pid is dead

Recovery actions:
    reset_issue_label       - Swap loom:building -> loom:issue on issue
    cleanup_stale_worktree  - Remove stale worktree + branches (0 commits, no changes)

Sources of truth:
    .loom/spawn-loop-state.json           - Live spawn-loop tasks (Phase 1, #3374)
    gh issue list --label loom:building   - Forge label cross-check

Environment variables:
    LOOM_HEARTBEAT_STALE_THRESHOLD  Seconds before heartbeat is stale (default: 300)
    LOOM_LABEL_GRACE_PERIOD         Seconds to skip recently-labeled issues (default: 600)
""",
    )

    parser.add_argument(
        "--recover",
        action="store_true",
        help="Actually perform recovery (default is dry-run)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output JSON for programmatic use",
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Show detailed progress",
    )

    args = parser.parse_args(argv)

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    if not args.json:
        log_info("Orphaned Spawn-Loop Task Detection & Recovery")
        if not args.recover:
            log_info("DRY RUN - No changes will be made")
            log_info("Use --recover to actually perform recovery")

    try:
        result = run_orphan_recovery(
            repo_root,
            recover=args.recover,
            verbose=args.verbose,
        )
    except Exception as exc:
        log_error(f"Error during orphan recovery: {exc}")
        return 1

    if args.json:
        print(format_result_json(result))
    else:
        print(format_result_human(result))

    if result.total_orphaned > 0 and not args.recover:
        return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
