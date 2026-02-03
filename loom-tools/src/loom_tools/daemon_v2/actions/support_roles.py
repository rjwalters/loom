"""Spawn support roles (interval and demand-based)."""

from __future__ import annotations

from typing import TYPE_CHECKING

from loom_tools.agent_spawn import spawn_agent, session_exists, kill_stuck_session
from loom_tools.common.logging import log_info, log_success, log_warning
from loom_tools.common.time_utils import now_utc
from loom_tools.models.daemon_state import SupportRoleEntry

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext


# Support roles and their corresponding slash commands
SUPPORT_ROLES = {
    "guide": "guide",
    "champion": "champion",
    "doctor": "doctor",
    "auditor": "auditor",
    "judge": "judge",
    "architect": "architect",
    "hermit": "hermit",
}


def spawn_support_role(
    ctx: DaemonContext,
    role: str,
    *,
    demand: bool = False,
) -> bool:
    """Spawn a support role.

    Args:
        ctx: Daemon context
        role: Role name (guide, champion, doctor, etc.)
        demand: True if this is a demand-based spawn (vs interval)

    Returns True if spawned successfully.
    """
    if ctx.state is None:
        return False

    if role not in SUPPORT_ROLES:
        log_warning(f"Unknown support role: {role}")
        return False

    # Check if already running
    entry = ctx.state.support_roles.get(role)
    if entry and entry.status == "running":
        log_info(f"Support role {role} already running")
        return False

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    spawn_reason = "demand" if demand else "interval"
    log_info(f"Spawning support role {role} ({spawn_reason})")

    # Kill existing session if it exists (may be stuck)
    if session_exists(role):
        log_info(f"Killing existing session for {role}")
        kill_stuck_session(role)

    # Build args
    args = ""
    if ctx.config.force_mode and role in ("champion",):
        args = "--force"

    # Spawn the role
    result = spawn_agent(
        role=role,
        name=role,
        args=args,
        worktree="",
        repo_root=ctx.repo_root,
    )

    if result.status == "error":
        log_warning(f"Failed to spawn {role}: {result.error}")
        return False

    # Update state
    if entry is None:
        entry = SupportRoleEntry()
        ctx.state.support_roles[role] = entry

    entry.status = "running"
    entry.started = timestamp
    entry.tmux_session = result.session

    log_success(f"Spawned support role {role}")
    return True


def spawn_roles_from_actions(ctx: DaemonContext) -> int:
    """Spawn support roles based on recommended actions from snapshot.

    Returns the number of roles spawned.
    """
    actions = ctx.get_recommended_actions()
    spawned = 0

    # Demand-based triggers (higher priority)
    if "spawn_champion_demand" in actions:
        if spawn_support_role(ctx, "champion", demand=True):
            spawned += 1

    if "spawn_doctor_demand" in actions:
        if spawn_support_role(ctx, "doctor", demand=True):
            spawned += 1

    if "spawn_judge_demand" in actions:
        if spawn_support_role(ctx, "judge", demand=True):
            spawned += 1

    # Interval-based triggers
    if "trigger_guide" in actions:
        if spawn_support_role(ctx, "guide"):
            spawned += 1

    if "trigger_champion" in actions and "spawn_champion_demand" not in actions:
        if spawn_support_role(ctx, "champion"):
            spawned += 1

    if "trigger_doctor" in actions and "spawn_doctor_demand" not in actions:
        if spawn_support_role(ctx, "doctor"):
            spawned += 1

    if "trigger_auditor" in actions:
        if spawn_support_role(ctx, "auditor"):
            spawned += 1

    if "trigger_judge" in actions and "spawn_judge_demand" not in actions:
        if spawn_support_role(ctx, "judge"):
            spawned += 1

    # Work generation roles
    if "trigger_architect" in actions:
        if spawn_support_role(ctx, "architect"):
            spawned += 1

    if "trigger_hermit" in actions:
        if spawn_support_role(ctx, "hermit"):
            spawned += 1

    return spawned
