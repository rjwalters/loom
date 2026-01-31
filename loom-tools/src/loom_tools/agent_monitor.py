"""Agent monitoring with stuck detection and signal handling.

This module provides async monitoring of Claude Code agents in tmux sessions,
detecting completion, handling shutdown signals, and implementing stuck detection.

Replaces the shell script agent-wait-bg.sh with full feature parity.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import pathlib
import re
import subprocess
import time
from dataclasses import dataclass, field

from loom_tools.common.logging import log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.models.agent_wait import (
    CompletionReason,
    ContractCheckResult,
    MonitorConfig,
    SignalType,
    StuckAction,
    WaitResult,
    WaitStatus,
)

# tmux configuration (must match agent-spawn.sh)
TMUX_SOCKET = "loom"
SESSION_PREFIX = "loom-"

# Pattern for detecting Claude is processing (from agent-wait-bg.sh)
PROCESSING_INDICATORS = "esc to interrupt"

# Progress tracking directory
PROGRESS_DIR = pathlib.Path("/tmp/loom-agent-progress")

# Adaptive contract checking intervals (see issue #1678)
# Contract checks are expensive (GitHub API calls), so we start with longer
# intervals and decrease them over time as completion becomes more likely.
#
# Interval schedule based on elapsed time:
#   0-180s:   No contract checks (wait for initial processing)
#   180-270s: 90s interval (agent still early in work)
#   270-330s: 60s interval (agent progressing)
#   330-360s: 30s interval (likely nearing completion)
#   360s+:    10s interval (final detection mode)
CONTRACT_INITIAL_DELAY = 180


def get_adaptive_contract_interval(elapsed: int, override: int = 0) -> int:
    """Get adaptive contract check interval based on elapsed time since agent started.

    Returns the appropriate interval in seconds, or 0 if we should skip this check.

    The schedule balances detection latency against API cost:
      0-180s:   Skip checks (return 0) - agent still processing initial work
      180-270s: 90s interval - early work phase
      270-330s: 60s interval - mid work phase
      330-360s: 30s interval - likely nearing completion
      360s+:    10s interval - final rapid detection mode

    If override is set > 0, returns that fixed value instead.
    Returns 0 to signal "skip this check" (used during initial delay period).
    """
    # Allow override for testing or specific use cases
    if override > 0:
        return override

    # Adaptive schedule based on elapsed time
    if elapsed < 180:
        return 0  # No check yet - wait for initial delay
    elif elapsed < 270:
        return 90
    elif elapsed < 330:
        return 60
    elif elapsed < 360:
        return 30
    else:
        return 10


@dataclass
class ProgressTracker:
    """Tracks agent progress via tmux pane content hashing."""

    name: str
    last_hash: str = ""
    last_progress_time: float = field(default_factory=time.time)

    def check_progress(self, session_name: str) -> bool:
        """Check if pane content has changed. Returns True if progress detected."""
        content = capture_pane(session_name)
        if not content:
            return False

        current_hash = hashlib.md5(content.encode()).hexdigest()
        if current_hash != self.last_hash:
            self.last_hash = current_hash
            self.last_progress_time = time.time()
            return True
        return False

    def get_idle_time(self) -> int:
        """Get seconds since last progress."""
        return int(time.time() - self.last_progress_time)


def capture_pane(session_name: str) -> str:
    """Capture current tmux pane content."""
    try:
        result = subprocess.run(
            ["tmux", "-L", TMUX_SOCKET, "capture-pane", "-t", session_name, "-p"],
            capture_output=True,
            text=True,
            check=False,
        )
        return result.stdout if result.returncode == 0 else ""
    except Exception:
        return ""


def send_keys(session_name: str, keys: str) -> bool:
    """Send keys to tmux session."""
    try:
        subprocess.run(
            ["tmux", "-L", TMUX_SOCKET, "send-keys", "-t", session_name, keys],
            check=True,
            capture_output=True,
        )
        return True
    except Exception:
        return False


def kill_session(session_name: str) -> bool:
    """Kill a tmux session."""
    try:
        subprocess.run(
            ["tmux", "-L", TMUX_SOCKET, "kill-session", "-t", session_name],
            check=False,
            capture_output=True,
        )
        return True
    except Exception:
        return False


def session_exists(session_name: str) -> bool:
    """Check if tmux session exists."""
    try:
        result = subprocess.run(
            ["tmux", "-L", TMUX_SOCKET, "has-session", "-t", session_name],
            capture_output=True,
            check=False,
        )
        return result.returncode == 0
    except Exception:
        return False


class AgentMonitor:
    """Monitors a Claude Code agent in a tmux session.

    Provides completion detection, stuck detection, and signal handling
    for autonomous agent orchestration.
    """

    def __init__(self, config: MonitorConfig) -> None:
        self.config = config
        self.session_name = f"{SESSION_PREFIX}{config.name}"
        self.repo_root = find_repo_root()
        self.log_file = self.repo_root / ".loom" / "logs" / f"{self.session_name}.log"

        self.progress_tracker = ProgressTracker(name=config.name)
        self.start_time = time.time()

        # State tracking
        self._completion_detected = False
        self._completion_reason: CompletionReason | None = None
        self._idle_contract_checked = False
        self._last_contract_check = 0.0
        self._stuck_warned = False
        self._stuck_critical_reported = False
        self._last_prompt_stuck_check = 0.0
        self._prompt_stuck_since = 0.0  # When stuck state was first detected (0 = not stuck)
        self._prompt_stuck_recovery_attempted = False
        self._prompt_stuck_recovery_time = 0.0  # When last recovery was attempted
        self._prompt_resolved = False
        self._last_heartbeat_time = self.start_time

    @property
    def elapsed(self) -> int:
        """Seconds elapsed since monitoring started."""
        return int(time.time() - self.start_time)

    async def monitor(self) -> WaitResult:
        """Main monitoring loop. Returns when agent completes or a signal is detected."""
        log_info(
            f"Monitoring agent '{self.config.name}' "
            f"(poll: {self.config.poll_interval}s, timeout: {self.config.timeout}s)"
        )

        if self.config.task_id:
            log_info(
                f"Heartbeat emission: every {self.config.heartbeat_interval}s "
                f"(task-id: {self.config.task_id})"
            )

        if self.config.phase:
            # Check if using adaptive or fixed intervals
            override = (
                self.config.contract_interval
                if self.config.contract_interval != 90
                else 0
            )
            if override == 0:
                log_info(
                    f"Proactive contract checking: adaptive intervals for phase "
                    f"'{self.config.phase}' (initial delay: {CONTRACT_INITIAL_DELAY}s)"
                )
            elif override > 0:
                log_info(
                    f"Proactive contract checking: fixed {override}s interval "
                    f"for phase '{self.config.phase}'"
                )
            else:
                log_info(
                    f"Proactive contract checking: disabled for phase '{self.config.phase}'"
                )

        sc = self.config.stuck_config
        log_info(
            f"Stuck detection: warning={sc.warning_threshold}s, "
            f"critical={sc.critical_threshold}s, action={sc.action.value}"
        )
        log_info(
            f"Prompt stuck detection: check_interval={sc.prompt_stuck_check_interval}s, "
            f"age_threshold={sc.prompt_stuck_age_threshold}s, "
            f"recovery_cooldown={sc.prompt_stuck_recovery_cooldown}s"
        )

        while True:
            # Check timeout
            if self.elapsed >= self.config.timeout:
                return WaitResult(
                    status=WaitStatus.TIMEOUT,
                    name=self.config.name,
                    elapsed=self.elapsed,
                )

            # Check if session still exists
            if not session_exists(self.session_name):
                return WaitResult(
                    status=WaitStatus.SESSION_NOT_FOUND,
                    name=self.config.name,
                    elapsed=self.elapsed,
                )

            # Check for interactive prompts (plan mode approval)
            if not self._prompt_resolved:
                if self._check_and_resolve_prompts():
                    self._prompt_resolved = True

            # Check for shutdown signals
            signal = await self._check_signals()
            if signal:
                return WaitResult(
                    status=WaitStatus.SIGNAL,
                    name=self.config.name,
                    elapsed=self.elapsed,
                    signal_type=signal,
                )

            # Check shepherd progress file for errored status (fast error detection)
            if self.config.task_id:
                if self._check_errored_status():
                    log_warning(
                        f"Shepherd errored (progress file status), "
                        f"terminating session '{self.session_name}'"
                    )
                    kill_session(self.session_name)
                    return WaitResult(
                        status=WaitStatus.ERRORED,
                        name=self.config.name,
                        elapsed=self.elapsed,
                        error_message="progress_file_errored",
                    )

            # Fast "stuck at prompt" detection
            result = self._check_prompt_stuck()
            if result:
                return result

            # Proactive phase contract checking
            if not self._completion_detected:
                if self._check_proactive_contract():
                    self._completion_detected = True
                    self._completion_reason = CompletionReason.PHASE_CONTRACT_SATISFIED

            # Idle-triggered contract checking (backup)
            if not self._completion_detected:
                if self._check_idle_contract():
                    self._completion_detected = True
                    self._completion_reason = CompletionReason.PHASE_CONTRACT_SATISFIED

            # Reset idle check flag if there's been new activity
            if self._idle_contract_checked:
                idle_time = self._get_log_idle_time()
                if idle_time >= 0 and idle_time < self.config.idle_timeout:
                    self._idle_contract_checked = False

            # Check completion patterns in log (backup detection)
            if not self._completion_detected:
                reason = self._check_completion_patterns()
                if reason:
                    if reason == CompletionReason.EXPLICIT_EXIT:
                        self._completion_detected = True
                        self._completion_reason = reason
                    elif self.config.phase and self.config.issue:
                        # Verify phase contract before trusting pattern
                        log_info(
                            f"Completion pattern detected ({reason.value}) - "
                            "verifying phase contract"
                        )
                        await asyncio.sleep(3)
                        if self._verify_phase_contract():
                            self._completion_detected = True
                            self._completion_reason = reason
                            log_info("Phase contract verified - terminating session")
                        else:
                            log_warning(
                                "Completion pattern detected but phase contract "
                                "not yet satisfied - continuing to wait"
                            )
                    else:
                        self._completion_detected = True
                        self._completion_reason = reason

            # Handle completion
            if self._completion_detected:
                return await self._handle_completion()

            # Update progress tracking
            self.progress_tracker.check_progress(self.session_name)

            # Check stuck status
            stuck_result = self._check_stuck_status()
            if stuck_result:
                return stuck_result

            # Emit periodic heartbeat
            await self._emit_heartbeat()

            await asyncio.sleep(self.config.poll_interval)

    async def _check_signals(self) -> SignalType | None:
        """Check for shutdown signals. Returns signal type if detected."""
        # Check global shutdown signal
        stop_file = self.repo_root / ".loom" / "stop-shepherds"
        if stop_file.exists():
            log_warning("Shutdown signal detected (stop-shepherds)")
            return SignalType.SHUTDOWN

        # Check per-issue abort label
        if self.config.issue:
            try:
                result = subprocess.run(
                    [
                        "gh",
                        "issue",
                        "view",
                        str(self.config.issue),
                        "--json",
                        "labels",
                        "--jq",
                        ".labels[].name",
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if "loom:abort" in result.stdout:
                    log_warning(f"Abort signal detected for issue #{self.config.issue}")
                    return SignalType.ABORT
            except Exception:
                pass

        return None

    def _check_errored_status(self) -> bool:
        """Check shepherd progress file for errored status."""
        if not self.config.task_id:
            return False

        progress_file = (
            self.repo_root
            / ".loom"
            / "progress"
            / f"shepherd-{self.config.task_id}.json"
        )
        if not progress_file.exists():
            return False

        try:
            data = json.loads(progress_file.read_text())
            return data.get("status") == "errored"
        except Exception:
            return False

    def _check_and_resolve_prompts(self) -> bool:
        """Check for and auto-resolve interactive prompts (e.g., plan mode)."""
        pane_content = capture_pane(self.session_name)
        if not pane_content:
            return False

        # Detect Claude Code plan mode approval prompt
        if "Would you like to proceed" in pane_content:
            log_info(
                f"Plan mode approval prompt detected in {self.session_name} - "
                "auto-approving"
            )
            send_keys(self.session_name, "1")
            send_keys(self.session_name, "Enter")
            return True

        return False

    def _check_prompt_stuck(self) -> WaitResult | None:
        """Fast detection of 'stuck at prompt' state.

        Detection fires when agent has been stuck for >= age_threshold seconds.
        We check at check_interval frequency for responsiveness.
        Recovery can be re-attempted after recovery_cooldown if still stuck.
        """
        sc = self.config.stuck_config
        now = time.time()
        since_last_check = now - self._last_prompt_stuck_check

        if since_last_check < sc.prompt_stuck_check_interval:
            # Not time to check yet - but still check for processing indicators
            # to reset stuck tracking faster
            if self._prompt_stuck_since > 0:
                pane_content = capture_pane(self.session_name)
                if pane_content and PROCESSING_INDICATORS in pane_content:
                    log_info("Agent now processing - resetting stuck-at-prompt tracking")
                    self._prompt_stuck_since = 0.0
                    self._prompt_stuck_recovery_attempted = False
            return None

        self._last_prompt_stuck_check = now

        if self._is_stuck_at_prompt():
            # Agent appears stuck at prompt
            if self._prompt_stuck_since == 0:
                # First detection of stuck state
                self._prompt_stuck_since = now
                log_info(
                    "Checking for stuck-at-prompt (first detection, "
                    "waiting for age threshold)"
                )

            stuck_duration = int(now - self._prompt_stuck_since)

            # Check if recovery cooldown has elapsed (allow re-attempt)
            since_recovery = 0.0
            if self._prompt_stuck_recovery_time > 0:
                since_recovery = now - self._prompt_stuck_recovery_time
            recovery_allowed = (
                not self._prompt_stuck_recovery_attempted
                or since_recovery >= sc.prompt_stuck_recovery_cooldown
            )

            # Only take action if stuck for >= threshold AND recovery is allowed
            if (
                stuck_duration >= sc.prompt_stuck_age_threshold
                and recovery_allowed
            ):
                log_warning(
                    f"Agent stuck at prompt for {stuck_duration}s "
                    f"(total elapsed: {self.elapsed}s) - attempting recovery"
                )

                self._prompt_stuck_recovery_attempted = True
                self._prompt_stuck_recovery_time = now
                role_cmd = self._extract_role_command()

                if self._attempt_prompt_stuck_recovery(role_cmd):
                    log_success("Agent recovered from stuck-at-prompt state")
                    # Reset stuck tracking on successful recovery
                    self._prompt_stuck_since = 0.0
                    self._prompt_stuck_recovery_attempted = False
                else:
                    log_warning(
                        f"Stuck-at-prompt recovery failed - "
                        f"will retry after {sc.prompt_stuck_recovery_cooldown}s cooldown"
                    )
            elif stuck_duration < sc.prompt_stuck_age_threshold:
                # Still within initial detection period
                remaining = sc.prompt_stuck_age_threshold - stuck_duration
                log_info(
                    f"Agent may be stuck at prompt "
                    f"({stuck_duration}s/{sc.prompt_stuck_age_threshold}s threshold, "
                    f"{remaining}s until detection)"
                )
        else:
            # Agent is not stuck at prompt - reset tracking
            if self._prompt_stuck_since > 0:
                log_info("Agent no longer stuck at prompt - resetting tracking")
            self._prompt_stuck_since = 0.0
            self._prompt_stuck_recovery_attempted = False

        return None

    def _is_stuck_at_prompt(self) -> bool:
        """Check if agent is stuck at prompt - command visible but not processing."""
        pane_content = capture_pane(self.session_name)
        if not pane_content:
            return False

        # Check for role slash command visible at the prompt line
        command_at_prompt = bool(
            re.search(r"❯\s*/?(builder|judge|curator|doctor|shepherd)", pane_content)
        )

        # Check for processing indicators
        processing = PROCESSING_INDICATORS in pane_content

        return command_at_prompt and not processing

    def _extract_role_command(self) -> str:
        """Extract the likely role command from the session name for retry."""
        name = self.config.name
        if name.startswith("builder-issue-"):
            issue_num = name[len("builder-issue-") :]
            return f"/builder {issue_num}"
        return ""

    def _attempt_prompt_stuck_recovery(self, role_cmd: str) -> bool:
        """Attempt to recover an agent stuck at the prompt."""
        # Strategy 1: Try an Enter key nudge
        log_info("Trying Enter key nudge to recover stuck prompt...")
        send_keys(self.session_name, "Enter")
        time.sleep(3)

        pane_content = capture_pane(self.session_name)
        if pane_content and PROCESSING_INDICATORS in pane_content:
            log_success("Agent recovered with Enter key nudge")
            return True

        # Strategy 2: Re-send role command if available
        if role_cmd:
            log_info(f"Enter nudge failed, re-sending role command: {role_cmd}")
            time.sleep(2)
            send_keys(self.session_name, role_cmd)
            send_keys(self.session_name, "Enter")
            time.sleep(3)

            pane_content = capture_pane(self.session_name)
            if pane_content and PROCESSING_INDICATORS in pane_content:
                log_success("Agent recovered with full command retry")
                return True

        log_warning("Prompt stuck recovery failed - intervention may be needed")
        return False

    def _check_proactive_contract(self) -> bool:
        """Proactive phase contract checking with adaptive intervals.

        Uses adaptive intervals that decrease as the agent runs longer (issue #1678):
          0-180s:   No checks (initial processing delay)
          180-270s: 90s interval
          270-330s: 60s interval
          330-360s: 30s interval
          360s+:    10s interval (final rapid detection)

        Override with config.contract_interval to use a fixed interval instead.
        Set contract_interval to 0 to disable proactive checking.
        """
        if not self.config.phase:
            return False

        # Get adaptive interval based on elapsed time
        # If contract_interval is set (non-default), use it as override
        override = (
            self.config.contract_interval if self.config.contract_interval != 90 else 0
        )
        adaptive_interval = get_adaptive_contract_interval(self.elapsed, override)

        # adaptive_interval of 0 means "skip this check" (during initial delay or disabled)
        if adaptive_interval <= 0:
            return False

        now = time.time()
        since_last = now - self._last_contract_check
        if since_last < adaptive_interval:
            return False

        self._last_contract_check = now
        result = self._check_phase_contract(check_only=True)
        if result.satisfied:
            log_info(
                f"Phase contract satisfied ({result.status}) via proactive check "
                f"(interval: {adaptive_interval}s)"
            )
            log_info("Agent completed work but didn't exit - terminating session")
            return True

        return False

    def _check_idle_contract(self) -> bool:
        """Check phase contract when agent is idle."""
        if not self.config.phase:
            return False
        if self._idle_contract_checked:
            return False

        idle_time = self._get_log_idle_time()
        if idle_time < 0 or idle_time < self.config.idle_timeout:
            return False

        log_info(
            f"Agent idle for {idle_time}s (threshold: {self.config.idle_timeout}s) "
            "- checking phase contract"
        )

        result = self._check_phase_contract(check_only=True)
        if result.satisfied:
            log_info(
                f"Phase contract satisfied ({result.status}) - terminating session"
            )
            return True

        self._idle_contract_checked = True
        log_info("Phase contract not satisfied - continuing to wait")
        return False

    def _verify_phase_contract(self) -> bool:
        """Verify phase contract is satisfied."""
        result = self._check_phase_contract(check_only=True)
        return result.satisfied

    def _check_phase_contract(self, check_only: bool = False) -> ContractCheckResult:
        """Check phase contract via the Python validate_phase module."""
        if not self.config.phase or not self.config.issue:
            return ContractCheckResult(satisfied=False)

        try:
            from loom_tools.validate_phase import validate_phase

            result = validate_phase(
                phase=self.config.phase,
                issue=self.config.issue,
                repo_root=self.repo_root,
                worktree=self.config.worktree,
                pr_number=self.config.pr_number,
                check_only=check_only,
            )
            return ContractCheckResult.from_json(result.to_dict())
        except Exception:
            return ContractCheckResult(satisfied=False)

    def _get_log_idle_time(self) -> int:
        """Get seconds since log file was last modified."""
        if not self.log_file.exists():
            return -1

        try:
            mtime = self.log_file.stat().st_mtime
            return int(time.time() - mtime)
        except Exception:
            return -1

    def _check_completion_patterns(self) -> CompletionReason | None:
        """Check for role-specific completion patterns in log file."""
        if not self.log_file.exists():
            return None

        try:
            # Read last 100 lines
            with open(self.log_file) as f:
                lines = f.readlines()
            recent_log = "".join(lines[-100:])
        except Exception:
            return None

        if not recent_log:
            return None

        # Extract phase from session name
        phase = self._extract_phase()

        # Generic completion: /exit command
        if re.search(r"(^|\s+|❯\s*|>\s*)/exit\s*$", recent_log, re.MULTILINE):
            return CompletionReason.EXPLICIT_EXIT

        # Phase-specific patterns
        if phase == "builder":
            if re.search(r"https://github\.com/.*/pull/[0-9]+", recent_log):
                return CompletionReason.BUILDER_PR_CREATED
        elif phase == "judge":
            if re.search(
                r"add-label.*loom:pr|add-label.*loom:changes-requested|"
                r'--add-label "loom:pr"|--add-label "loom:changes-requested"',
                recent_log,
            ):
                return CompletionReason.JUDGE_REVIEW_COMPLETE
        elif phase == "doctor":
            if re.search(
                r"remove-label.*loom:treating.*add-label.*loom:review-requested|"
                r"remove-label.*loom:changes-requested.*add-label.*loom:review-requested",
                recent_log,
            ):
                return CompletionReason.DOCTOR_FIXES_COMPLETE
        elif phase == "curator":
            if re.search(
                r'add-label.*loom:curated|--add-label "loom:curated"', recent_log
            ):
                return CompletionReason.CURATOR_CURATION_COMPLETE
        else:
            # Unknown phase - check all patterns as fallback
            if re.search(r"https://github\.com/.*/pull/[0-9]+", recent_log):
                return CompletionReason.BUILDER_PR_CREATED
            if re.search(
                r"add-label.*loom:pr|add-label.*loom:changes-requested|"
                r'--add-label "loom:pr"|--add-label "loom:changes-requested"',
                recent_log,
            ):
                return CompletionReason.JUDGE_REVIEW_COMPLETE
            if re.search(
                r'add-label.*loom:curated|--add-label "loom:curated"', recent_log
            ):
                return CompletionReason.CURATOR_CURATION_COMPLETE

        return None

    def _extract_phase(self) -> str:
        """Extract phase from session name."""
        if self.config.phase:
            return self.config.phase

        base_name = self.config.name
        for phase in ("builder", "judge", "curator", "doctor", "shepherd"):
            if base_name.startswith(phase):
                return phase
        return ""

    async def _handle_completion(self) -> WaitResult:
        """Handle detected completion."""
        reason = self._completion_reason or CompletionReason.PHASE_CONTRACT_SATISFIED

        if reason == CompletionReason.EXPLICIT_EXIT:
            log_info(
                f"/exit detected in output - sending /exit to prompt "
                f"and terminating '{self.session_name}'"
            )
            send_keys(self.session_name, "/exit")
            send_keys(self.session_name, "Enter")
            await asyncio.sleep(1)

        # Clean up
        self._cleanup_progress_files()
        kill_session(self.session_name)

        log_success(
            f"Agent '{self.config.name}' completed ({reason.value} after {self.elapsed}s)"
        )

        return WaitResult(
            status=WaitStatus.COMPLETED,
            name=self.config.name,
            elapsed=self.elapsed,
            reason=reason,
        )

    def _check_stuck_status(self) -> WaitResult | None:
        """Check for stuck agent status."""
        sc = self.config.stuck_config
        idle_time = self.progress_tracker.get_idle_time()

        if idle_time > sc.critical_threshold and not self._stuck_critical_reported:
            self._stuck_critical_reported = True
            return self._handle_stuck("CRITICAL", idle_time)

        if (
            idle_time > sc.warning_threshold
            and not self._stuck_warned
            and not self._stuck_critical_reported
        ):
            self._stuck_warned = True
            if sc.action == StuckAction.WARN:
                log_warning(
                    f"WARNING: Agent '{self.config.name}' may be stuck "
                    f"(no progress for {idle_time}s)"
                )
            else:
                log_warning(
                    f"Agent '{self.config.name}' showing signs of being stuck "
                    f"(no progress for {idle_time}s)"
                )

        return None

    def _handle_stuck(self, status: str, idle_time: int) -> WaitResult | None:
        """Handle stuck agent based on configured action."""
        sc = self.config.stuck_config

        if sc.action == StuckAction.WARN:
            log_warning(
                f"CRITICAL: Agent '{self.config.name}' appears stuck "
                f"(no progress for {idle_time}s)"
            )
            return None

        if sc.action == StuckAction.PAUSE:
            log_warning(
                f"PAUSE: Pausing stuck agent '{self.config.name}' "
                f"(no progress for {idle_time}s)"
            )
            # Signal the agent to pause via .loom/signals
            signal_dir = self.repo_root / ".loom" / "signals"
            signal_dir.mkdir(parents=True, exist_ok=True)
            signal_file = signal_dir / f"pause-{self.config.name}"
            signal_file.write_text(
                f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} - "
                f"Auto-paused: stuck detection (idle {idle_time}s)\n"
            )
            self._cleanup_progress_files()
            return WaitResult(
                status=WaitStatus.STUCK,
                name=self.config.name,
                elapsed=self.elapsed,
                stuck_status=status,
                stuck_action="paused",
                idle_time=idle_time,
            )

        if sc.action in (StuckAction.RESTART, StuckAction.RETRY):
            action_name = "RESTART" if sc.action == StuckAction.RESTART else "RETRY"
            log_warning(
                f"{action_name}: Killing stuck agent '{self.config.name}' "
                f"(no progress for {idle_time}s)"
            )
            self._capture_stuck_diagnostics(idle_time)
            kill_session(self.session_name)
            self._cleanup_progress_files()
            return WaitResult(
                status=WaitStatus.STUCK,
                name=self.config.name,
                elapsed=self.elapsed,
                stuck_status=status,
                stuck_action=sc.action.value,
                idle_time=idle_time,
            )

        return None

    def _capture_stuck_diagnostics(self, idle_time: int) -> None:
        """Capture diagnostic information from a stuck agent."""
        diag_dir = self.repo_root / ".loom" / "diagnostics"
        diag_dir.mkdir(parents=True, exist_ok=True)

        timestamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        diag_file = diag_dir / f"stuck-{self.config.name}-{int(time.time())}.txt"

        content = [
            "=== Stuck Agent Diagnostics ===",
            f"Agent: {self.config.name}",
            f"Session: {self.session_name}",
            f"Timestamp: {timestamp}",
            f"Idle time: {idle_time}s",
            "",
            "=== Tmux Pane Content (last visible) ===",
            capture_pane(self.session_name) or "(session not available)",
            "",
            "=== Log File Tail ===",
        ]

        if self.log_file.exists():
            try:
                with open(self.log_file) as f:
                    lines = f.readlines()
                content.append("".join(lines[-50:]))
            except Exception:
                content.append("(could not read log)")
        else:
            content.append(f"(no log file found at {self.log_file})")

        diag_file.write_text("\n".join(content))
        log_info(f"Diagnostics captured to {diag_file}")

    def _cleanup_progress_files(self) -> None:
        """Clean up progress tracking files."""
        for suffix in ("", ".hash", ".time"):
            progress_file = PROGRESS_DIR / f"{self.config.name}{suffix}"
            try:
                progress_file.unlink(missing_ok=True)
            except Exception:
                pass

    async def _emit_heartbeat(self) -> None:
        """Emit periodic heartbeat to keep shepherd progress file fresh."""
        if not self.config.task_id:
            return

        now = time.time()
        since_last = now - self._last_heartbeat_time
        if since_last < self.config.heartbeat_interval:
            return

        self._last_heartbeat_time = now

        elapsed_min = self.elapsed // 60
        phase_desc = self.config.phase or "agent"

        try:
            from loom_tools.milestones import report_milestone

            report_milestone(
                self.repo_root,
                self.config.task_id,
                "heartbeat",
                quiet=True,
                action=f"{phase_desc} running ({elapsed_min}m elapsed)",
            )
        except Exception:
            pass


async def monitor_agent(config: MonitorConfig) -> WaitResult:
    """Monitor an agent and return the result when done."""
    monitor = AgentMonitor(config)
    return await monitor.monitor()


def main() -> None:
    """CLI entry point for agent monitoring."""
    import argparse
    import sys

    parser = argparse.ArgumentParser(
        description="Monitor a Claude Code agent in a tmux session"
    )
    parser.add_argument("name", help="Agent session name")
    parser.add_argument(
        "--timeout", type=int, default=3600, help="Maximum time to wait (default: 3600)"
    )
    parser.add_argument(
        "--poll-interval",
        type=int,
        default=5,
        help="Time between signal checks (default: 5)",
    )
    parser.add_argument("--issue", type=int, help="Issue number for per-issue abort")
    parser.add_argument("--task-id", help="Shepherd task ID for heartbeat emission")
    parser.add_argument("--phase", help="Phase name (curator, builder, judge, doctor)")
    parser.add_argument("--worktree", help="Worktree path for builder phase recovery")
    parser.add_argument("--pr", type=int, help="PR number for judge/doctor validation")
    parser.add_argument(
        "--idle-timeout",
        type=int,
        default=60,
        help="Time without output before checking phase contract (default: 60)",
    )
    parser.add_argument(
        "--contract-interval",
        type=int,
        default=90,
        help="Seconds between proactive phase contract checks (default: 90)",
    )
    parser.add_argument(
        "--min-idle-elapsed",
        type=int,
        default=10,
        help="Override idle prompt detection threshold (default: 10)",
    )
    parser.add_argument("--json", action="store_true", help="Output result as JSON")

    args = parser.parse_args()

    config = MonitorConfig.from_args(
        name=args.name,
        timeout=args.timeout,
        poll_interval=args.poll_interval,
        issue=args.issue,
        task_id=args.task_id,
        phase=args.phase,
        worktree=args.worktree,
        pr_number=args.pr,
        idle_timeout=args.idle_timeout,
        contract_interval=args.contract_interval,
        min_idle_elapsed=args.min_idle_elapsed,
    )

    result = asyncio.run(monitor_agent(config))

    if args.json:
        print(json.dumps(result.to_dict()))
    else:
        # Exit with appropriate code
        if result.status == WaitStatus.COMPLETED:
            log_success(
                f"Agent completed: {result.reason.value if result.reason else 'unknown'}"
            )
        elif result.status == WaitStatus.SIGNAL:
            log_warning(
                f"Signal detected: {result.signal_type.value if result.signal_type else 'unknown'}"
            )

    # Exit codes matching agent-wait-bg.sh
    exit_codes = {
        WaitStatus.COMPLETED: 0,
        WaitStatus.TIMEOUT: 1,
        WaitStatus.SESSION_NOT_FOUND: 2,
        WaitStatus.SIGNAL: 3,
        WaitStatus.STUCK: 4,
        WaitStatus.ERRORED: 4,
    }
    sys.exit(exit_codes.get(result.status, 1))


if __name__ == "__main__":
    main()
