"""Single iteration logic for the daemon."""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.state import read_daemon_state, write_json_file
from loom_tools.common.time_utils import now_utc
from loom_tools.models.daemon_state import SystematicFailure
from loom_tools.daemon_v2.actions.completions import check_completions, handle_completion
from loom_tools.daemon_v2.actions.proposals import promote_proposals
from loom_tools.agent_spawn import kill_stuck_session, session_exists
from loom_tools.daemon_v2.actions.shepherds import (
    _unclaim_issue,
    force_reclaim_stale_shepherds,
    spawn_shepherds,
)
from loom_tools.daemon_v2.actions.support_roles import spawn_roles_from_actions
from loom_tools.orphan_recovery import run_orphan_recovery
from loom_tools.snapshot import build_snapshot

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext


@dataclass
class IterationResult:
    """Result of a single daemon iteration."""

    status: str  # "success", "failure", "shutdown"
    summary: str
    shepherds_spawned: int = 0
    support_roles_spawned: int = 0
    proposals_promoted: int = 0
    completions_handled: int = 0


def run_iteration(ctx: DaemonContext) -> IterationResult:
    """Execute a single daemon iteration.

    The iteration:
    1. Captures fresh snapshot (system state)
    2. Checks for completed shepherds/roles
    3. Executes recommended actions from snapshot
    4. Saves updated state
    5. Returns summary
    """
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # 1. Capture fresh snapshot
    log_info("Capturing system state...")
    try:
        ctx.snapshot = build_snapshot(repo_root=ctx.repo_root)
    except Exception as e:
        log_error(f"Failed to capture snapshot: {e}")
        return IterationResult(
            status="failure",
            summary=f"ERROR: snapshot failed: {e}",
        )

    # 2. Read daemon state
    ctx.state = read_daemon_state(ctx.repo_root)

    # Update last_poll timestamp
    ctx.state.last_poll = timestamp
    ctx.state.iteration = ctx.iteration

    # 3. Check completions
    completions = check_completions(ctx)
    for completion in completions:
        handle_completion(ctx, completion)

    # Recompute active shepherd count after completions modify ctx.state
    if completions and ctx.state is not None and ctx.snapshot is not None:
        active_shepherds = sum(
            1 for e in ctx.state.shepherds.values() if e.status == "working"
        )
        ctx.snapshot["computed"]["active_shepherds"] = active_shepherds
        ctx.snapshot["computed"]["available_shepherd_slots"] = max(
            0, ctx.config.max_shepherds - active_shepherds
        )

    # 4. Proactive stale shepherd reclaim (every iteration)
    #    Detects shepherds with dead tmux sessions, stale heartbeats, or
    #    missing progress files. Runs before action planning so reclaimed
    #    slots are available for new spawns.
    _reclaim_stale_shepherds(ctx)

    # 5. Get recommended actions
    actions = ctx.get_recommended_actions()
    if ctx.config.debug_mode:
        log_info(f"Recommended actions: {actions}")

    # Track results
    result = IterationResult(status="success", summary="")
    result.completions_handled = len(completions)

    # 6. Execute actions based on recommendations

    # Promote proposals (force mode only)
    if "promote_proposals" in actions and ctx.config.force_mode:
        result.proposals_promoted = promote_proposals(ctx)

    # Spawn shepherds
    if "spawn_shepherds" in actions:
        result.shepherds_spawned = spawn_shepherds(ctx)

    # Spawn support roles (handles all interval and demand triggers)
    result.support_roles_spawned = spawn_roles_from_actions(ctx)

    # Recover orphans
    if "recover_orphans" in actions:
        log_info("Running orphan recovery...")
        try:
            run_orphan_recovery(ctx.repo_root, recover=True, verbose=ctx.config.debug_mode)
        except Exception as e:
            log_warning(f"Orphan recovery failed: {e}")

    # 7. Stall escalation
    _update_stall_counter(ctx, result)

    # 8. Save state
    _save_daemon_state(ctx)

    # 9. Build summary
    result.summary = _build_summary(ctx, result)

    return result


def _reclaim_stale_shepherds(ctx: DaemonContext) -> None:
    """Proactively reclaim stale shepherds every iteration.

    Runs force_reclaim_stale_shepherds when any shepherd is in "working"
    status. This catches shepherds with dead sessions, stale heartbeats,
    or missing progress files without waiting for stall escalation.
    """
    if ctx.state is None or ctx.snapshot is None:
        return

    has_working = any(
        e.status == "working" for e in ctx.state.shepherds.values()
    )
    if not has_working:
        return

    reclaimed = force_reclaim_stale_shepherds(ctx)
    if reclaimed > 0:
        log_success(f"Proactive reclaim: freed {reclaimed} stale shepherd(s)")
        # Recompute active shepherd count after reclaim
        active_shepherds = sum(
            1 for e in ctx.state.shepherds.values() if e.status == "working"
        )
        ctx.snapshot["computed"]["active_shepherds"] = active_shepherds
        ctx.snapshot["computed"]["available_shepherd_slots"] = max(
            0, ctx.config.max_shepherds - active_shepherds
        )


def _save_daemon_state(ctx: DaemonContext) -> None:
    """Save the current daemon state to disk."""
    if ctx.state is None:
        return

    try:
        write_json_file(ctx.state_file, ctx.state.to_dict())
    except Exception as e:
        log_warning(f"Failed to save daemon state: {e}")


def _build_summary(ctx: DaemonContext, result: IterationResult) -> str:
    """Build a compact summary line for the iteration."""
    if ctx.snapshot is None:
        return "no snapshot"

    computed = ctx.snapshot.get("computed", {})

    parts = [
        f"ready={computed.get('total_ready', 0)}",
        f"building={computed.get('total_building', 0)}",
        f"blocked={computed.get('total_blocked', 0)}",
        f"shepherds={computed.get('active_shepherds', 0)}/{ctx.config.max_shepherds}",
    ]

    # Add spawned counts if any
    if result.shepherds_spawned > 0:
        parts.append(f"spawned_s={result.shepherds_spawned}")
    if result.support_roles_spawned > 0:
        parts.append(f"spawned_r={result.support_roles_spawned}")
    if result.proposals_promoted > 0:
        parts.append(f"promoted={result.proposals_promoted}")
    if result.completions_handled > 0:
        parts.append(f"completed={result.completions_handled}")

    # Add health status
    health = computed.get("health_status", "healthy")
    if health != "healthy":
        parts.append(f"health={health}")

    # Add warnings
    warnings = computed.get("health_warnings", [])
    for warn in warnings:
        code = warn.get("code", "")
        if code:
            parts.append(f"WARN:{code}")

    # Add stall counter if non-zero
    if ctx.consecutive_stalled > 0:
        parts.append(f"stalled={ctx.consecutive_stalled}")

    return " ".join(parts)


def _iteration_made_progress(result: IterationResult) -> bool:
    """Check whether the iteration made meaningful progress."""
    return (
        result.shepherds_spawned > 0
        or result.completions_handled > 0
        or result.proposals_promoted > 0
        or result.support_roles_spawned > 0
    )


def _update_stall_counter(ctx: DaemonContext, result: IterationResult) -> None:
    """Update the consecutive stalled counter and trigger escalation if needed.

    An iteration is "stalled" when health_status != "healthy" AND no
    meaningful work was done. The counter resets when progress is made.
    """
    if ctx.snapshot is None:
        return

    health = ctx.snapshot.get("computed", {}).get("health_status", "healthy")
    made_progress = _iteration_made_progress(result)

    if made_progress:
        if ctx.consecutive_stalled > 0:
            log_info(
                f"Stall counter reset (was {ctx.consecutive_stalled}): "
                "meaningful progress detected"
            )
        ctx.consecutive_stalled = 0
        return

    if health in ("healthy", "degraded"):
        ctx.consecutive_stalled = 0
        return

    # Stalled: warning-level issues present and no progress
    ctx.consecutive_stalled += 1
    log_warning(
        f"Consecutive stalled iterations: {ctx.consecutive_stalled} "
        f"(health={health})"
    )

    # Trigger escalation levels
    if ctx.consecutive_stalled >= ctx.config.stall_restart_threshold:
        _escalate_level_3(ctx)
    elif ctx.consecutive_stalled >= ctx.config.stall_recovery_threshold:
        _escalate_level_2(ctx)
    elif ctx.consecutive_stalled >= ctx.config.stall_diagnostic_threshold:
        _escalate_level_1(ctx)


def _escalate_level_1(ctx: DaemonContext) -> None:
    """Level 1 (diagnostic): Log detailed diagnostics about stale shepherds."""
    log_warning("STALL-L1: Detailed diagnostics for stalled pipeline")

    if ctx.state is None or ctx.snapshot is None:
        return

    # Log shepherd status details
    for name, entry in ctx.state.shepherds.items():
        if entry.status != "working":
            continue

        # Check tmux session
        tmux_alive = session_exists(name)
        log_info(
            f"STALL-L1: {name}: issue=#{entry.issue}, "
            f"task_id={entry.task_id}, tmux={'alive' if tmux_alive else 'DEAD'}"
        )

    # Log heartbeat status from snapshot progress
    shepherd_progress = ctx.snapshot.get("shepherds", {}).get("progress", [])
    for prog in shepherd_progress:
        age = prog.get("heartbeat_age_seconds", 0)
        stale = prog.get("heartbeat_stale", False)
        log_info(
            f"STALL-L1: task={prog.get('task_id', '?')}, "
            f"issue=#{prog.get('issue', '?')}, "
            f"phase={prog.get('current_phase', '?')}, "
            f"heartbeat_age={age}s, stale={stale}"
        )

    # Log ready issues vs shepherd slots
    ready_count = ctx.snapshot.get("computed", {}).get("total_ready", 0)
    available_slots = ctx.get_available_shepherd_slots()
    log_info(
        f"STALL-L1: ready_issues={ready_count}, "
        f"available_slots={available_slots}"
    )


def _escalate_level_2(ctx: DaemonContext) -> None:
    """Level 2 (recovery): Force reclaim stale shepherds and run orphan recovery."""
    log_warning(
        f"STALL-L2: Force recovery triggered "
        f"(stalled {ctx.consecutive_stalled} consecutive iterations)"
    )

    # Force reclaim stale shepherds (kill dead tmux, reset state, revert labels)
    reclaimed = force_reclaim_stale_shepherds(ctx)
    if reclaimed > 0:
        log_success(f"STALL-L2: Reclaimed {reclaimed} stale shepherd(s)")

    # Run orphan recovery unconditionally (bypass the orphaned_count > 0 gate)
    log_info("STALL-L2: Running unconditional orphan recovery...")
    try:
        recovery_result = run_orphan_recovery(
            ctx.repo_root, recover=True, verbose=ctx.config.debug_mode
        )
        if recovery_result.total_recovered > 0:
            log_success(
                f"STALL-L2: Orphan recovery recovered {recovery_result.total_recovered} item(s)"
            )
    except Exception as e:
        log_warning(f"STALL-L2: Orphan recovery failed: {e}")


def _escalate_level_3(ctx: DaemonContext) -> None:
    """Level 3 (pool restart): Kill all shepherd sessions and reset state."""
    log_warning(
        f"STALL-L3: Agent pool restart triggered "
        f"(stalled {ctx.consecutive_stalled} consecutive iterations)"
    )

    if ctx.state is None:
        return

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    killed = 0

    for name, entry in list(ctx.state.shepherds.items()):
        if entry.status != "working":
            continue

        # Kill tmux session if it exists
        if session_exists(name):
            log_info(f"STALL-L3: Killing tmux session for {name}")
            kill_stuck_session(name)
            killed += 1

        # Revert issue label
        issue = entry.issue
        if issue is not None:
            try:
                _unclaim_issue(issue)
                log_info(f"STALL-L3: Reverted issue #{issue} labels")
            except Exception as e:
                log_warning(f"STALL-L3: Failed to revert labels for #{issue}: {e}")

        # Reset entry to idle
        entry.status = "idle"
        entry.issue = None
        entry.task_id = None
        entry.output_file = None
        entry.idle_since = timestamp
        entry.idle_reason = "pool_restart"
        entry.last_issue = issue
        entry.last_completed = timestamp

    # Clear stale progress files
    progress_dir = ctx.repo_root / ".loom" / "progress"
    if progress_dir.is_dir():
        cleared = 0
        for pfile in progress_dir.glob("shepherd-*.json"):
            try:
                pfile.unlink()
                cleared += 1
            except OSError:
                pass
        if cleared > 0:
            log_info(f"STALL-L3: Cleared {cleared} stale progress file(s)")

    # Reset systematic failure state — pool restart is a clean slate
    if ctx.state.systematic_failure.active:
        log_info(
            f"STALL-L3: Cleared systematic failure state "
            f"(pattern={ctx.state.systematic_failure.pattern})"
        )
        ctx.state.systematic_failure = SystematicFailure()
        ctx.state.recent_failures = []

    log_success(f"STALL-L3: Pool restart complete - killed {killed} session(s)")

    ctx.consecutive_stalled = 0
