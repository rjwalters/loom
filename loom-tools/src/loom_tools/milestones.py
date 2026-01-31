"""Report shepherd progress milestones to ``.loom/progress/`` JSON files.

Provides both a programmatic API (``report_milestone()``) and a CLI
entry point (``main()``) registered as ``loom-milestone``.

Events
------
started, phase_entered, phase_completed, worktree_created, first_commit,
pr_created, heartbeat, completed, blocked, error
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.paths import LoomPaths
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import now_utc
from loom_tools.models.progress import Milestone, ShepherdProgress

_TASK_ID_RE = re.compile(r"^[a-f0-9]{7}$")

VALID_EVENTS = frozenset(
    {
        "started",
        "phase_entered",
        "phase_completed",
        "worktree_created",
        "first_commit",
        "pr_created",
        "heartbeat",
        "completed",
        "blocked",
        "error",
    }
)

# Required keyword arguments per event.
_REQUIRED: dict[str, set[str]] = {
    "started": {"issue"},
    "phase_entered": {"phase"},
    "phase_completed": {"phase"},
    "worktree_created": {"path"},
    "first_commit": {"sha"},
    "pr_created": {"pr_number"},
    "heartbeat": {"action"},
    "completed": set(),
    "blocked": {"reason"},
    "error": {"error"},
}


def _get_timestamp() -> str:
    return now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")


def _validate_task_id(task_id: str) -> None:
    if not _TASK_ID_RE.match(task_id):
        raise ValueError(
            f"Invalid task_id '{task_id}' — must be exactly 7 lowercase hex characters"
        )


def _progress_path(repo_root: pathlib.Path, task_id: str) -> pathlib.Path:
    return LoomPaths(repo_root).progress_file(task_id)


def _ensure_progress_dir(repo_root: pathlib.Path) -> None:
    LoomPaths(repo_root).progress_dir.mkdir(parents=True, exist_ok=True)


def _build_milestone_data(event: str, **kwargs: Any) -> dict[str, Any]:
    """Build the ``data`` dict for a milestone entry."""
    data: dict[str, Any] = {}
    if event == "started":
        data["issue"] = int(kwargs["issue"])
        data["mode"] = kwargs.get("mode", "")
    elif event == "phase_entered":
        data["phase"] = kwargs["phase"]
    elif event == "phase_completed":
        data["phase"] = kwargs["phase"]
        if "duration_seconds" in kwargs:
            data["duration_seconds"] = int(kwargs["duration_seconds"])
        if "status" in kwargs:
            data["status"] = kwargs["status"]
    elif event == "worktree_created":
        data["path"] = kwargs["path"]
    elif event == "first_commit":
        data["sha"] = kwargs["sha"]
    elif event == "pr_created":
        data["pr_number"] = int(kwargs["pr_number"])
    elif event == "heartbeat":
        data["action"] = kwargs["action"]
    elif event == "completed":
        data["pr_merged"] = bool(kwargs.get("pr_merged", False))
    elif event == "blocked":
        data["reason"] = kwargs["reason"]
        data["details"] = kwargs.get("details", "")
    elif event == "error":
        data["error"] = kwargs["error"]
        data["will_retry"] = bool(kwargs.get("will_retry", False))
    return data


def _apply_state_updates(
    progress: ShepherdProgress,
    event: str,
    data: dict[str, Any],
    timestamp: str,
) -> None:
    """Mutate *progress* to reflect the state changes for *event*."""
    progress.last_heartbeat = timestamp
    if event == "phase_entered":
        progress.current_phase = data["phase"]
    elif event == "completed":
        progress.status = "completed"
        progress.current_phase = None  # type: ignore[assignment]
    elif event == "blocked":
        progress.status = "blocked"
    elif event == "error":
        progress.status = "retrying" if data.get("will_retry") else "errored"


# ── Programmatic API ────────────────────────────────────────────


def report_milestone(
    repo_root: pathlib.Path,
    task_id: str,
    event: str,
    *,
    quiet: bool = False,
    **kwargs: Any,
) -> bool:
    """Report a shepherd progress milestone.

    Parameters
    ----------
    repo_root:
        Repository root (must contain ``.loom/``).
    task_id:
        7-character lowercase hex shepherd task ID.
    event:
        One of the nine supported event types.
    quiet:
        Suppress informational output on success.
    **kwargs:
        Event-specific arguments (``issue``, ``phase``, etc.).

    Returns
    -------
    bool
        ``True`` on success, ``False`` on error.
    """
    try:
        _validate_task_id(task_id)

        if event not in VALID_EVENTS:
            log_error(f"Unknown event '{event}'")
            return False

        # Check required kwargs
        missing = _REQUIRED[event] - set(kwargs)
        if missing:
            names = ", ".join(f"--{k.replace('_', '-')}" for k in sorted(missing))
            log_error(f"Missing required argument(s) for '{event}': {names}")
            return False

        _ensure_progress_dir(repo_root)
        timestamp = _get_timestamp()
        data = _build_milestone_data(event, **kwargs)
        progress_file = _progress_path(repo_root, task_id)

        if event == "started":
            progress = ShepherdProgress(
                task_id=task_id,
                issue=int(kwargs["issue"]),
                mode=kwargs.get("mode", ""),
                started_at=timestamp,
                current_phase="started",
                last_heartbeat=timestamp,
                status="working",
                milestones=[
                    Milestone(event="started", timestamp=timestamp, data=data),
                ],
            )
            write_json_file(progress_file, progress.to_dict())

            if not quiet:
                log_success(
                    f"Started tracking shepherd {task_id} for issue #{kwargs['issue']}"
                )
            return True

        # All other events require an existing progress file.
        if not progress_file.is_file():
            log_error(f"No progress file found for task {task_id}")
            log_error(
                f"Run 'report-milestone.sh started --task-id {task_id} --issue N' first"
            )
            return False

        raw = read_json_file(progress_file)
        if not isinstance(raw, dict):
            log_error("Progress file is corrupted")
            return False

        progress = ShepherdProgress.from_dict(raw)
        milestone = Milestone(event=event, timestamp=timestamp, data=data)
        progress.milestones.append(milestone)
        _apply_state_updates(progress, event, data, timestamp)
        write_json_file(progress_file, progress.to_dict())

        if not quiet:
            _log_event(event, data)

        return True

    except ValueError as exc:
        log_error(str(exc))
        return False
    except Exception as exc:
        log_error(f"Unexpected error: {exc}")
        return False


def _log_event(event: str, data: dict[str, Any]) -> None:
    """Emit a coloured log line appropriate for *event*."""
    if event == "phase_entered":
        log_info(f"Phase: {data['phase']}")
    elif event == "phase_completed":
        duration = data.get("duration_seconds", "")
        status = data.get("status", "")
        suffix = f" ({duration}s, {status})" if duration and status else ""
        log_success(f"Phase completed: {data['phase']}{suffix}")
    elif event == "worktree_created":
        log_info(f"Worktree created: {data['path']}")
    elif event == "first_commit":
        log_info(f"First commit: {data['sha']}")
    elif event == "pr_created":
        log_info(f"PR created: #{data['pr_number']}")
    elif event == "heartbeat":
        log_info(f"Heartbeat: {data['action']}")
    elif event == "completed":
        log_success("Completed")
    elif event == "blocked":
        log_warning(f"Blocked: {data['reason']}")
    elif event == "error":
        log_error(f"Error: {data['error']}")


# ── CLI ─────────────────────────────────────────────────────────


def main(argv: list[str] | None = None) -> int:
    """CLI entry point (registered as ``loom-milestone``)."""
    parser = argparse.ArgumentParser(
        description="Report shepherd progress milestones",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Events:
  started             --task-id ID --issue NUM [--mode MODE]
  phase_entered       --task-id ID --phase PHASE
  phase_completed     --task-id ID --phase PHASE [--duration-seconds N] [--status S]
  worktree_created    --task-id ID --path PATH
  first_commit        --task-id ID --sha SHA
  pr_created          --task-id ID --pr-number NUM
  heartbeat           --task-id ID --action "description"
  completed           --task-id ID [--pr-merged]
  blocked             --task-id ID --reason "reason" [--details "details"]
  error               --task-id ID --error "message" [--will-retry]

Examples:
  loom-milestone started --task-id abc1234 --issue 42 --mode force-pr
  loom-milestone phase_entered --task-id abc1234 --phase builder
  loom-milestone phase_completed --task-id abc1234 --phase builder --duration-seconds 120 --status success
  loom-milestone heartbeat --task-id abc1234 --action "running tests"
  loom-milestone completed --task-id abc1234 --pr-merged
  loom-milestone error --task-id abc1234 --error "build failed" --will-retry
""",
    )

    parser.add_argument(
        "event",
        nargs="?",
        choices=sorted(VALID_EVENTS),
        help="Milestone event type",
    )
    parser.add_argument(
        "--task-id", required=False, help="Shepherd task ID (7 hex chars)"
    )
    parser.add_argument("--issue", type=int, help="Issue number (for 'started')")
    parser.add_argument("--mode", help="Orchestration mode (for 'started')")
    parser.add_argument(
        "--phase", help="Phase name (for 'phase_entered', 'phase_completed')"
    )
    parser.add_argument(
        "--duration-seconds",
        type=int,
        help="Phase duration in seconds (for 'phase_completed')",
    )
    parser.add_argument(
        "--status", help="Phase completion status (for 'phase_completed')"
    )
    parser.add_argument("--path", help="Worktree path (for 'worktree_created')")
    parser.add_argument("--sha", help="Commit SHA (for 'first_commit')")
    parser.add_argument("--pr-number", type=int, help="PR number (for 'pr_created')")
    parser.add_argument("--action", help="Action description (for 'heartbeat')")
    parser.add_argument(
        "--pr-merged", action="store_true", help="PR was merged (for 'completed')"
    )
    parser.add_argument("--reason", help="Block reason (for 'blocked')")
    parser.add_argument("--details", help="Additional details (for 'blocked')")
    parser.add_argument("--error", dest="error_msg", help="Error message (for 'error')")
    parser.add_argument(
        "--will-retry", action="store_true", help="Error is recoverable (for 'error')"
    )
    parser.add_argument(
        "--quiet", "-q", action="store_true", help="Suppress output on success"
    )

    args = parser.parse_args(argv)

    if not args.event:
        parser.print_help()
        return 0

    if not args.task_id:
        log_error("--task-id is required")
        return 1

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    # Collect event-specific kwargs from parsed args.
    kwargs: dict[str, Any] = {}
    if args.issue is not None:
        kwargs["issue"] = args.issue
    if args.mode is not None:
        kwargs["mode"] = args.mode
    if args.phase is not None:
        kwargs["phase"] = args.phase
    if args.duration_seconds is not None:
        kwargs["duration_seconds"] = args.duration_seconds
    if args.status is not None:
        kwargs["status"] = args.status
    if args.path is not None:
        kwargs["path"] = args.path
    if args.sha is not None:
        kwargs["sha"] = args.sha
    if args.pr_number is not None:
        kwargs["pr_number"] = args.pr_number
    if args.action is not None:
        kwargs["action"] = args.action
    if args.pr_merged:
        kwargs["pr_merged"] = True
    if args.reason is not None:
        kwargs["reason"] = args.reason
    if args.details is not None:
        kwargs["details"] = args.details
    if args.error_msg is not None:
        kwargs["error"] = args.error_msg
    if args.will_retry:
        kwargs["will_retry"] = True

    ok = report_milestone(
        repo_root, args.task_id, args.event, quiet=args.quiet, **kwargs
    )
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
