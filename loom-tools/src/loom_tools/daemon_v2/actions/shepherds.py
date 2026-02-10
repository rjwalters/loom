"""Spawn shepherds for ready issues."""

from __future__ import annotations

import subprocess
from typing import TYPE_CHECKING, Any

from loom_tools.agent_spawn import (
    capture_tmux_output,
    kill_stuck_session,
    session_exists,
    spawn_agent,
)
from loom_tools.common.github import gh_run
from loom_tools.common.issue_failures import record_failure
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.time_utils import now_utc, parse_iso_timestamp
from loom_tools.models.daemon_state import ShepherdEntry

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext

# Tier 1: Early warning for shepherds that may have failed at startup.
# After this many seconds without a progress file, log a warning and
# capture tmux output but do NOT kill the session.
STARTUP_GRACE_PERIOD = 120  # 2 minutes

# Tier 2: Hard reclaim for shepherds that never created a progress file.
# After this many seconds, kill the session and save diagnostic output.
NO_PROGRESS_GRACE_PERIOD = 300  # 5 minutes


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


def _has_existing_checkpoint(repo_root: "pathlib.Path", issue_num: int) -> bool:
    """Check if a previous shepherd left a checkpoint for this issue."""
    worktree_path = repo_root / ".loom" / "worktrees" / f"issue-{issue_num}"
    checkpoint_file = worktree_path / ".loom-checkpoint"
    return checkpoint_file.is_file()


def _has_existing_branch(repo_root: "pathlib.Path", issue_num: int) -> bool:
    """Check if a feature branch exists on remote for this issue."""
    branch_name = f"feature/issue-{issue_num}"
    try:
        result = subprocess.run(
            ["git", "ls-remote", "--heads", "origin", branch_name],
            capture_output=True, text=True, timeout=15,
            cwd=repo_root,
        )
        return bool(result.stdout.strip())
    except (subprocess.TimeoutExpired, OSError):
        return False


def _spawn_single_shepherd(
    ctx: DaemonContext,
    shepherd_name: str,
    issue_num: int,
) -> bool:
    """Spawn a single shepherd for an issue.

    Detects existing checkpoints and feature branches from prior attempts
    and passes --resume flag so the shepherd can pick up where it left off.

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

    # Check for prior work (checkpoint or remote branch)
    has_checkpoint = _has_existing_checkpoint(ctx.repo_root, issue_num)
    has_branch = _has_existing_branch(ctx.repo_root, issue_num)

    # Build spawn arguments
    args = str(issue_num)
    if ctx.config.force_mode:
        args += " --force"
    args += " --allow-dirty-main"

    if has_checkpoint or has_branch:
        args += " --resume"
        log_info(
            f"Issue #{issue_num} has prior work "
            f"(checkpoint={has_checkpoint}, branch={has_branch}) - passing --resume"
        )

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


def _has_budget_exhaustion_warning(snapshot: dict[str, Any]) -> bool:
    """Check if the snapshot health warnings include session_budget_low."""
    health_warnings = snapshot.get("computed", {}).get("health_warnings", [])
    return any(w.get("code") == "session_budget_low" for w in health_warnings)


def _preserve_wip(repo_root: "pathlib.Path", issue: int) -> bool:
    """Commit and push WIP changes in a shepherd's worktree before kill.

    Returns True if WIP was preserved (committed and/or pushed).
    """
    worktree_path = repo_root / ".loom" / "worktrees" / f"issue-{issue}"
    if not worktree_path.is_dir():
        return False

    try:
        # Check for uncommitted changes
        result = subprocess.run(
            ["git", "status", "--porcelain"],
            capture_output=True, text=True, timeout=15,
            cwd=worktree_path,
        )
        has_changes = bool(result.stdout.strip())

        if has_changes:
            # Stage and commit WIP
            subprocess.run(
                ["git", "add", "-A"],
                capture_output=True, timeout=15,
                cwd=worktree_path,
            )
            subprocess.run(
                ["git", "commit", "-m", "WIP: budget exhausted - preserving partial progress"],
                capture_output=True, timeout=30,
                cwd=worktree_path,
            )
            log_info(f"STALL-L2: Committed WIP for issue #{issue}")

        # Push the branch if it has a remote tracking branch or commits ahead
        branch_result = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True, text=True, timeout=10,
            cwd=worktree_path,
        )
        branch_name = branch_result.stdout.strip()
        if branch_name:
            push_result = subprocess.run(
                ["git", "push", "-u", "origin", branch_name],
                capture_output=True, timeout=60,
                cwd=worktree_path,
            )
            if push_result.returncode == 0:
                log_info(f"STALL-L2: Pushed WIP branch {branch_name} for issue #{issue}")
                return True

        return has_changes
    except (subprocess.TimeoutExpired, OSError) as e:
        log_warning(f"STALL-L2: Failed to preserve WIP for issue #{issue}: {e}")
        return False


def force_reclaim_stale_shepherds(ctx: DaemonContext) -> int:
    """Force reclaim shepherds with stale heartbeats.

    Checks each 'working' shepherd for:
    1. Dead tmux sessions (no session or no claude process)
    2. Stale heartbeats from progress files

    For each stale shepherd found:
    - Detects if stale due to budget exhaustion (session_budget_low)
    - Preserves WIP (commits and pushes) for budget-exhausted shepherds
    - Captures tmux output for post-mortem analysis
    - Kills the tmux session if it exists
    - Records failure with appropriate error_class
    - Resets the shepherd entry to idle
    - Reverts the issue label from loom:building to loom:issue

    Returns the number of shepherds reclaimed.
    """
    if ctx.state is None or ctx.snapshot is None:
        return 0

    reclaimed = 0
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Detect budget exhaustion from snapshot health warnings
    is_budget_exhausted = _has_budget_exhaustion_warning(ctx.snapshot)

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

        # Check 3: no progress file after grace period
        if not is_stale and entry.started and entry.task_id:
            is_stale = _check_no_progress_file(ctx, name, entry)

        if not is_stale:
            continue

        issue = entry.issue

        # Determine error class for this stale shepherd
        error_class = "budget_exhausted" if is_budget_exhausted else "shepherd_failure"

        # Preserve WIP before killing (especially important for budget exhaustion)
        if issue is not None and is_budget_exhausted:
            _preserve_wip(ctx.repo_root, issue)

        # Capture tmux output before killing for post-mortem analysis
        if session_exists(name):
            _save_diagnostic_output(ctx, name)
            log_info(f"STALL-L2: Killing tmux session for {name}")
            kill_stuck_session(name)

        # Record failure in persistent log
        if issue is not None:
            phase = entry.last_phase or "unknown"
            record_failure(
                ctx.repo_root,
                issue,
                error_class=error_class,
                phase=phase,
                details=f"Shepherd {name} stale during {phase} phase ({error_class})",
            )
            log_info(f"STALL-L2: Recorded {error_class} failure for issue #{issue}")

        # Reset shepherd entry to idle
        entry.status = "idle"
        entry.issue = None
        entry.task_id = None
        entry.output_file = None
        entry.idle_since = timestamp
        entry.idle_reason = "stall_recovery"
        entry.last_issue = issue
        entry.last_completed = timestamp
        entry.startup_warning_at = None

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


def _check_no_progress_file(
    ctx: DaemonContext,
    name: str,
    entry: ShepherdEntry,
) -> bool:
    """Check if a working shepherd has no progress file past the grace period.

    Implements two-tier detection:
      Tier 1 (startup_grace_period, default 120s): Log a warning, capture tmux
        output snapshot, and set ``startup_warning_at`` but do NOT reclaim.
      Tier 2 (no_progress_grace_period, default 300s): Save diagnostic output
        to ``.loom/logs/`` and return True to trigger reclaim.

    Returns True if the shepherd should be considered stale (Tier 2).
    """
    if entry.started is None or entry.task_id is None:
        return False

    now = now_utc()
    try:
        started_dt = parse_iso_timestamp(entry.started)
        spawn_age = int((now - started_dt).total_seconds())
    except (ValueError, OSError):
        return False

    startup_grace = ctx.config.startup_grace_period
    hard_reclaim_grace = ctx.config.no_progress_grace_period

    if spawn_age < startup_grace:
        return False

    # Check if a progress file exists for this shepherd's task_id
    has_progress = _has_progress_data(ctx, entry)

    if has_progress:
        return False

    # --- No progress file found ---

    # Tier 1: Early warning (past startup grace, before hard reclaim)
    if spawn_age < hard_reclaim_grace:
        if entry.startup_warning_at is None:
            entry.startup_warning_at = now.strftime("%Y-%m-%dT%H:%M:%SZ")
            log_warning(
                f"STALL-T1: {name} has no progress file after {spawn_age}s "
                f"(task_id={entry.task_id}, issue=#{entry.issue}) — monitoring"
            )
            # Capture a tmux output snapshot for debugging
            if session_exists(name):
                output = capture_tmux_output(name)
                if output.strip():
                    log_info(
                        f"STALL-T1: {name} tmux snapshot (last lines): "
                        + output.strip().splitlines()[-1][:200]
                    )
        return False

    # Tier 2: Hard reclaim
    log_warning(
        f"STALL-T2: {name} has no progress file after {spawn_age}s "
        f"(task_id={entry.task_id}, issue=#{entry.issue}) — reclaiming"
    )
    _save_diagnostic_output(ctx, name)
    return True


def _has_progress_data(ctx: DaemonContext, entry: ShepherdEntry) -> bool:
    """Check if a shepherd has any associated progress data.

    Checks both the filesystem progress file and the snapshot progress entries
    since the task_id in daemon-state may not match the progress file name.
    """
    progress_file = ctx.repo_root / ".loom" / "progress" / f"shepherd-{entry.task_id}.json"
    if progress_file.exists():
        return True

    # Fall back to checking if any snapshot progress entry tracks this issue
    shepherd_progress = (
        ctx.snapshot.get("shepherds", {}).get("progress", [])
        if ctx.snapshot
        else []
    )
    return any(
        prog.get("issue") == entry.issue
        for prog in shepherd_progress
    )


def _save_diagnostic_output(ctx: DaemonContext, name: str) -> None:
    """Capture tmux output and save to a diagnostic log file.

    Saves to ``.loom/logs/stall-diagnostic-{name}-{timestamp}.log`` for
    post-mortem analysis of stuck/failed shepherd sessions.
    """
    output = capture_tmux_output(name, lines=500)
    if not output.strip():
        log_info(f"STALL: No tmux output to capture for {name}")
        return

    log_dir = ctx.repo_root / ".loom" / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)

    ts = now_utc().strftime("%Y%m%d-%H%M%S")
    diag_file = log_dir / f"stall-diagnostic-{name}-{ts}.log"
    try:
        diag_file.write_text(output)
        log_info(f"STALL: Saved diagnostic output to {diag_file}")
    except OSError as e:
        log_warning(f"STALL: Failed to save diagnostic output for {name}: {e}")


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
