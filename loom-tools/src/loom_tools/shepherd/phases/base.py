"""Base classes for phase runners."""

from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol

if TYPE_CHECKING:
    from loom_tools.shepherd.context import ShepherdContext


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
    spawn_result = subprocess.run(
        spawn_cmd,
        cwd=ctx.repo_root,
        text=True,
        capture_output=True,
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

    # Wait for completion
    wait_result = subprocess.run(
        wait_cmd,
        cwd=ctx.repo_root,
        text=True,
        capture_output=True,
        check=False,
        env=env,
    )

    wait_exit = wait_result.returncode

    # Clean up the worker session
    destroy_cmd = [str(scripts_dir / "agent-destroy.sh"), name, "--force"]
    subprocess.run(
        destroy_cmd,
        cwd=ctx.repo_root,
        text=True,
        capture_output=True,
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
