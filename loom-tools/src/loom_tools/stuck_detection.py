"""Stuck agent detection for the Loom spawn loop.

Phase 3.1.3 port (#3392, epic #3372). The previous implementation read
``.loom/daemon-state.json`` (shepherd pool) plus per-shepherd progress files
under ``.loom/progress/shepherd-*.json``. Both producers are slated for
hard-deletion in Phase 3 of the deprecation epic. This module now reads
``.loom/spawn-loop-state.json`` (#3374, Phase 1) and matches agents on the
spawn loop's per-task entries.

Agent identifier scheme: spawn-loop tasks are addressed as ``sweep-<issue>``
(matching the per-issue log file ``.loom/logs/sweep-issue-<N>.log``).

Detection coverage tradeoffs (explicit per #3392 AC):

* **Retired**: ``missing_milestone:worktree_created`` — the spawn loop emits
  no milestone events; there is no signal to match.
* **Retired**: the daemon-state ``shepherd`` cross-check that previously
  let an operator query ``shepherd-1``/``shepherd-2``/``shepherd-3`` by
  pool slot name. Spawn-loop tasks are identified by issue number instead.
* **Kept (rewritten)**: ``extended_work`` — now computed purely from
  ``tasks[].started_at`` (no PR cross-check is bypassed; the existing forge
  query in ``ExtendedWorkDetector._check_pr_exists`` still applies).
* **Kept**: ``stale_heartbeat`` — sourced from ``tasks[].last_heartbeat``
  (a new field added in this PR; spawn-loop.sh refreshes it on every tick
  for still-alive children).
* **Kept**: ``no_progress``/``looping``/``error_spike`` — sourced from the
  per-issue sweep log at ``.loom/logs/sweep-issue-<N>.log`` (the spawn
  loop's child stdout/stderr destination).

Exit codes:
    0 - No stuck agents detected
    1 - Error occurred
    2 - Stuck agents detected
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys
from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any

from loom_tools.common.github import gh_pr_list
from loom_tools.common.logging import log_error, log_success
from loom_tools.common.paths import LoomPaths
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import (
    read_json_file,
    read_spawn_loop_state,
    read_stuck_history,
    write_json_file,
)
from loom_tools.common.time_utils import elapsed_seconds, now_utc
from loom_tools.models.spawn_loop_state import SpawnLoopTask
from loom_tools.models.stuck import (
    StuckDetection,
    StuckHistoryEntry,
    StuckMetrics,
    StuckThresholds,
)
from loom_tools.stuck_formatting import (
    format_agent_json,
    format_check_human,
    format_check_json,
    format_history_human,
    format_intervention_summary,
    format_status_human,
)


# Default thresholds
DEFAULT_IDLE_THRESHOLD = 600  # 10 minutes without output
DEFAULT_WORKING_THRESHOLD = 1800  # 30 minutes on same issue without PR
DEFAULT_LOOP_THRESHOLD = 3  # 3 similar error patterns = looping
DEFAULT_ERROR_SPIKE_THRESHOLD = 5  # 5 errors in 5 minutes
DEFAULT_HEARTBEAT_STALE = 120  # 2 minutes without heartbeat
DEFAULT_INTERVENTION_MODE = "escalate"

# Maximum history entries to keep
MAX_HISTORY_ENTRIES = 100

# Agent ID prefix used for spawn-loop tasks. Mirrors `.loom/logs/sweep-issue-<N>.log`.
SWEEP_AGENT_PREFIX = "sweep-"


@dataclass
class AgentState:
    """Current state of an agent for stuck detection.

    Populated from a :class:`SpawnLoopTask` entry. ``output_file`` points at
    the per-issue sweep log written by the spawn loop (``sweep-issue-<N>.log``).
    ``heartbeat`` is the literal ``last_heartbeat`` value from the spawn-loop
    state file (a UTC ISO8601 string or ``None``).
    """

    agent_id: str
    issue: int | None = None
    output_file: str | None = None
    started: str | None = None
    pid: int | None = None
    token: str | None = None
    status: str = "idle"
    heartbeat: str | None = None


@dataclass
class DetectionResult:
    """Result from a single detector."""

    detected: bool = False
    indicator: str | None = None
    severity: str = "none"  # none, warning, elevated, critical
    suggested_intervention: str = "none"  # none, alert, suggest, pause, clarify, escalate


class BaseDetector(ABC):
    """Base class for stuck detection strategies."""

    @abstractmethod
    def detect(
        self, agent_state: AgentState, thresholds: StuckThresholds
    ) -> DetectionResult:
        """Check if agent shows signs of being stuck."""
        raise NotImplementedError


class IdleTimeoutDetector(BaseDetector):
    """Detect agents with no output for extended time."""

    def detect(
        self, agent_state: AgentState, thresholds: StuckThresholds
    ) -> DetectionResult:
        """Check idle time from output file modification time."""
        result = DetectionResult()

        if not agent_state.output_file:
            return result

        try:
            path = pathlib.Path(agent_state.output_file)
            if not path.exists():
                return result

            mtime = path.stat().st_mtime
            import time

            now = time.time()
            idle_seconds = int(now - mtime)

            if idle_seconds >= thresholds.idle:
                result.detected = True
                result.indicator = f"no_progress:{idle_seconds}s"
                result.severity = "warning"
                result.suggested_intervention = "alert"
        except Exception:
            pass

        return result


class StaleHeartbeatDetector(BaseDetector):
    """Detect agents with stale heartbeats in spawn-loop state.

    Reads ``AgentState.heartbeat`` (sourced from
    ``spawn-loop-state.json::running[].last_heartbeat`` — written every tick
    by spawn-loop.sh's ``state_reap_dead`` for still-alive children).
    """

    def detect(
        self, agent_state: AgentState, thresholds: StuckThresholds
    ) -> DetectionResult:
        result = DetectionResult()

        if not agent_state.heartbeat:
            return result

        try:
            heartbeat_age = elapsed_seconds(agent_state.heartbeat)

            if heartbeat_age >= thresholds.heartbeat_stale:
                result.detected = True
                result.indicator = f"stale_heartbeat:{heartbeat_age}s"
                result.severity = "warning"
                result.suggested_intervention = "alert"
        except Exception:
            pass

        return result


class ExtendedWorkDetector(BaseDetector):
    """Detect agents working too long without creating a PR.

    Threshold check uses ``AgentState.started`` (sourced from
    ``spawn-loop-state.json::running[].started_at``). PR existence is still
    cross-checked against the forge — that path is unchanged by the spawn-loop
    port.
    """

    def detect(
        self, agent_state: AgentState, thresholds: StuckThresholds
    ) -> DetectionResult:
        result = DetectionResult()

        if not agent_state.started or not agent_state.issue:
            return result

        try:
            working_seconds = elapsed_seconds(agent_state.started)

            if working_seconds < thresholds.working:
                return result

            # Check if PR exists for this issue
            pr_exists = self._check_pr_exists(agent_state.issue)

            if not pr_exists:
                result.detected = True
                result.indicator = f"extended_work:{working_seconds}s"
                result.severity = "elevated"
                result.suggested_intervention = "suggest"
        except Exception:
            pass

        return result

    def _check_pr_exists(self, issue_number: int) -> bool:
        """Check if a PR exists that closes this issue."""
        try:
            prs = gh_pr_list(
                state="open",
                fields=["number", "body", "headRefName"],
            )

            for pr in prs:
                body = pr.get("body", "") or ""
                head_ref = pr.get("headRefName", "") or ""

                # Check for "Closes #N", "Fixes #N", "Resolves #N" in body
                pattern = rf"(Closes|Fixes|Resolves) #{issue_number}\b"
                if re.search(pattern, body, re.IGNORECASE):
                    return True

                # Check for issue-N in branch name
                if re.search(rf"issue-{issue_number}\b", head_ref):
                    return True

            return False
        except Exception:
            return False


class LoopDetector(BaseDetector):
    """Detect agents that are looping on repeated errors."""

    def detect(
        self, agent_state: AgentState, thresholds: StuckThresholds
    ) -> DetectionResult:
        result = DetectionResult()

        if not agent_state.output_file:
            return result

        try:
            path = pathlib.Path(agent_state.output_file)
            if not path.exists():
                return result

            lines = self._read_tail(path, 100)
            loop_count = self._count_repeated_errors(lines)

            if loop_count >= thresholds.loop:
                result.detected = True
                result.indicator = f"looping:{loop_count}x"
                result.severity = "critical"
                result.suggested_intervention = "pause"
        except Exception:
            pass

        return result

    def _read_tail(self, path: pathlib.Path, lines: int) -> list[str]:
        """Read the last N lines of a file."""
        try:
            with open(path, "rb") as f:
                f.seek(0, 2)
                file_size = f.tell()
                start_pos = max(0, file_size - lines * 100)
                f.seek(start_pos)
                content = f.read().decode("utf-8", errors="replace")
                return content.splitlines()[-lines:]
        except Exception:
            return []

    def _count_repeated_errors(self, lines: list[str]) -> int:
        """Count max repetitions of any error pattern."""
        error_pattern = re.compile(
            r"error|failed|exception|cannot|unable", re.IGNORECASE
        )
        error_lines = [line for line in lines if error_pattern.search(line)]

        if not error_lines:
            return 0

        from collections import Counter

        counts = Counter(error_lines)
        return counts.most_common(1)[0][1] if counts else 0


class ErrorSpikeDetector(BaseDetector):
    """Detect agents with many errors in a short period."""

    def detect(
        self, agent_state: AgentState, thresholds: StuckThresholds
    ) -> DetectionResult:
        result = DetectionResult()

        if not agent_state.output_file:
            return result

        try:
            path = pathlib.Path(agent_state.output_file)
            if not path.exists():
                return result

            lines = self._read_tail(path, 500)
            error_count = self._count_errors(lines)

            if error_count >= thresholds.error_spike:
                result.detected = True
                result.indicator = f"error_spike:{error_count}"
                result.severity = "elevated"
                result.suggested_intervention = "clarify"
        except Exception:
            pass

        return result

    def _read_tail(self, path: pathlib.Path, lines: int) -> list[str]:
        try:
            with open(path, "rb") as f:
                f.seek(0, 2)
                file_size = f.tell()
                start_pos = max(0, file_size - lines * 100)
                f.seek(start_pos)
                content = f.read().decode("utf-8", errors="replace")
                return content.splitlines()[-lines:]
        except Exception:
            return []

    def _count_errors(self, lines: list[str]) -> int:
        error_pattern = re.compile(
            r"error|failed|exception|panic|fatal", re.IGNORECASE
        )
        return sum(1 for line in lines if error_pattern.search(line))


# NOTE: `MissingMilestoneDetector` / `NoWorktreeDetector` were intentionally
# retired in this port (#3392). The spawn loop emits no milestone events; the
# `missing_milestone:worktree_created` signal has no equivalent under the new
# producer. Operators relying on the warning should monitor `extended_work`
# (longer threshold, but catches the same "stuck before progress" case via the
# PR-existence cross-check) or watch the per-issue log via `tail -f
# .loom/logs/sweep-issue-<N>.log` directly.


@dataclass
class StuckDetectionConfig:
    """Configuration for stuck detection."""

    idle_threshold: int = DEFAULT_IDLE_THRESHOLD
    working_threshold: int = DEFAULT_WORKING_THRESHOLD
    loop_threshold: int = DEFAULT_LOOP_THRESHOLD
    error_spike_threshold: int = DEFAULT_ERROR_SPIKE_THRESHOLD
    heartbeat_stale: int = DEFAULT_HEARTBEAT_STALE
    intervention_mode: str = DEFAULT_INTERVENTION_MODE
    updated_at: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> StuckDetectionConfig:
        return cls(
            idle_threshold=data.get("idle_threshold", DEFAULT_IDLE_THRESHOLD),
            working_threshold=data.get("working_threshold", DEFAULT_WORKING_THRESHOLD),
            loop_threshold=data.get("loop_threshold", DEFAULT_LOOP_THRESHOLD),
            error_spike_threshold=data.get(
                "error_spike_threshold", DEFAULT_ERROR_SPIKE_THRESHOLD
            ),
            heartbeat_stale=data.get("heartbeat_stale", DEFAULT_HEARTBEAT_STALE),
            intervention_mode=data.get("intervention_mode", DEFAULT_INTERVENTION_MODE),
            updated_at=data.get("updated_at"),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "idle_threshold": self.idle_threshold,
            "working_threshold": self.working_threshold,
            "loop_threshold": self.loop_threshold,
            "error_spike_threshold": self.error_spike_threshold,
            "heartbeat_stale": self.heartbeat_stale,
            "intervention_mode": self.intervention_mode,
        }
        if self.updated_at:
            d["updated_at"] = self.updated_at
        return d

    def to_thresholds(self) -> StuckThresholds:
        return StuckThresholds(
            idle=self.idle_threshold,
            working=self.working_threshold,
            loop=self.loop_threshold,
            error_spike=self.error_spike_threshold,
            heartbeat_stale=self.heartbeat_stale,
        )


def _agent_id_for_issue(issue: int) -> str:
    """Return the canonical stuck-detection agent ID for a spawn-loop task."""
    return f"{SWEEP_AGENT_PREFIX}{issue}"


def _issue_from_agent_id(agent_id: str) -> int | None:
    """Reverse of :func:`_agent_id_for_issue`. Returns ``None`` if not a sweep ID."""
    if not agent_id.startswith(SWEEP_AGENT_PREFIX):
        return None
    suffix = agent_id[len(SWEEP_AGENT_PREFIX):]
    try:
        return int(suffix)
    except ValueError:
        return None


def _agent_state_from_task(
    task: SpawnLoopTask, paths: LoomPaths
) -> AgentState:
    """Build an :class:`AgentState` from a :class:`SpawnLoopTask`."""
    log_path = paths.logs_dir / f"sweep-issue-{task.issue}.log"
    return AgentState(
        agent_id=_agent_id_for_issue(task.issue),
        issue=task.issue,
        output_file=str(log_path),
        started=task.started_at,
        pid=task.pid,
        token=task.token,
        status="working",
        heartbeat=task.last_heartbeat,
    )


class StuckDetectionRunner:
    """Orchestrates all stuck detection strategies against the spawn loop."""

    def __init__(
        self,
        repo_root: pathlib.Path,
        config: StuckDetectionConfig | None = None,
    ):
        self.repo_root = repo_root
        self.paths = LoomPaths(repo_root)
        self.loom_dir = self.paths.loom_dir
        self.config = config or self._load_config()
        self.thresholds = self.config.to_thresholds()

        # Detector set after the #3392 port. MissingMilestoneDetector /
        # NoWorktreeDetector removed (no spawn-loop equivalent signal).
        self.detectors: list[BaseDetector] = [
            StaleHeartbeatDetector(),
            IdleTimeoutDetector(),
            ExtendedWorkDetector(),
            LoopDetector(),
            ErrorSpikeDetector(),
        ]

    def _load_config(self) -> StuckDetectionConfig:
        """Load configuration from .loom/stuck-config.json."""
        config_path = self.loom_dir / "stuck-config.json"
        data = read_json_file(config_path)
        if isinstance(data, dict):
            return StuckDetectionConfig.from_dict(data)
        return StuckDetectionConfig()

    def save_config(self) -> None:
        """Save configuration to .loom/stuck-config.json."""
        config_path = self.loom_dir / "stuck-config.json"
        data = self.config.to_dict()
        data["updated_at"] = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        write_json_file(config_path, data)

    def _find_task(self, agent_id: str) -> SpawnLoopTask | None:
        """Look up a spawn-loop task by stuck-detection agent ID.

        Agent IDs use the ``sweep-<issue>`` form. Returns ``None`` if no
        matching task is currently tracked by the spawn loop.
        """
        issue = _issue_from_agent_id(agent_id)
        if issue is None:
            return None
        state = read_spawn_loop_state(self.repo_root)
        for task in state.running:
            if task.issue == issue:
                return task
        return None

    def check_agent(self, agent_id: str, verbose: bool = False) -> StuckDetection:
        """Check a single agent for stuck indicators."""
        task = self._find_task(agent_id)

        if not task:
            return StuckDetection(
                agent_id=agent_id,
                status="unknown",
                checked_at=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
            )

        agent_state = _agent_state_from_task(task, self.paths)
        return self._run_detection(agent_state)

    def _run_detection(self, agent_state: AgentState) -> StuckDetection:
        """Run all detectors and combine results."""
        indicators: list[str] = []
        severity = "none"
        suggested_intervention = "none"
        stuck = False

        heartbeat_age = self._get_heartbeat_age(agent_state)
        working_seconds = self._get_working_seconds(agent_state)
        loop_count = self._get_loop_count(agent_state)
        error_count = self._get_error_count(agent_state)

        # Prefer heartbeat-based idle detection if available (matches bash behavior).
        # When heartbeat is available, use it as idle_seconds; otherwise fall back
        # to the output file's mtime.
        if heartbeat_age >= 0:
            idle_seconds = heartbeat_age
        else:
            idle_seconds = self._get_idle_seconds(agent_state)

        # Check idle threshold
        if idle_seconds >= self.thresholds.idle:
            stuck = True
            if heartbeat_age >= 0:
                indicators.append(f"stale_heartbeat:{idle_seconds}s")
            else:
                indicators.append(f"no_progress:{idle_seconds}s")
            severity = "warning"
            suggested_intervention = "alert"

        # Extended work (uses started_at + PR cross-check via forge).
        if working_seconds >= self.thresholds.working:
            pr_exists = ExtendedWorkDetector()._check_pr_exists(agent_state.issue or 0)
            if not pr_exists:
                stuck = True
                indicators.append(f"extended_work:{working_seconds}s")
                if severity in ("none", "warning"):
                    severity = "elevated"
                suggested_intervention = "suggest"

        # Looping
        if loop_count >= self.thresholds.loop:
            stuck = True
            indicators.append(f"looping:{loop_count}x")
            severity = "critical"
            suggested_intervention = "pause"

        # Error spike
        if error_count >= self.thresholds.error_spike:
            stuck = True
            indicators.append(f"error_spike:{error_count}")
            if severity != "critical":
                severity = "elevated"
            if suggested_intervention in ("none", "alert"):
                suggested_intervention = "clarify"

        # NOTE: missing_milestone:worktree_created path retired (#3392).
        # No spawn-loop signal corresponds to milestone events.

        return StuckDetection(
            agent_id=agent_state.agent_id,
            issue=agent_state.issue,
            status="working",
            stuck=stuck,
            severity=severity,
            suggested_intervention=suggested_intervention,
            indicators=indicators,
            metrics=StuckMetrics(
                idle_seconds=idle_seconds,
                working_seconds=working_seconds,
                loop_count=loop_count,
                error_count=error_count,
                heartbeat_age=heartbeat_age if heartbeat_age >= 0 else None,
                current_phase="sweep",  # Spawn loop runs only the sweep skill.
            ),
            thresholds=self.thresholds,
            checked_at=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
        )

    def _get_idle_seconds(self, agent_state: AgentState) -> int:
        """Get idle time from output file modification time."""
        if not agent_state.output_file:
            return -1

        try:
            path = pathlib.Path(agent_state.output_file)
            if not path.exists():
                return -1

            import time

            mtime = path.stat().st_mtime
            now = time.time()
            return int(now - mtime)
        except Exception:
            return -1

    def _get_heartbeat_age(self, agent_state: AgentState) -> int:
        """Get heartbeat age from spawn-loop state."""
        if not agent_state.heartbeat:
            return -1
        try:
            return elapsed_seconds(agent_state.heartbeat)
        except Exception:
            return -1

    def _get_working_seconds(self, agent_state: AgentState) -> int:
        if not agent_state.started:
            return 0
        try:
            return elapsed_seconds(agent_state.started)
        except Exception:
            return 0

    def _get_loop_count(self, agent_state: AgentState) -> int:
        if not agent_state.output_file:
            return 0
        try:
            detector = LoopDetector()
            path = pathlib.Path(agent_state.output_file)
            if not path.exists():
                return 0
            lines = detector._read_tail(path, 100)
            return detector._count_repeated_errors(lines)
        except Exception:
            return 0

    def _get_error_count(self, agent_state: AgentState) -> int:
        if not agent_state.output_file:
            return 0
        try:
            detector = ErrorSpikeDetector()
            path = pathlib.Path(agent_state.output_file)
            if not path.exists():
                return 0
            lines = detector._read_tail(path, 500)
            return detector._count_errors(lines)
        except Exception:
            return 0

    def check_all(self) -> tuple[list[StuckDetection], list[str]]:
        """Check every spawn-loop task for stuck indicators.

        Returns (per-agent detections, list of stuck agent IDs). When the spawn
        loop has no tracked children, returns ``([], [])`` — the spawn loop
        may simply not be running, or may have legitimately nothing to do.
        """
        results: list[StuckDetection] = []
        stuck_agents: list[str] = []

        state = read_spawn_loop_state(self.repo_root)

        for task in state.running:
            agent_state = _agent_state_from_task(task, self.paths)
            detection = self._run_detection(agent_state)
            results.append(detection)

            if detection.stuck:
                stuck_agents.append(detection.agent_id)
                self._record_detection(detection)

                if self.config.intervention_mode != "none":
                    self._trigger_intervention(detection)

        return results, stuck_agents

    def _record_detection(self, detection: StuckDetection) -> None:
        """Record detection in history file."""
        history_path = self.loom_dir / "stuck-history.json"
        history = read_stuck_history(self.repo_root)

        entry = StuckHistoryEntry(
            detected_at=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
            detection=detection,
        )

        history.entries.append(entry)

        if len(history.entries) > MAX_HISTORY_ENTRIES:
            history.entries = history.entries[-MAX_HISTORY_ENTRIES:]

        if not history.created_at:
            history.created_at = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

        write_json_file(history_path, history.to_dict())

    def _trigger_intervention(self, detection: StuckDetection) -> None:
        """Trigger intervention for stuck agent."""
        interventions_dir = self.loom_dir / "interventions"
        interventions_dir.mkdir(exist_ok=True)

        timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        file_timestamp = now_utc().strftime("%Y%m%d%H%M%S")

        intervention_file = (
            interventions_dir / f"{detection.agent_id}-{file_timestamp}.json"
        )
        intervention_data = {
            "agent_id": detection.agent_id,
            "issue": detection.issue,
            "intervention_type": detection.suggested_intervention,
            "severity": detection.severity,
            "indicators": ", ".join(detection.indicators),
            "triggered_at": timestamp,
            "status": "pending",
            "detection": detection.to_dict(),
        }
        write_json_file(intervention_file, intervention_data)

        summary_file = interventions_dir / f"{detection.agent_id}-latest.txt"
        summary = format_intervention_summary(detection, timestamp, self.loom_dir)
        summary_file.write_text(summary)

        if detection.suggested_intervention in ("pause", "escalate"):
            self._pause_agent(detection)

    def _pause_agent(self, detection: StuckDetection) -> None:
        """Pause an agent via signal.sh.

        Note: signal.sh was written for the daemon's named shepherd pool
        (``shepherd-1`` etc.). For spawn-loop tasks we pass the ``sweep-<N>``
        ID; if signal.sh doesn't recognize it the call is a no-op (we still
        write the intervention file so operators have a record).
        """
        signal_script = self.repo_root / ".loom" / "scripts" / "signal.sh"
        indicators = ", ".join(detection.indicators)
        message = f"Auto-paused: stuck detection ({indicators})"

        if detection.suggested_intervention == "escalate":
            message = f"ESCALATION: stuck detection ({indicators})"

        if signal_script.exists():
            try:
                subprocess.run(
                    [str(signal_script), "stop", detection.agent_id, message],
                    capture_output=True,
                    check=False,
                )
            except Exception:
                pass


def cmd_check(args: argparse.Namespace) -> int:
    """Handle the 'check' command."""
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        if args.json:
            print('{"error":"no loom directory","stuck_agents":[],"total_checked":0}')
        return 1

    runner = StuckDetectionRunner(repo_root)
    results, stuck_agents = runner.check_all()

    if args.json:
        print(format_check_json(results, stuck_agents, runner.config))
    else:
        print(format_check_human(results, stuck_agents, runner.config))

    return 2 if stuck_agents else 0


def cmd_check_agent(args: argparse.Namespace) -> int:
    """Handle the 'check-agent' command."""
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    runner = StuckDetectionRunner(repo_root)
    detection = runner.check_agent(args.agent_id, args.verbose)

    print(format_agent_json(detection))
    return 2 if detection.stuck else 0


def cmd_status(args: argparse.Namespace) -> int:
    """Handle the 'status' command."""
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    runner = StuckDetectionRunner(repo_root)
    print(format_status_human(repo_root, runner.config))
    return 0


def cmd_configure(args: argparse.Namespace) -> int:
    """Handle the 'configure' command."""
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    runner = StuckDetectionRunner(repo_root)

    if args.idle_threshold is not None:
        runner.config.idle_threshold = args.idle_threshold
    if args.working_threshold is not None:
        runner.config.working_threshold = args.working_threshold
    if args.loop_threshold is not None:
        runner.config.loop_threshold = args.loop_threshold
    if args.error_threshold is not None:
        runner.config.error_spike_threshold = args.error_threshold
    if args.intervention_mode is not None:
        runner.config.intervention_mode = args.intervention_mode

    runner.save_config()

    log_success(f"Configuration saved to {runner.loom_dir / 'stuck-config.json'}")
    print()
    print(f"  Idle threshold: {runner.config.idle_threshold}s")
    print(f"  Working threshold: {runner.config.working_threshold}s")
    print(f"  Loop threshold: {runner.config.loop_threshold}x")
    print(f"  Error spike threshold: {runner.config.error_spike_threshold}")
    print(f"  Intervention mode: {runner.config.intervention_mode}")

    return 0


def cmd_history(args: argparse.Namespace) -> int:
    """Handle the 'history' command."""
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    print(format_history_human(repo_root, args.agent_id))
    return 0


def cmd_intervene(args: argparse.Namespace) -> int:
    """Handle the 'intervene' command."""
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    runner = StuckDetectionRunner(repo_root)
    detection = runner.check_agent(args.agent_id)

    detection.suggested_intervention = args.type

    runner._trigger_intervention(detection)

    log_success(f"Intervention triggered for {args.agent_id}")
    print(f"  Type: {args.type}")
    print(f"  Message: {args.message or 'Manual intervention triggered'}")
    print(
        f"  Details: {runner.loom_dir / 'interventions' / f'{args.agent_id}-latest.txt'}"
    )

    return 0


def cmd_clear(args: argparse.Namespace) -> int:
    """Handle the 'clear' command."""
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    interventions_dir = repo_root / ".loom" / "interventions"
    signal_script = repo_root / ".loom" / "scripts" / "signal.sh"

    if args.target == "all":
        if interventions_dir.exists():
            for f in interventions_dir.glob("*.json"):
                f.unlink()
            for f in interventions_dir.glob("*.txt"):
                f.unlink()
        log_success("Cleared all intervention files")
    else:
        if interventions_dir.exists():
            for f in interventions_dir.glob(f"{args.target}-*.json"):
                f.unlink()
            for f in interventions_dir.glob(f"{args.target}-*.txt"):
                f.unlink()

        if signal_script.exists():
            try:
                subprocess.run(
                    [str(signal_script), "clear", args.target],
                    capture_output=True,
                    check=False,
                )
            except Exception:
                pass

        log_success(f"Cleared interventions for {args.target}")

    return 0


def main(argv: list[str] | None = None) -> int:
    """Main entry point for stuck detection CLI."""
    parser = argparse.ArgumentParser(
        description="Stuck agent detection for the Loom spawn loop",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Exit codes:
    0 - No stuck agents detected
    1 - Error occurred
    2 - Stuck agents detected

Stuck Indicators (post-#3392 spawn-loop port):
    1. No Progress / Stale Heartbeat - No output for extended time, or the
       spawn loop has not refreshed the per-task heartbeat (default: 10 min
       for log mtime, 2 min for heartbeat).
    2. Extended Work - Same issue for >30 min without an open PR closing it.
    3. Looping - Repeated similar error patterns in the per-issue sweep log.
    4. Error Spike - Multiple errors in short period in the per-issue sweep log.

Retired indicators (no replacement under the spawn loop):
    - Missing milestone (worktree_created) — spawn loop emits no milestones.

Intervention Types:
    alert     - Notify human observer
    suggest   - Suggest role switch (e.g., Builder -> Doctor)
    pause     - Auto-pause agent with summary
    clarify   - Request clarification from issue author
    escalate  - Full escalation chain
""",
    )

    subparsers = parser.add_subparsers(dest="command", help="Available commands")

    # check command
    check_parser = subparsers.add_parser(
        "check", help="Check all agents for stuck indicators"
    )
    check_parser.add_argument(
        "--json", action="store_true", help="Output as JSON"
    )
    check_parser.add_argument(
        "--verbose", "-v", action="store_true", help="Verbose output"
    )

    # check-agent command
    check_agent_parser = subparsers.add_parser(
        "check-agent", help="Check specific agent (sweep-<issue>)"
    )
    check_agent_parser.add_argument(
        "agent_id", help="Agent ID (e.g., sweep-42)"
    )
    check_agent_parser.add_argument(
        "--verbose", "-v", action="store_true", help="Verbose output"
    )

    # status command
    subparsers.add_parser("status", help="Show stuck detection status summary")

    # configure command
    configure_parser = subparsers.add_parser(
        "configure", help="Configure thresholds"
    )
    configure_parser.add_argument(
        "--idle-threshold", type=int, help="Idle threshold in seconds"
    )
    configure_parser.add_argument(
        "--working-threshold", type=int, help="Working threshold in seconds"
    )
    configure_parser.add_argument(
        "--loop-threshold", type=int, help="Loop threshold count"
    )
    configure_parser.add_argument(
        "--error-threshold", type=int, help="Error spike threshold"
    )
    configure_parser.add_argument(
        "--intervention-mode",
        choices=["none", "alert", "suggest", "pause", "clarify", "escalate"],
        help="Intervention mode",
    )

    # history command
    history_parser = subparsers.add_parser(
        "history", help="Show intervention history"
    )
    history_parser.add_argument(
        "agent_id", nargs="?", help="Filter by agent ID"
    )

    # intervene command
    intervene_parser = subparsers.add_parser(
        "intervene", help="Manually trigger intervention"
    )
    intervene_parser.add_argument("agent_id", help="Agent ID (e.g., sweep-42)")
    intervene_parser.add_argument(
        "type",
        choices=["alert", "suggest", "pause", "clarify", "escalate"],
        help="Intervention type",
    )
    intervene_parser.add_argument(
        "message", nargs="?", help="Optional message"
    )

    # clear command
    clear_parser = subparsers.add_parser(
        "clear", help="Clear stuck state/interventions"
    )
    clear_parser.add_argument(
        "target", help="Agent ID or 'all'"
    )

    args = parser.parse_args(argv)

    if args.command is None:
        parser.print_help()
        return 0

    if args.command == "check":
        return cmd_check(args)
    elif args.command == "check-agent":
        return cmd_check_agent(args)
    elif args.command == "status":
        return cmd_status(args)
    elif args.command == "configure":
        return cmd_configure(args)
    elif args.command == "history":
        return cmd_history(args)
    elif args.command == "intervene":
        return cmd_intervene(args)
    elif args.command == "clear":
        return cmd_clear(args)

    return 0


if __name__ == "__main__":
    sys.exit(main())
