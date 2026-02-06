"""Spawn shepherds for ready issues."""

from __future__ import annotations

import subprocess
from typing import TYPE_CHECKING, Any

from loom_tools.agent_spawn import spawn_agent, session_exists, kill_stuck_session
from loom_tools.common.github import gh_run
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.time_utils import now_utc
from loom_tools.models.daemon_state import ShepherdEntry

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext


def spawn_shepherds(ctx: DaemonContext) -> int:
    """Spawn shepherds for ready issues.

    Returns the number of shepherds successfully spawned.
    """
    if ctx.snapshot is None or ctx.state is None:
        return 0

    available_slots = ctx.get_available_shepherd_slots()
    ready_issues = ctx.get_ready_issues()

    if available_slots <= 0:
        log_info("No available shepherd slots")
        return 0

    if not ready_issues:
        log_info("No ready issues to assign")
        return 0

    # Limit to available slots
    issues_to_spawn = ready_issues[:available_slots]
    spawned = 0

    for issue in issues_to_spawn:
        issue_num = issue.get("number")
        if issue_num is None:
            continue

        # Find an idle shepherd slot
        shepherd_name = _find_idle_shepherd(ctx)
        if shepherd_name is None:
            log_warning("No idle shepherd slot available")
            break

        success = _spawn_single_shepherd(ctx, shepherd_name, issue_num)
        if success:
            spawned += 1

    return spawned


def _find_idle_shepherd(ctx: DaemonContext) -> str | None:
    """Find an idle shepherd slot in daemon state.

    Creates new shepherd entries if needed up to max_shepherds.
    """
    if ctx.state is None:
        return None

    # Check existing shepherds for idle slots
    for name, entry in ctx.state.shepherds.items():
        if entry.status == "idle":
            return name

    # Check if we can create a new shepherd
    current_count = len(ctx.state.shepherds)
    if current_count < ctx.config.max_shepherds:
        new_name = f"shepherd-{current_count + 1}"
        ctx.state.shepherds[new_name] = ShepherdEntry(status="idle")
        return new_name

    return None


def _spawn_single_shepherd(
    ctx: DaemonContext,
    shepherd_name: str,
    issue_num: int,
) -> bool:
    """Spawn a single shepherd for an issue.

    Returns True if successful.
    """
    if ctx.state is None:
        return False

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Claim the issue
    if not _claim_issue(issue_num):
        log_error(f"Failed to claim issue #{issue_num}")
        return False

    log_info(f"Claimed issue #{issue_num} for {shepherd_name}")

    # Build spawn arguments
    args = str(issue_num)
    if ctx.config.force_mode:
        args += " --force"
    args += " --allow-dirty-main"

    # Check for existing session and handle it
    if session_exists(shepherd_name):
        log_info(f"Killing existing session for {shepherd_name}")
        kill_stuck_session(shepherd_name)

    # Spawn the shepherd
    result = spawn_agent(
        role="shepherd",
        name=shepherd_name,
        args=args,
        worktree="",  # Shepherd creates its own worktree
        repo_root=ctx.repo_root,
    )

    if result.status == "error":
        log_error(f"Failed to spawn {shepherd_name}: {result.error}")
        # Unclaim the issue
        _unclaim_issue(issue_num)
        return False

    # Update daemon state
    entry = ctx.state.shepherds.get(shepherd_name)
    if entry is None:
        entry = ShepherdEntry()
        ctx.state.shepherds[shepherd_name] = entry

    entry.status = "working"
    entry.issue = issue_num
    entry.task_id = _extract_task_id(result.session)
    entry.output_file = result.log
    entry.started = timestamp
    entry.last_phase = "started"
    entry.execution_mode = "tmux"
    entry.idle_since = None
    entry.idle_reason = None

    log_success(f"Spawned {shepherd_name} for issue #{issue_num}")
    return True


def _claim_issue(issue_num: int) -> bool:
    """Claim an issue by swapping labels.

    Returns True if successful.
    """
    try:
        gh_run([
            "issue", "edit", str(issue_num),
            "--remove-label", "loom:issue",
            "--add-label", "loom:building",
        ])
        return True
    except Exception as e:
        log_error(f"Failed to claim issue #{issue_num}: {e}")
        return False


def _unclaim_issue(issue_num: int) -> None:
    """Unclaim an issue (revert label swap on failure)."""
    try:
        gh_run([
            "issue", "edit", str(issue_num),
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])
    except Exception:
        pass


def force_reclaim_stale_shepherds(ctx: DaemonContext) -> int:
    """Force reclaim shepherds with stale heartbeats.

    Checks each 'working' shepherd for:
    1. Dead tmux sessions (no session or no claude process)
    2. Stale heartbeats from progress files

    For each stale shepherd found:
    - Kills the tmux session if it exists
    - Resets the shepherd entry to idle
    - Reverts the issue label from loom:building to loom:issue

    Returns the number of shepherds reclaimed.
    """
    if ctx.state is None or ctx.snapshot is None:
        return 0

    reclaimed = 0
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Get stale heartbeat info from snapshot
    shepherd_progress = (
        ctx.snapshot.get("shepherds", {}).get("progress", [])
    )
    stale_task_ids: set[str] = set()
    for prog in shepherd_progress:
        if prog.get("heartbeat_stale", False):
            tid = prog.get("task_id", "")
            if tid:
                stale_task_ids.add(tid)

    for name, entry in list(ctx.state.shepherds.items()):
        if entry.status != "working":
            continue

        is_stale = False

        # Check 1: tmux session dead
        if not session_exists(name):
            log_warning(f"STALL-L2: {name} has no tmux session")
            is_stale = True
        else:
            # Check if claude is actually running
            session_name = f"loom-{name}"
            from loom_tools.agent_spawn import _get_pane_pid, _is_claude_running

            shell_pid = _get_pane_pid(session_name)
            if not shell_pid or not _is_claude_running(shell_pid):
                log_warning(f"STALL-L2: {name} tmux session has no active claude process")
                is_stale = True

        # Check 2: stale heartbeat from progress files
        if not is_stale and entry.task_id and entry.task_id in stale_task_ids:
            log_warning(f"STALL-L2: {name} has stale heartbeat (task_id={entry.task_id})")
            is_stale = True

        if not is_stale:
            continue

        # Kill tmux session if it exists
        if session_exists(name):
            log_info(f"STALL-L2: Killing tmux session for {name}")
            kill_stuck_session(name)

        # Reset shepherd entry to idle
        issue = entry.issue
        entry.status = "idle"
        entry.issue = None
        entry.task_id = None
        entry.output_file = None
        entry.idle_since = timestamp
        entry.idle_reason = "stall_recovery"
        entry.last_issue = issue
        entry.last_completed = timestamp

        log_info(f"STALL-L2: Reset {name} to idle")

        # Revert issue label
        if issue is not None:
            try:
                _unclaim_issue(issue)
                log_info(f"STALL-L2: Reverted issue #{issue} labels to loom:issue")
            except Exception as e:
                log_warning(f"STALL-L2: Failed to revert labels for issue #{issue}: {e}")

        reclaimed += 1

    return reclaimed


def _extract_task_id(session_name: str) -> str | None:
    """Extract task ID from spawn result.

    The task ID is a 7-character hex string generated by the Claude CLI.
    Since we don't have direct access to it from spawn_agent, we generate
    a placeholder that will be updated when the progress file is created.
    """
    import hashlib
    import time

    # Generate a 7-char hex ID based on session name and time
    # This will be replaced by the actual task ID from progress files
    data = f"{session_name}-{time.time()}".encode()
    return hashlib.sha256(data).hexdigest()[:7]
