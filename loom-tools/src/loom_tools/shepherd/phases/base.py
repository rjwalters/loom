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

from loom_tools.claim import extend_claim
from loom_tools.common.logging import log_warning, strip_ansi
from loom_tools.common.paths import LoomPaths
from loom_tools.common.state import read_json_file

if TYPE_CHECKING:
    from loom_tools.shepherd.context import ShepherdContext

# How often (in seconds) to poll the progress file during agent-wait-bg.sh
_HEARTBEAT_POLL_INTERVAL = 5

# Minimum characters of non-header content required for "meaningful output".
# Sessions with less than this are treated as transient spawn failures
# (e.g., Claude API error on startup).  See issues #2135, #2381, #2401.
INSTANT_EXIT_MIN_OUTPUT_CHARS = 100

# Sentinel line written by claude-wrapper.sh just before invoking the Claude
# CLI.  Output before this marker is wrapper pre-flight boilerplate and should
# be excluded when measuring meaningful output.  See issue #2401.
_CLI_START_SENTINEL = "# CLAUDE_CLI_START"

# Maximum retries for instant-exit detection, with exponential backoff.
INSTANT_EXIT_MAX_RETRIES = 3
INSTANT_EXIT_BACKOFF_SECONDS = [2, 4, 8]

# How often (in seconds) to extend the file-based claim during worker polling.
# The claim TTL is 2 hours; extending every 30 minutes provides ample margin.
_CLAIM_EXTEND_INTERVAL = 1800

# MCP failure detection patterns (case-insensitive).
# These patterns appear in Claude CLI output when the MCP server fails
# to initialize, causing an immediate exit with no useful work done.
MCP_FAILURE_PATTERNS = [
    "MCP server failed",
    "MCP.*failed",
    "mcp server failed",
]

# Minimum characters of non-header output for a session to be considered
# "productive" when checking MCP failure patterns.  Sessions with more output
# than this are assumed to have done real work — the "MCP server failed" text
# is just Claude CLI status-bar noise, not a real failure.
# See issues #2374 and #2381.
MCP_FAILURE_MIN_OUTPUT_CHARS = 500

# Maximum retries for MCP failure detection, with longer backoff.
# MCP failures are often systemic (stale build, resource contention)
# so we use longer backoff than instant-exit.
MCP_FAILURE_MAX_RETRIES = 3
MCP_FAILURE_BACKOFF_SECONDS = [5, 15, 30]


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


class BasePhase:
    """Base class for phase runners with helper methods for creating PhaseResults.

    Subclasses should set the ``phase_name`` class attribute to the name of
    the phase (e.g., "builder", "judge"). This name is automatically used
    in all PhaseResult objects created via the helper methods.

    Example usage::

        class MyPhase(BasePhase):
            phase_name = "my_phase"

            def run(self, ctx: ShepherdContext) -> PhaseResult:
                if some_error:
                    return self.failed("something went wrong", {"detail": "info"})
                return self.success("phase completed")
    """

    phase_name: str = ""

    def result(
        self,
        status: PhaseStatus,
        message: str = "",
        data: dict[str, Any] | None = None,
    ) -> PhaseResult:
        """Create a PhaseResult with this phase's name.

        Args:
            status: The status of the phase result.
            message: A human-readable message describing the result.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with the phase_name set automatically.
        """
        return PhaseResult(
            status=status,
            message=message,
            phase_name=self.phase_name,
            data=data or {},
        )

    def success(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a successful PhaseResult.

        Args:
            message: A human-readable message describing the success.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with SUCCESS status.
        """
        return self.result(PhaseStatus.SUCCESS, message, data)

    def failed(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a failed PhaseResult.

        Args:
            message: A human-readable message describing the failure.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with FAILED status.
        """
        return self.result(PhaseStatus.FAILED, message, data)

    def skipped(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a skipped PhaseResult.

        Args:
            message: A human-readable message describing why skipped.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with SKIPPED status.
        """
        return self.result(PhaseStatus.SKIPPED, message, data)

    def shutdown(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a shutdown PhaseResult.

        Args:
            message: A human-readable message describing the shutdown.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with SHUTDOWN status.
        """
        return self.result(PhaseStatus.SHUTDOWN, message, data)

    def stuck(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a stuck PhaseResult.

        Args:
            message: A human-readable message describing the stuck state.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with STUCK status.
        """
        return self.result(PhaseStatus.STUCK, message, data)


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


def _get_cli_output(stripped: str) -> str:
    """Extract non-header output produced after the CLI start sentinel.

    If the ``# CLAUDE_CLI_START`` sentinel is present, only lines after the
    **last** occurrence are considered (the wrapper may emit multiple sentinels
    when retrying).  Lines starting with ``# `` are always excluded as log
    headers.

    If no sentinel is found the session is considered an instant exit
    (the wrapper always writes the sentinel before invoking Claude, so
    its absence means Claude never started).  See issue #2405.

    Args:
        stripped: ANSI-stripped log file content.

    Returns:
        The meaningful (non-header, post-sentinel) output as a single string.
    """
    lines = stripped.splitlines()

    # Find the last sentinel index.
    sentinel_idx: int | None = None
    for i, line in enumerate(lines):
        if line.strip() == _CLI_START_SENTINEL:
            sentinel_idx = i

    start = (sentinel_idx + 1) if sentinel_idx is not None else 0
    return "\n".join(line for line in lines[start:] if not line.startswith("# "))


def _is_mcp_failure(log_path: Path) -> bool:
    """Check if a session log indicates an MCP server initialization failure.

    Detects cases where the Claude CLI exits immediately because the MCP
    server (mcp-loom) failed to initialize.  This is a distinct failure mode
    from generic instant-exits (API errors, network issues) because it
    typically has a systemic cause (stale build, resource contention) that
    benefits from different retry/backoff strategy.

    To avoid false positives on productive sessions (where the Claude CLI
    status bar may show "1 MCP server failed" as informational text), the
    function checks output volume.  Sessions that produced substantial
    non-header output are assumed productive — the MCP text is status-bar
    noise, not a real failure.  See issues #2374 and #2381.

    Note: A previous implementation used ``st_mtime - st_ctime`` as a
    duration gate, but this is always ~0 for actively-written log files
    because writing updates both timestamps simultaneously.

    Args:
        log_path: Path to the worker session log file.

    Returns:
        True if the log contains MCP failure indicators **and** the session
        produced minimal output (below the output volume threshold).
    """
    if not log_path.is_file():
        return False

    try:
        import re

        content = log_path.read_text()
        stripped = strip_ansi(content)

        # If the session produced substantial CLI output beyond headers
        # and wrapper pre-flight, it was productive — MCP text is just
        # status bar noise.  See issues #2374, #2381, #2401.
        cli_output = _get_cli_output(stripped)
        if len(cli_output.strip()) >= MCP_FAILURE_MIN_OUTPUT_CHARS:
            return False

        for pattern in MCP_FAILURE_PATTERNS:
            if re.search(pattern, cli_output, re.IGNORECASE):
                return True
    except OSError:
        pass
    return False


def _is_instant_exit(log_path: Path) -> bool:
    """Check if a session log indicates an instant-exit (transient spawn failure).

    A session is considered an instant exit when the log file exists but has
    no meaningful output (< INSTANT_EXIT_MIN_OUTPUT_CHARS non-header chars).

    This detects cases where the Claude CLI spawns but immediately exits due to
    transient API errors, without producing any substantive work.

    Note: A previous implementation also checked ``st_mtime - st_ctime`` as
    a duration gate, but this is always ~0 for actively-written log files
    because writing updates both timestamps simultaneously.  The output-size
    check alone is sufficient and reliable.  See issue #2381.

    Args:
        log_path: Path to the worker session log file.

    Returns:
        True if the session appears to be an instant exit.
    """
    if not log_path.is_file():
        # No log file at all — could be spawn failure, not instant exit.
        return False

    try:
        content = log_path.read_text()
        stripped = strip_ansi(content)

        # If the sentinel is absent, Claude never started — treat as instant
        # exit regardless of how much wrapper pre-flight output exists.
        # See issue #2405.
        if _CLI_START_SENTINEL not in stripped:
            return True

        # Exclude log header lines and wrapper pre-flight output (everything
        # before the last ``# CLAUDE_CLI_START`` sentinel) so that only
        # actual Claude CLI output counts.  See issues #2135, #2381, #2401.
        cli_output = _get_cli_output(stripped)
        return len(cli_output.strip()) < INSTANT_EXIT_MIN_OUTPUT_CHARS
    except OSError:
        return False


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
        Exit code from agent-wait-bg.sh (or synthetic):
        - 0: Success
        - 3: Shutdown signal
        - 4: Agent stuck after retry
        - 5: Failures are pre-existing (Doctor only)
        - 6: Instant exit detected (session < 5s with no meaningful output)
        - 7: MCP server failure detected (session exited due to MCP init failure)
        - Other: Error
    """
    scripts_dir = ctx.scripts_dir

    # Guard against missing scripts directory.  This can happen when the
    # working tree is on a branch that predates the Loom installation (the
    # branch was created before .loom/scripts/ was added to the repo).
    # See issue #2147.
    spawn_script = scripts_dir / "agent-spawn.sh"
    if not spawn_script.is_file():
        log_warning(
            f"Script not found: {spawn_script} — "
            "the branch may predate Loom installation"
        )
        return 1

    # Build spawn command
    spawn_cmd = [
        str(spawn_script),
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
    wait_script = scripts_dir / "agent-wait-bg.sh"
    if not wait_script.is_file():
        log_warning(
            f"Script not found: {wait_script} — "
            "the branch may predate Loom installation"
        )
        return 1

    wait_cmd = [
        str(wait_script),
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
        if phase in ("builder", "doctor", "judge"):
            wait_cmd.extend(["--min-idle-elapsed", "120"])

    if worktree:
        wait_cmd.extend(["--worktree", str(worktree)])

    if pr_number:
        wait_cmd.extend(["--pr", str(pr_number)])

    wait_cmd.extend(["--task-id", ctx.config.task_id])

    # Set LOOM_STUCK_ACTION for retry behavior
    env = os.environ.copy()
    env.pop("CLAUDECODE", None)  # Prevent nested session guard from blocking subprocess
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
    last_claim_extend = time.monotonic()
    agent_id = f"shepherd-{ctx.config.task_id}"

    while wait_proc.poll() is None:
        heartbeats = _read_heartbeats(progress_file, phase=phase)
        for hb in heartbeats[seen_heartbeats:]:
            action = hb.get("data", {}).get("action", "")
            if action:
                _print_heartbeat(action)
        seen_heartbeats = len(heartbeats)

        # Extend file-based claim periodically to prevent TTL expiry
        # during long worker phases.  See issue #2405.
        elapsed = time.monotonic() - last_claim_extend
        if elapsed >= _CLAIM_EXTEND_INTERVAL:
            extend_claim(ctx.repo_root, ctx.config.issue, agent_id)
            last_claim_extend = time.monotonic()

        time.sleep(_HEARTBEAT_POLL_INTERVAL)

    # Check for any final heartbeats written before process exit
    heartbeats = _read_heartbeats(progress_file, phase=phase)
    for hb in heartbeats[seen_heartbeats:]:
        action = hb.get("data", {}).get("action", "")
        if action:
            _print_heartbeat(action)

    wait_exit = wait_proc.returncode

    # Clean up the worker session
    destroy_script = scripts_dir / "agent-destroy.sh"
    if destroy_script.is_file():
        destroy_cmd = [str(destroy_script), name, "--force"]
        subprocess.run(
            destroy_cmd,
            cwd=ctx.repo_root,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )

    # Detect instant-exit / MCP failure: session produced no meaningful output.
    # Return synthetic exit code 6 (instant-exit) or 7 (MCP failure) so the
    # retry layer can handle it with backoff.  See issues #2135, #2279.
    #
    # Check on ALL exit codes, not just 0.  A degraded CLI session may exit
    # with a non-zero code (e.g., 2 for API error) while still producing no
    # meaningful output — this is functionally the same as an instant-exit
    # and should be retried rather than treated as a builder error.
    # See issue #2446.
    #
    # Check for MCP failure first (exit code 7) since it's a more specific
    # failure mode with different retry/backoff strategy.
    paths = LoomPaths(ctx.repo_root)
    log_path = paths.worker_log_file(role, ctx.config.issue)
    if _is_mcp_failure(log_path):
        log_warning(
            f"MCP server failure detected for {role} session '{name}': "
            f"MCP server failed to initialize (exit code {wait_exit}, log: {log_path})"
        )
        return 7
    if _is_instant_exit(log_path):
        log_warning(
            f"Instant-exit detected for {role} session '{name}': "
            f"session produced no meaningful output (exit code {wait_exit}, log: {log_path})"
        )
        return 6

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
    """Run a phase with automatic retry on stuck, instant-exit, or MCP failure.

    On exit code 4 (stuck), retries up to max_retries times.
    On exit code 6 (instant exit), retries up to INSTANT_EXIT_MAX_RETRIES
    times with exponential backoff.
    On exit code 7 (MCP failure), retries up to MCP_FAILURE_MAX_RETRIES
    times with longer backoff (MCP failures are often systemic).

    Returns:
        Exit code: 0=success, 3=shutdown, 4=stuck after retries,
                   6=instant-exit after retries, 7=MCP failure after retries,
                   other=error
    """
    stuck_retries = 0
    instant_exit_retries = 0
    mcp_failure_retries = 0

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

        # --- Pre-retry approval check (judge phase only) ---
        # If the judge already completed its work (applied loom:pr or
        # loom:changes-requested) before the MCP/instant-exit failure
        # occurred, skip the retry entirely.  See issue #2335.
        if exit_code in (6, 7) and phase == "judge" and ctx.pr_number is not None:
            ctx.label_cache.invalidate_pr(ctx.pr_number)
            if ctx.has_pr_label("loom:pr") or ctx.has_pr_label(
                "loom:changes-requested"
            ):
                log_warning(
                    f"Judge already completed (PR #{ctx.pr_number} has outcome label), "
                    f"skipping retry despite exit code {exit_code}"
                )
                return 0

        # --- MCP failure handling (exit code 7) ---
        # MCP failures are systemic (stale build, resource contention) so
        # use longer backoff than generic instant-exits.  See issue #2279.
        if exit_code == 7:
            mcp_failure_retries += 1
            if mcp_failure_retries > MCP_FAILURE_MAX_RETRIES:
                log_warning(
                    f"MCP server failure persisted for {role} after "
                    f"{MCP_FAILURE_MAX_RETRIES} retries"
                )
                return 7  # Caller should treat as failure

            backoff_idx = min(
                mcp_failure_retries - 1, len(MCP_FAILURE_BACKOFF_SECONDS) - 1
            )
            backoff = MCP_FAILURE_BACKOFF_SECONDS[backoff_idx]

            ctx.report_milestone(
                "error",
                error=f"MCP server failure detected for {role}",
                will_retry=True,
            )
            ctx.report_milestone(
                "heartbeat",
                action=(
                    f"retrying MCP failure {role} "
                    f"(attempt {mcp_failure_retries}/{MCP_FAILURE_MAX_RETRIES}, "
                    f"backoff {backoff}s)"
                ),
            )

            time.sleep(backoff)
            continue

        # --- Instant-exit handling (exit code 6) ---
        if exit_code == 6:
            instant_exit_retries += 1
            if instant_exit_retries > INSTANT_EXIT_MAX_RETRIES:
                log_warning(
                    f"Instant-exit persisted for {role} after "
                    f"{INSTANT_EXIT_MAX_RETRIES} retries"
                )
                return 6  # Caller should treat as failure

            # Exponential backoff before retry
            backoff_idx = min(
                instant_exit_retries - 1, len(INSTANT_EXIT_BACKOFF_SECONDS) - 1
            )
            backoff = INSTANT_EXIT_BACKOFF_SECONDS[backoff_idx]

            ctx.report_milestone(
                "error",
                error=f"instant-exit detected for {role}",
                will_retry=True,
            )
            ctx.report_milestone(
                "heartbeat",
                action=(
                    f"retrying instant-exit {role} "
                    f"(attempt {instant_exit_retries}/{INSTANT_EXIT_MAX_RETRIES}, "
                    f"backoff {backoff}s)"
                ),
            )

            time.sleep(backoff)
            continue

        # --- Stuck handling (exit code 4) ---
        if exit_code != 4:
            return exit_code

        stuck_retries += 1
        if stuck_retries > max_retries:
            return 4  # Still stuck after max retries

        # Report retry milestone
        ctx.report_milestone(
            "heartbeat",
            action=f"retrying stuck {role} (attempt {stuck_retries})",
        )
