"""Spawn support roles (interval and demand-based)."""

from __future__ import annotations

import time
from typing import TYPE_CHECKING

from loom_tools.agent_spawn import (
    is_session_active,
    kill_stuck_session,
    session_exists,
    spawn_agent,
)
from loom_tools.common.logging import log_info, log_success, log_warning
from loom_tools.common.time_utils import now_utc
from loom_tools.daemon_v2.actions.completions import CompletionEntry
from loom_tools.models.daemon_state import SupportRoleEntry

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext

# Delay between consecutive support role spawns (seconds).
# Prevents auth cache lock contention when multiple roles start simultaneously.
# See issue #3109.
SPAWN_STAGGER_DELAY = 3


# Support roles and their corresponding slash commands
SUPPORT_ROLES = {
    "guide": "guide",
    "champion": "champion",
    "doctor": "doctor",
    "auditor": "auditor",
    "judge": "judge",
    "architect": "architect",
    "hermit": "hermit",
    "curator": "curator",
}


def spawn_support_role(
    ctx: DaemonContext,
    role: str,
    *,
    demand: bool = False,
    target_pr: int | None = None,
) -> bool:
    """Spawn a support role.

    Args:
        ctx: Daemon context
        role: Role name (guide, champion, doctor, etc.)
        demand: True if this is a demand-based spawn (vs interval)
        target_pr: Optional PR number to target (e.g., ``/doctor 123``)

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
    if target_pr:
        spawn_reason = f"targeted(PR #{target_pr})"
    log_info(f"Spawning support role {role} ({spawn_reason})")

    # Kill existing session if it exists (may be stuck)
    if session_exists(role):
        log_info(f"Killing existing session for {role}")
        kill_stuck_session(role)

    # Build args
    args = ""
    if target_pr:
        args = str(target_pr)
    elif ctx.config.force_mode and role in ("champion",):
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


def _get_first_targeted_pr(ctx: DaemonContext, key: str) -> int | None:
    """Get the first targeted PR number from the snapshot's computed section."""
    if ctx.snapshot is None:
        return None
    prs = ctx.snapshot.get("computed", {}).get(key, [])
    return prs[0] if prs else None


def spawn_roles_from_actions(ctx: DaemonContext) -> int:
    """Spawn support roles based on recommended actions from snapshot.

    Staggers spawns by SPAWN_STAGGER_DELAY seconds between consecutive
    successful spawns to prevent auth cache lock contention.  See issue #3109.

    Returns the number of roles spawned.
    """
    actions = ctx.get_recommended_actions()
    spawned = 0

    def _spawn_with_stagger(role: str, **kwargs: object) -> bool:
        """Spawn a role and stagger if this is not the first spawn."""
        nonlocal spawned
        if spawned > 0:
            log_info(
                f"Staggering {role} spawn by {SPAWN_STAGGER_DELAY}s "
                f"to avoid auth cache contention"
            )
            time.sleep(SPAWN_STAGGER_DELAY)
        if spawn_support_role(ctx, role, **kwargs):  # type: ignore[arg-type]
            spawned += 1
            return True
        return False

    # Demand-based triggers (higher priority)
    # Targeted dispatch takes precedence: if orphaned PRs exist, the
    # snapshot generates ``spawn_*_targeted`` instead of ``spawn_*_demand``.
    if "spawn_champion_demand" in actions:
        _spawn_with_stagger("champion", demand=True)

    if "spawn_doctor_targeted" in actions:
        pr = _get_first_targeted_pr(ctx, "doctor_targeted_prs")
        _spawn_with_stagger("doctor", demand=True, target_pr=pr)
    elif "spawn_doctor_demand" in actions:
        _spawn_with_stagger("doctor", demand=True)

    if "spawn_judge_targeted" in actions:
        pr = _get_first_targeted_pr(ctx, "judge_targeted_prs")
        _spawn_with_stagger("judge", demand=True, target_pr=pr)
    elif "spawn_judge_demand" in actions:
        _spawn_with_stagger("judge", demand=True)

    # Interval-based triggers
    if "trigger_guide" in actions:
        _spawn_with_stagger("guide")

    if "trigger_champion" in actions and "spawn_champion_demand" not in actions:
        _spawn_with_stagger("champion")

    if "trigger_doctor" in actions and "spawn_doctor_demand" not in actions and "spawn_doctor_targeted" not in actions:
        _spawn_with_stagger("doctor")

    if "trigger_auditor" in actions:
        _spawn_with_stagger("auditor")

    if "trigger_judge" in actions and "spawn_judge_demand" not in actions and "spawn_judge_targeted" not in actions:
        _spawn_with_stagger("judge")

    # Work generation roles
    if "trigger_architect" in actions:
        _spawn_with_stagger("architect")

    if "trigger_hermit" in actions:
        _spawn_with_stagger("hermit")

    if "trigger_curator" in actions:
        _spawn_with_stagger("curator")

    return spawned


def reclaim_completed_support_roles(ctx: DaemonContext) -> list[CompletionEntry]:
    """Detect support roles whose Claude process has finished and mark them idle.

    Iterates over all support roles with status ``"running"`` and checks
    whether the Claude process inside their tmux session is still active.
    A role is considered complete when:

    - The tmux session no longer exists at all, OR
    - The tmux session exists but the Claude process has exited (the shell
      is sitting at an idle prompt).

    In the second case the lingering tmux session is killed so it does not
    block future spawns.

    Returns a list of ``CompletionEntry`` objects for completed roles.
    """
    if ctx.state is None:
        return []

    completed: list[CompletionEntry] = []

    for role_name, entry in ctx.state.support_roles.items():
        if entry.status != "running":
            continue

        # Check if the tmux session has a live Claude process
        if is_session_active(role_name):
            continue

        # Session is either gone or Claude has exited.
        # If the tmux session still exists (shell-only), kill it.
        if session_exists(role_name):
            log_info(
                f"Support role {role_name} Claude process exited "
                f"(tmux session still open) — killing stale session"
            )
            kill_stuck_session(role_name)
        else:
            log_info(
                f"Support role {role_name} tmux session exited — marking as completed"
            )

        completed.append(
            CompletionEntry(
                type="support_role",
                name=role_name,
            )
        )

    return completed
