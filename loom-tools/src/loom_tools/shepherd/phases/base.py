"""Base classes for phase runners."""

from __future__ import annotations

import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol

from loom_tools.common.state import read_json_file

if TYPE_CHECKING:
    from loom_tools.shepherd.context import ShepherdContext

# How often (in seconds) to poll the progress file during agent-wait-bg.sh
_HEARTBEAT_POLL_INTERVAL = 5


class PhaseStatus(Enum):
    """Result status of phase execution."""

    SUCCESS = "success"
    SKIPPED = "skipped"
    FAILED = "failed"
    SHUTDOWN = "shutdown"
    STUCK = "stuck"


@dataclass
class PhaseResult:
    """Result of phase execution."""

    status: PhaseStatus
    message: str = ""
    phase_name: str = ""
    data: dict[str, Any] = field(default_factory=dict)

    @property
    def is_success(self) -> bool:
        return self.status in (PhaseStatus.SUCCESS, PhaseStatus.SKIPPED)

    @property
    def is_shutdown(self) -> bool:
        return self.status == PhaseStatus.SHUTDOWN


class PhaseRunner(Protocol):
    """Protocol for phase execution.

    Each phase runner must implement:
    - should_skip: Check if phase should be skipped
    - run: Execute the phase
    - validate: Validate phase contract after execution
    """

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Check if phase should be skipped.

        Returns:
            Tuple of (should_skip, reason)
        """
        ...

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Execute the phase.

        Returns:
            PhaseResult with status and message
        """
        ...

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate phase contract after execution.

        Returns:
            True if contract is satisfied
        """
        ...


def _read_heartbeats(
    progress_file: Path, *, phase: str | None = None
) -> list[dict[str, Any]]:
    """Read heartbeat milestones from a shepherd progress file.

    Args:
        progress_file: Path to the shepherd progress JSON file.
        phase: If provided, only return heartbeats that occurred after
            the most recent ``phase_entered`` milestone for this phase.
            This prevents stale heartbeats from earlier phases from
            being displayed.

    Returns a list of heartbeat milestone dicts, each with
    ``timestamp`` and ``data.action`` keys.
    """
    data = read_json_file(progress_file)
    if not isinstance(data, dict):
        return []

    milestones = data.get("milestones", [])

    # Find the index of the most recent phase_entered milestone for this phase.
    # Only heartbeats between that point and the next phase_entered belong to
    # the current phase, preventing stale heartbeats from earlier phases from
    # being displayed during later phases.
    start_index = 0
    if phase:
        for i, m in enumerate(milestones):
            if (
                m.get("event") == "phase_entered"
                and m.get("data", {}).get("phase") == phase
            ):
                start_index = i + 1

    # Find the end boundary: the next phase_entered after start_index
    # (for any phase). During live polling, this boundary won't exist yet
    # so end_index == len(milestones), which is the common case.
    end_index = len(milestones)
    if phase and start_index > 0:
        for i in range(start_index, len(milestones)):
            if milestones[i].get("event") == "phase_entered":
                end_index = i
                break

    return [
        m
        for m in milestones[start_index:end_index]
        if m.get("event") == "heartbeat"
    ]


def _print_heartbeat(action: str) -> None:
    """Print a heartbeat status line to stderr.

    Uses dim/gray ANSI to differentiate from cyan phase headers.
    Format: ``[HH:MM:SS] ⟳ action``
    """
    ts = time.strftime("%H:%M:%S")
    # \033[2m = dim, \033[0m = reset
    print(f"\033[2m[{ts}] \u27f3 {action}\033[0m", file=sys.stderr)


def run_worker_phase(
    ctx: ShepherdContext,
    *,
    role: str,
    name: str,
    timeout: int,
    phase: str | None = None,
    worktree: Path | None = None,
    pr_number: int | None = None,
    args: str | None = None,
) -> int:
    """Run a phase worker and wait for completion.

    This wraps the agent-spawn.sh → agent-wait-bg.sh → agent-destroy.sh flow.
    While waiting, polls the shepherd progress file for heartbeat milestones
    and prints them to stderr so the operator can see ongoing activity.

    Args:
        ctx: Shepherd context
        role: Worker role (e.g., "builder", "judge")
        name: Session name (e.g., "builder-issue-42")
        timeout: Timeout in seconds
        phase: Phase name for activity detection
        worktree: Optional worktree path
        pr_number: Optional PR number
        args: Optional arguments for the worker

    Returns:
        Exit code from agent-wait-bg.sh:
        - 0: Success
        - 3: Shutdown signal
        - 4: Agent stuck after retry
        - Other: Error
    """
    scripts_dir = ctx.scripts_dir

    # Build spawn command
    spawn_cmd = [
        str(scripts_dir / "agent-spawn.sh"),
        "--role",
        role,
        "--name",
        name,
        "--on-demand",
    ]

    if args:
        spawn_cmd.extend(["--args", args])

    if worktree:
        spawn_cmd.extend(["--worktree", str(worktree)])

    # Spawn the worker
    # Redirect to DEVNULL to suppress output - agent logs are captured to
    # .loom/logs/<session>.log for debugging purposes
    spawn_result = subprocess.run(
        spawn_cmd,
        cwd=ctx.repo_root,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )

    if spawn_result.returncode != 0:
        return 1

    # Build wait command
    wait_cmd = [
        str(scripts_dir / "agent-wait-bg.sh"),
        name,
        "--timeout",
        str(timeout),
        "--poll-interval",
        str(ctx.config.poll_interval),
        "--issue",
        str(ctx.config.issue),
    ]

    if phase:
        wait_cmd.extend(["--phase", phase])
        # Work-producing roles need longer idle thresholds
        if phase in ("builder", "doctor"):
            wait_cmd.extend(["--min-idle-elapsed", "120"])

    if worktree:
        wait_cmd.extend(["--worktree", str(worktree)])

    if pr_number:
        wait_cmd.extend(["--pr", str(pr_number)])

    wait_cmd.extend(["--task-id", ctx.config.task_id])

    # Set LOOM_STUCK_ACTION for retry behavior
    env = os.environ.copy()
    env["LOOM_STUCK_ACTION"] = "retry"

    # Launch wait process (non-blocking) so we can poll for heartbeats
    wait_proc = subprocess.Popen(
        wait_cmd,
        cwd=ctx.repo_root,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )

    # Poll progress file for heartbeat updates while waiting
    progress_file = ctx.progress_dir / f"shepherd-{ctx.config.task_id}.json"
    seen_heartbeats = 0

    while wait_proc.poll() is None:
        heartbeats = _read_heartbeats(progress_file, phase=phase)
        for hb in heartbeats[seen_heartbeats:]:
            action = hb.get("data", {}).get("action", "")
            if action:
                _print_heartbeat(action)
        seen_heartbeats = len(heartbeats)
        time.sleep(_HEARTBEAT_POLL_INTERVAL)

    # Check for any final heartbeats written before process exit
    heartbeats = _read_heartbeats(progress_file, phase=phase)
    for hb in heartbeats[seen_heartbeats:]:
        action = hb.get("data", {}).get("action", "")
        if action:
            _print_heartbeat(action)

    wait_exit = wait_proc.returncode

    # Clean up the worker session
    destroy_cmd = [str(scripts_dir / "agent-destroy.sh"), name, "--force"]
    subprocess.run(
        destroy_cmd,
        cwd=ctx.repo_root,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )

    return wait_exit


def run_phase_with_retry(
    ctx: ShepherdContext,
    *,
    role: str,
    name: str,
    timeout: int,
    max_retries: int,
    phase: str | None = None,
    worktree: Path | None = None,
    pr_number: int | None = None,
    args: str | None = None,
) -> int:
    """Run a phase with automatic retry on stuck detection.

    On exit code 4 (stuck), retries up to max_retries times.

    Returns:
        Exit code: 0=success, 3=shutdown, 4=stuck after retries, other=error
    """
    stuck_retries = 0

    while True:
        exit_code = run_worker_phase(
            ctx,
            role=role,
            name=name,
            timeout=timeout,
            phase=phase,
            worktree=worktree,
            pr_number=pr_number,
            args=args,
        )

        if exit_code != 4:
            # Not stuck - return as-is
            return exit_code

        stuck_retries += 1
        if stuck_retries > max_retries:
            return 4  # Still stuck after max retries

        # Report retry milestone
        ctx.report_milestone(
            "heartbeat",
            action=f"retrying stuck {role} (attempt {stuck_retries})",
        )
