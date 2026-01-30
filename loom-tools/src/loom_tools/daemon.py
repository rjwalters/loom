"""Loom Daemon Loop - Python implementation for robust continuous operation.

This module implements the daemon loop that orchestrates Loom iterations,
delegating iteration work to Claude via the /loom iterate command.

Features:
    - Deterministic loop behavior (no LLM interpretation variability)
    - Configurable poll interval via environment variable
    - Timeout protection prevents hung iterations
    - Exponential backoff on repeated failures (configurable)
    - Graceful shutdown via .loom/stop-daemon signal file
    - Session state rotation on startup
    - Force mode support passed to iterations
    - PID file prevents multiple instances (.loom/daemon-loop.pid)
    - Iteration metrics and health reporting (.loom/daemon-metrics.json)
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import now_utc


# Configuration defaults from environment
POLL_INTERVAL = int(os.environ.get("LOOM_POLL_INTERVAL", "120"))
ITERATION_TIMEOUT = int(os.environ.get("LOOM_ITERATION_TIMEOUT", "300"))
MAX_BACKOFF = int(os.environ.get("LOOM_MAX_BACKOFF", "1800"))
BACKOFF_MULTIPLIER = int(os.environ.get("LOOM_BACKOFF_MULTIPLIER", "2"))
BACKOFF_THRESHOLD = int(os.environ.get("LOOM_BACKOFF_THRESHOLD", "3"))
SLOW_ITERATION_THRESHOLD_MULTIPLIER = int(
    os.environ.get("LOOM_SLOW_ITERATION_THRESHOLD_MULTIPLIER", "2")
)

# File paths relative to repo root
LOG_FILE = ".loom/daemon.log"
STATE_FILE = ".loom/daemon-state.json"
METRICS_FILE = ".loom/daemon-metrics.json"
STOP_SIGNAL = ".loom/stop-daemon"
PID_FILE = ".loom/daemon-loop.pid"


@dataclass
class DaemonConfig:
    """Configuration for the daemon loop."""

    poll_interval: int = POLL_INTERVAL
    iteration_timeout: int = ITERATION_TIMEOUT
    max_backoff: int = MAX_BACKOFF
    backoff_multiplier: int = BACKOFF_MULTIPLIER
    backoff_threshold: int = BACKOFF_THRESHOLD
    slow_iteration_multiplier: int = SLOW_ITERATION_THRESHOLD_MULTIPLIER
    force_mode: bool = False
    debug_mode: bool = False


@dataclass
class IterationResult:
    """Result of a single daemon iteration."""

    status: str  # "success", "failure", "timeout"
    duration_seconds: int
    summary: str
    warn_codes: list[str] = field(default_factory=list)


@dataclass
class DaemonMetrics:
    """Metrics tracked across daemon iterations."""

    session_start: str = ""
    total_iterations: int = 0
    successful_iterations: int = 0
    failed_iterations: int = 0
    timeout_iterations: int = 0
    iteration_durations: list[int] = field(default_factory=list)
    average_iteration_seconds: int = 0
    last_iteration: dict[str, Any] | None = None
    health_status: str = "healthy"
    consecutive_failures: int = 0
    last_success: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> DaemonMetrics:
        """Create metrics from dictionary."""
        health = data.get("health", {})
        return cls(
            session_start=data.get("session_start", ""),
            total_iterations=data.get("total_iterations", 0),
            successful_iterations=data.get("successful_iterations", 0),
            failed_iterations=data.get("failed_iterations", 0),
            timeout_iterations=data.get("timeout_iterations", 0),
            iteration_durations=data.get("iteration_durations", []),
            average_iteration_seconds=data.get("average_iteration_seconds", 0),
            last_iteration=data.get("last_iteration"),
            health_status=health.get("status", "healthy"),
            consecutive_failures=health.get("consecutive_failures", 0),
            last_success=health.get("last_success"),
        )

    def to_dict(self) -> dict[str, Any]:
        """Convert metrics to dictionary."""
        return {
            "session_start": self.session_start,
            "total_iterations": self.total_iterations,
            "successful_iterations": self.successful_iterations,
            "failed_iterations": self.failed_iterations,
            "timeout_iterations": self.timeout_iterations,
            "iteration_durations": self.iteration_durations,
            "average_iteration_seconds": self.average_iteration_seconds,
            "last_iteration": self.last_iteration,
            "health": {
                "status": self.health_status,
                "consecutive_failures": self.consecutive_failures,
                "last_success": self.last_success,
            },
        }

    def record_iteration(
        self,
        status: str,
        duration: int,
        summary: str,
    ) -> None:
        """Record the result of an iteration."""
        timestamp = now_utc().isoformat().replace("+00:00", "Z")

        self.total_iterations += 1
        self.last_iteration = {
            "timestamp": timestamp,
            "duration_seconds": duration,
            "status": status,
            "summary": summary,
        }

        if status == "success":
            self.successful_iterations += 1
            self.consecutive_failures = 0
            self.last_success = timestamp
            self.health_status = "healthy"
        elif status == "timeout":
            self.timeout_iterations += 1
            self.consecutive_failures += 1
        else:
            self.failed_iterations += 1
            self.consecutive_failures += 1

        # Update health status
        if self.consecutive_failures >= 3:
            self.health_status = "unhealthy"

        # Update rolling average (keep last 100 durations)
        self.iteration_durations = (self.iteration_durations + [duration])[-100:]
        if self.iteration_durations:
            self.average_iteration_seconds = sum(self.iteration_durations) // len(
                self.iteration_durations
            )


class DaemonLoop:
    """Main daemon loop controller."""

    def __init__(self, config: DaemonConfig, repo_root: pathlib.Path) -> None:
        self.config = config
        self.repo_root = repo_root
        self.session_id = f"{int(time.time())}-{os.getpid()}"
        self.iteration = 0
        self.consecutive_failures = 0
        self.current_backoff = config.poll_interval
        self.running = True
        self.log_file = repo_root / LOG_FILE
        self.state_file = repo_root / STATE_FILE
        self.metrics_file = repo_root / METRICS_FILE
        self.stop_signal = repo_root / STOP_SIGNAL
        self.pid_file = repo_root / PID_FILE
        self.metrics = DaemonMetrics(
            session_start=now_utc().isoformat().replace("+00:00", "Z")
        )

    def log(self, message: str) -> None:
        """Log a message to console and log file."""
        timestamp = now_utc().isoformat().replace("+00:00", "Z")
        line = f"{timestamp} {message}"
        print(line)
        try:
            with open(self.log_file, "a") as f:
                f.write(line + "\n")
        except Exception:
            pass

    def check_stop_signal(self) -> bool:
        """Check if the stop signal file exists."""
        return self.stop_signal.exists()

    def validate_session_ownership(self) -> bool:
        """Validate that we still own the daemon session.

        Returns False if another daemon has taken over.
        """
        if not self.state_file.exists():
            return True

        try:
            data = read_json_file(self.state_file)
            if isinstance(data, list):
                return True
            file_session_id = data.get("daemon_session_id")
            if file_session_id and file_session_id != self.session_id:
                return False
        except Exception:
            pass

        return True

    def init_state_file(self) -> None:
        """Initialize or update the daemon state file."""
        timestamp = now_utc().isoformat().replace("+00:00", "Z")

        if self.state_file.exists():
            try:
                data = read_json_file(self.state_file)
                if isinstance(data, dict):
                    data["force_mode"] = self.config.force_mode
                    data["started_at"] = timestamp
                    data["running"] = True
                    data["iteration"] = 0
                    data["daemon_session_id"] = self.session_id
                    write_json_file(self.state_file, data)
                    return
            except Exception:
                pass

        # Create fresh state file
        data = {
            "started_at": timestamp,
            "last_poll": None,
            "running": True,
            "iteration": 0,
            "force_mode": self.config.force_mode,
            "daemon_session_id": self.session_id,
            "shepherds": {},
            "completed_issues": [],
            "total_prs_merged": 0,
        }
        write_json_file(self.state_file, data)

    def init_metrics_file(self) -> None:
        """Initialize metrics file for new session."""
        write_json_file(self.metrics_file, self.metrics.to_dict())

    def update_metrics(
        self,
        status: str,
        duration: int,
        summary: str,
    ) -> None:
        """Update metrics file after an iteration."""
        self.metrics.record_iteration(status, duration, summary)
        try:
            write_json_file(self.metrics_file, self.metrics.to_dict())
        except Exception as e:
            self.log(f"Warning: Failed to update metrics file: {e}")

    def update_state_timing(self) -> None:
        """Update iteration timing summary in daemon-state.json."""
        if not self.state_file.exists() or not self.metrics_file.exists():
            return

        try:
            state_data = read_json_file(self.state_file)
            if isinstance(state_data, list):
                return

            metrics_data = read_json_file(self.metrics_file)
            if isinstance(metrics_data, list):
                return

            last_duration = 0
            if metrics_data.get("last_iteration"):
                last_duration = metrics_data["last_iteration"].get("duration_seconds", 0)

            avg_duration = metrics_data.get("average_iteration_seconds", 0)
            durations = metrics_data.get("iteration_durations", [])
            max_duration = max(durations) if durations else 0

            state_data["iteration_timing"] = {
                "last_duration_seconds": last_duration,
                "avg_duration_seconds": avg_duration,
                "max_duration_seconds": max_duration,
            }

            write_json_file(self.state_file, state_data)
        except Exception:
            pass

    def check_slow_iteration(self, duration: int) -> None:
        """Log warning if iteration was slow."""
        if self.metrics.total_iterations < 3:
            return

        avg = self.metrics.average_iteration_seconds
        if avg == 0:
            return

        threshold = avg * self.config.slow_iteration_multiplier
        if duration > threshold:
            self.log(
                f"WARNING: Slow iteration detected - {duration}s exceeds "
                f"{self.config.slow_iteration_multiplier}x average ({avg}s, threshold: {threshold}s)"
            )

    def run_iteration(self) -> IterationResult:
        """Run a single daemon iteration via Claude CLI."""
        start_time = time.time()

        # Build the command
        cmd_parts = ["/loom", "iterate"]
        if self.config.force_mode:
            cmd_parts.append("--force")
        if self.config.debug_mode:
            cmd_parts.append("--debug")

        iterate_cmd = " ".join(cmd_parts)

        try:
            result = subprocess.run(
                ["claude", "--print", iterate_cmd],
                capture_output=True,
                text=True,
                timeout=self.config.iteration_timeout,
                cwd=self.repo_root,
            )

            output = result.stdout

            # Extract summary line
            summary = ""
            for line in output.split("\n"):
                if line.startswith("ready="):
                    summary = line
                    break

            if not summary:
                if "shutdown" in output.lower():
                    summary = "SHUTDOWN_SIGNAL"
                elif "error" in output.lower():
                    for line in output.split("\n"):
                        if "error" in line.lower():
                            summary = f"ERROR: {line[:80]}"
                            break
                elif "complete" in output.lower() or "success" in output.lower():
                    summary = "completed"
                else:
                    # Take last non-empty line
                    lines = [l for l in output.split("\n") if l.strip()]
                    summary = lines[-1][:80] if lines else "no output"

            duration = int(time.time() - start_time)
            status = "success"

            if "ERROR" in summary:
                status = "failure"

            # Extract WARN: codes
            warn_codes = []
            if "WARN:" in summary:
                for token in summary.split():
                    if token.startswith("WARN:"):
                        warn_codes.append(token[5:])

            return IterationResult(
                status=status,
                duration_seconds=duration,
                summary=summary,
                warn_codes=warn_codes,
            )

        except subprocess.TimeoutExpired:
            duration = int(time.time() - start_time)
            return IterationResult(
                status="timeout",
                duration_seconds=duration,
                summary=f"TIMEOUT (iteration exceeded {self.config.iteration_timeout}s)",
            )

        except Exception as e:
            duration = int(time.time() - start_time)
            return IterationResult(
                status="failure",
                duration_seconds=duration,
                summary=f"ERROR: {e}",
            )

    def persist_warnings(self, warn_codes: list[str]) -> None:
        """Persist warnings to daemon-state.json."""
        if not self.state_file.exists():
            return

        try:
            data = read_json_file(self.state_file)
            if isinstance(data, list):
                return

            timestamp = now_utc().isoformat().replace("+00:00", "Z")

            if warn_codes:
                warnings = [
                    {
                        "time": timestamp,
                        "type": code,
                        "severity": "warning",
                        "message": f"Detected by daemon iteration {self.iteration}",
                        "context": {},
                        "acknowledged": False,
                    }
                    for code in warn_codes
                ]
                data["warnings"] = warnings
            else:
                data["warnings"] = []

            write_json_file(self.state_file, data)
        except Exception:
            pass

    def collect_health_metrics(self) -> None:
        """Run health-check.sh --collect if available."""
        health_check = self.repo_root / ".loom" / "scripts" / "health-check.sh"
        if health_check.exists() and os.access(health_check, os.X_OK):
            try:
                subprocess.run(
                    [str(health_check), "--collect"],
                    capture_output=True,
                    timeout=30,
                    cwd=self.repo_root,
                )
            except Exception:
                pass

    def check_pipeline_stalled(self) -> bool:
        """Check if the pipeline is stalled using health metrics."""
        health_metrics = self.repo_root / ".loom" / "health-metrics.json"
        if not health_metrics.exists():
            return False

        try:
            data = read_json_file(health_metrics)
            if isinstance(data, list):
                return False

            metrics = data.get("metrics", [])
            if not metrics:
                return False

            last_metric = metrics[-1]
            pipeline_health = last_metric.get("pipeline_health", {})
            return pipeline_health.get("status") == "stalled"
        except Exception:
            return False

    def update_backoff(self, success: bool, pipeline_stalled: bool = False) -> None:
        """Update backoff based on iteration result."""
        if success and not pipeline_stalled:
            # Reset backoff
            if self.consecutive_failures > 0 or self.current_backoff != self.config.poll_interval:
                self.consecutive_failures = 0
                self.current_backoff = self.config.poll_interval
                self.log(f"Backoff reset to {self.config.poll_interval}s")
        else:
            # Track failure and potentially increase backoff
            self.consecutive_failures += 1
            if self.consecutive_failures >= self.config.backoff_threshold:
                new_backoff = self.current_backoff * self.config.backoff_multiplier
                if new_backoff > self.config.max_backoff:
                    new_backoff = self.config.max_backoff
                if new_backoff != self.current_backoff:
                    self.current_backoff = new_backoff
                    if pipeline_stalled:
                        self.log(f"Pipeline stalled - increasing backoff to {self.current_backoff}s")
                    else:
                        self.log(f"Backing off to {self.current_backoff}s (failure {self.consecutive_failures})")
            elif pipeline_stalled:
                self.log(
                    f"Pipeline stalled - maintaining backoff at {self.current_backoff}s "
                    f"(soft failure {self.consecutive_failures}/{self.config.backoff_threshold})"
                )

    def cleanup(self) -> None:
        """Clean up on exit."""
        self.log("Daemon loop terminated")

        # Remove stop signal and PID file
        try:
            self.stop_signal.unlink(missing_ok=True)
        except Exception:
            pass

        try:
            self.pid_file.unlink(missing_ok=True)
        except Exception:
            pass

        # Update state file to mark as not running
        if self.state_file.exists():
            try:
                data = read_json_file(self.state_file)
                if isinstance(data, dict):
                    data["running"] = False
                    data["stopped_at"] = now_utc().isoformat().replace("+00:00", "Z")
                    write_json_file(self.state_file, data)
            except Exception:
                pass

    def rotate_state_file(self) -> None:
        """Rotate existing state file if present."""
        rotate_script = self.repo_root / ".loom" / "scripts" / "rotate-daemon-state.sh"
        if rotate_script.exists() and self.state_file.exists():
            self.log("Rotating previous daemon state...")
            try:
                subprocess.run(
                    [str(rotate_script)],
                    capture_output=True,
                    timeout=30,
                    cwd=self.repo_root,
                )
            except Exception:
                pass

    def archive_metrics_file(self) -> None:
        """Archive metrics file if it has meaningful data."""
        if not self.metrics_file.exists():
            return

        try:
            data = read_json_file(self.metrics_file)
            if isinstance(data, list):
                return

            iterations = data.get("total_iterations", 0)
            if iterations <= 0:
                return

            timestamp = now_utc().strftime("%Y%m%d-%H%M%S")
            archive_name = self.repo_root / ".loom" / f"daemon-metrics-{timestamp}.json"
            shutil.copy(self.metrics_file, archive_name)
            self.log(f"Archived previous metrics to: {archive_name}")

            # Prune old archives (keep last 10)
            archives = sorted(
                self.repo_root.glob(".loom/daemon-metrics-*.json"),
                reverse=True,
            )
            for archive in archives[10:]:
                try:
                    archive.unlink()
                except Exception:
                    pass
        except Exception:
            pass

    def print_header(self) -> None:
        """Print startup header."""
        mode_display = "Normal"
        if self.config.force_mode and self.config.debug_mode:
            mode_display = "Force + Debug"
        elif self.config.force_mode:
            mode_display = "Force"
        elif self.config.debug_mode:
            mode_display = "Debug"

        self.log("")
        self.log("=" * 67)
        self.log("  LOOM DAEMON - PYTHON IMPLEMENTATION")
        self.log("=" * 67)
        self.log(f"  Started: {now_utc().isoformat().replace('+00:00', 'Z')}")
        self.log(f"  PID: {os.getpid()}")
        self.log(f"  Session ID: {self.session_id}")
        self.log(f"  Mode: {mode_display}")
        self.log(f"  Poll interval: {self.config.poll_interval}s")
        self.log(f"  Iteration timeout: {self.config.iteration_timeout}s")
        self.log(
            f"  Max backoff: {self.config.max_backoff}s "
            f"(after {self.config.backoff_threshold} failures, "
            f"{self.config.backoff_multiplier}x multiplier)"
        )
        self.log(f"  PID file: {self.pid_file}")
        self.log(f"  Metrics file: {self.metrics_file}")
        self.log(f"  Stop signal: {self.stop_signal}")
        self.log("=" * 67)
        self.log("")

    def run(self) -> int:
        """Run the main daemon loop. Returns exit code."""
        # Check for existing daemon instance
        if self.pid_file.exists():
            try:
                existing_pid = int(self.pid_file.read_text().strip())
                # Check if process is running
                os.kill(existing_pid, 0)
                print(f"Error: Daemon loop already running (PID: {existing_pid})")
                print("Use --status to check status or stop the existing daemon first")
                return 1
            except ProcessLookupError:
                print("Removing stale PID file")
                self.pid_file.unlink()
            except ValueError:
                self.pid_file.unlink()

        # Write PID file
        self.pid_file.write_text(str(os.getpid()))

        # Check for claude CLI
        if not shutil.which("claude"):
            print("Error: 'claude' CLI not found in PATH")
            print("Install Claude Code CLI: https://claude.ai/code")
            self.pid_file.unlink(missing_ok=True)
            return 1

        # Setup signal handlers
        def signal_handler(signum: int, frame: Any) -> None:
            self.running = False

        signal.signal(signal.SIGINT, signal_handler)
        signal.signal(signal.SIGTERM, signal_handler)

        try:
            # Rotate existing state file
            self.rotate_state_file()

            # Archive existing metrics
            self.archive_metrics_file()

            # Create log directory if needed
            self.log_file.parent.mkdir(parents=True, exist_ok=True)

            # Print header
            self.print_header()

            # Initialize files
            self.init_metrics_file()
            self.log(f"Metrics file initialized: {self.metrics_file}")

            self.init_state_file()
            if self.config.force_mode:
                self.log("Force mode enabled - stored in daemon-state.json")
            self.log(f"Session ID: {self.session_id}")

            # Clear any existing stop signal
            self.stop_signal.unlink(missing_ok=True)

            # Main loop
            while self.running:
                self.iteration += 1

                # Check for stop signal
                if self.check_stop_signal():
                    self.log(f"Iteration {self.iteration}: SHUTDOWN_SIGNAL detected")
                    break

                # Validate session ownership
                if not self.validate_session_ownership():
                    try:
                        data = read_json_file(self.state_file)
                        file_session_id = data.get("daemon_session_id", "unknown") if isinstance(data, dict) else "unknown"
                    except Exception:
                        file_session_id = "unknown"
                    self.log("SESSION CONFLICT: Another daemon has taken over the state file")
                    self.log(f"  Our session:    {self.session_id}")
                    self.log(f"  File session:   {file_session_id}")
                    self.log("  Yielding to the other daemon instance. Exiting.")
                    break

                # Run iteration
                self.log(f"Iteration {self.iteration}: Starting...")
                result = self.run_iteration()

                # Update metrics
                self.update_metrics(result.status, result.duration_seconds, result.summary)
                self.update_state_timing()
                self.check_slow_iteration(result.duration_seconds)

                # Collect health metrics
                self.collect_health_metrics()

                # Persist warnings
                self.persist_warnings(result.warn_codes)

                # Log and handle result
                if "SHUTDOWN" in result.summary:
                    self.log(f"Iteration {self.iteration}: {result.summary}")
                    break
                elif result.status in ("failure", "timeout"):
                    self.log(f"Iteration {self.iteration}: {result.summary} ({result.duration_seconds}s)")
                    self.update_backoff(success=False)
                else:
                    self.log(f"Iteration {self.iteration}: {result.summary} ({result.duration_seconds}s)")
                    pipeline_stalled = self.check_pipeline_stalled()
                    self.update_backoff(success=True, pipeline_stalled=pipeline_stalled)

                # Check for stop signal again before sleeping
                if self.check_stop_signal():
                    self.log("SHUTDOWN_SIGNAL detected after iteration")
                    break

                # Sleep before next iteration
                self.log(f"Sleeping {self.current_backoff}s until next iteration...")
                time.sleep(self.current_backoff)

            self.log("Daemon loop completed gracefully")
            return 0

        finally:
            self.cleanup()


def show_status(repo_root: pathlib.Path) -> int:
    """Show daemon status and exit."""
    pid_file = repo_root / PID_FILE
    state_file = repo_root / STATE_FILE

    if pid_file.exists():
        try:
            pid = int(pid_file.read_text().strip())
            os.kill(pid, 0)  # Check if process exists
            print(f"Daemon loop running (PID: {pid})")

            if state_file.exists():
                data = read_json_file(state_file)
                if isinstance(data, dict):
                    session_id = data.get("daemon_session_id", "unknown")
                    print(f"  Session ID: {session_id}")
            return 0
        except ProcessLookupError:
            print("Daemon loop not running (stale PID file)")
            pid_file.unlink()
            return 1
        except ValueError:
            print("Daemon loop not running (invalid PID file)")
            pid_file.unlink()
            return 1

    print("Daemon loop not running")
    return 1


def show_health(repo_root: pathlib.Path) -> int:
    """Show daemon health status and exit."""
    metrics_file = repo_root / METRICS_FILE
    pid_file = repo_root / PID_FILE

    if not metrics_file.exists():
        print("Daemon: not running (no metrics file)")
        return 1

    # Check running status
    running_status = "stopped"
    if pid_file.exists():
        try:
            pid = int(pid_file.read_text().strip())
            os.kill(pid, 0)
            running_status = f"running (PID: {pid})"
        except (ProcessLookupError, ValueError):
            pass

    # Load metrics
    data = read_json_file(metrics_file)
    if isinstance(data, list):
        print("Daemon: metrics file invalid")
        return 1

    health = data.get("health", {})
    health_status = health.get("status", "unknown")
    total_iterations = data.get("total_iterations", 0)
    consecutive_failures = health.get("consecutive_failures", 0)
    avg_duration = data.get("average_iteration_seconds", 0)
    last_iteration = data.get("last_iteration", {})
    last_status = last_iteration.get("status", "none") if last_iteration else "none"
    last_duration = last_iteration.get("duration_seconds", 0) if last_iteration else 0

    # Calculate success rate
    if total_iterations > 0:
        successful = data.get("successful_iterations", 0)
        success_rate = (successful * 100) // total_iterations
    else:
        success_rate = "n/a"

    # Format health display
    health_display = health_status
    if health_status == "unhealthy":
        health_display = f"{health_status} ({consecutive_failures} consecutive failures)"

    print(f"Daemon: {running_status}")
    print(f"Health: {health_display}")
    print(f"Iterations: {total_iterations} ({success_rate}% success)")
    print(f"Avg duration: {avg_duration}s")
    print(f"Last iteration: {last_status} ({last_duration}s)")

    # Show health monitoring metrics if available
    health_metrics = repo_root / ".loom" / "health-metrics.json"
    if health_metrics.exists():
        hm_data = read_json_file(health_metrics)
        if isinstance(hm_data, dict):
            health_score = hm_data.get("health_score", "?")
            health_monitor_status = hm_data.get("health_status", "?")
            print(f"Health score: {health_score}/100 ({health_monitor_status})")

    # Show unacknowledged alerts
    alerts_file = repo_root / ".loom" / "alerts.json"
    if alerts_file.exists():
        alerts_data = read_json_file(alerts_file)
        if isinstance(alerts_data, dict):
            alerts = alerts_data.get("alerts", [])
            unack = [a for a in alerts if not a.get("acknowledged", False)]
            if unack:
                print(f"Alerts: {len(unack)} unacknowledged")

    if health_status == "unhealthy":
        return 2
    return 0


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the daemon loop CLI."""
    parser = argparse.ArgumentParser(
        description="Loom Daemon Loop - continuous orchestration",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Environment Variables:
    LOOM_POLL_INTERVAL         Seconds between iterations (default: 120)
    LOOM_ITERATION_TIMEOUT     Max seconds per iteration (default: 300)
    LOOM_MAX_BACKOFF           Maximum backoff interval in seconds (default: 1800)
    LOOM_BACKOFF_MULTIPLIER    Backoff multiplier on failure (default: 2)
    LOOM_BACKOFF_THRESHOLD     Failures before backoff kicks in (default: 3)

To stop the daemon gracefully:
    touch .loom/stop-daemon
""",
    )
    parser.add_argument(
        "--force", "-f",
        action="store_true",
        help="Enable force mode for aggressive autonomous development",
    )
    parser.add_argument(
        "--debug", "-d",
        action="store_true",
        help="Enable debug mode for verbose subagent troubleshooting",
    )
    parser.add_argument(
        "--status",
        action="store_true",
        help="Check if daemon loop is running",
    )
    parser.add_argument(
        "--health",
        action="store_true",
        help="Show daemon health status and exit",
    )

    args = parser.parse_args(argv)

    # Find repo root
    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        print("Error: .loom directory not found")
        print("Run this command from a Loom-enabled repository root")
        return 1

    # Handle status/health flags
    if args.status:
        return show_status(repo_root)

    if args.health:
        return show_health(repo_root)

    # Create and run daemon
    config = DaemonConfig(
        force_mode=args.force,
        debug_mode=args.debug,
    )
    daemon = DaemonLoop(config, repo_root)
    return daemon.run()


if __name__ == "__main__":
    sys.exit(main())
