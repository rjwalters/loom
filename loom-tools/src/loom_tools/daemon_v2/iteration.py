"""Single iteration logic for the daemon."""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.state import read_daemon_state, write_json_file
from loom_tools.common.time_utils import now_utc
from loom_tools.daemon_v2.actions.completions import check_completions, handle_completion
from loom_tools.daemon_v2.actions.proposals import promote_proposals
from loom_tools.daemon_v2.actions.shepherds import spawn_shepherds
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

    # 4. Get recommended actions
    actions = ctx.get_recommended_actions()
    if ctx.config.debug_mode:
        log_info(f"Recommended actions: {actions}")

    # Track results
    result = IterationResult(status="success", summary="")
    result.completions_handled = len(completions)

    # 5. Execute actions based on recommendations

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

    # 6. Save state
    _save_daemon_state(ctx)

    # 7. Build summary
    result.summary = _build_summary(ctx, result)

    return result


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

    return " ".join(parts)
