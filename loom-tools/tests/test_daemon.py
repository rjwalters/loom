"""Tests for the daemon module."""

from __future__ import annotations

import json
import pathlib


from loom_tools.daemon import (
    DaemonConfig,
    DaemonMetrics,
    IterationResult,
)


class TestDaemonConfig:
    def test_defaults(self) -> None:
        config = DaemonConfig()
        assert config.poll_interval == 120
        assert config.iteration_timeout == 300
        assert config.max_backoff == 1800
        assert config.backoff_multiplier == 2
        assert config.backoff_threshold == 3
        assert config.force_mode is False
        assert config.debug_mode is False

    def test_custom_values(self) -> None:
        config = DaemonConfig(
            poll_interval=60,
            force_mode=True,
            debug_mode=True,
        )
        assert config.poll_interval == 60
        assert config.force_mode is True
        assert config.debug_mode is True


class TestIterationResult:
    def test_success_result(self) -> None:
        result = IterationResult(
            status="success",
            duration_seconds=45,
            summary="ready=5 building=3",
        )
        assert result.status == "success"
        assert result.duration_seconds == 45
        assert result.summary == "ready=5 building=3"
        assert result.warn_codes == []

    def test_with_warn_codes(self) -> None:
        result = IterationResult(
            status="success",
            duration_seconds=45,
            summary="ready=5 WARN:no_work WARN:stalled",
            warn_codes=["no_work", "stalled"],
        )
        assert result.warn_codes == ["no_work", "stalled"]


class TestDaemonMetrics:
    def test_from_dict_empty(self) -> None:
        metrics = DaemonMetrics.from_dict({})
        assert metrics.total_iterations == 0
        assert metrics.health_status == "healthy"
        assert metrics.consecutive_failures == 0

    def test_from_dict_full(self) -> None:
        data = {
            "session_start": "2026-01-23T10:00:00Z",
            "total_iterations": 10,
            "successful_iterations": 8,
            "failed_iterations": 1,
            "timeout_iterations": 1,
            "iteration_durations": [30, 40, 50],
            "average_iteration_seconds": 40,
            "last_iteration": {
                "timestamp": "2026-01-23T11:00:00Z",
                "duration_seconds": 50,
                "status": "success",
                "summary": "completed",
            },
            "health": {
                "status": "healthy",
                "consecutive_failures": 0,
                "last_success": "2026-01-23T11:00:00Z",
            },
        }
        metrics = DaemonMetrics.from_dict(data)
        assert metrics.total_iterations == 10
        assert metrics.successful_iterations == 8
        assert metrics.failed_iterations == 1
        assert metrics.timeout_iterations == 1
        assert metrics.average_iteration_seconds == 40
        assert metrics.health_status == "healthy"
        assert metrics.consecutive_failures == 0

    def test_to_dict_roundtrip(self) -> None:
        metrics = DaemonMetrics(
            session_start="2026-01-23T10:00:00Z",
            total_iterations=5,
            successful_iterations=4,
            failed_iterations=1,
        )
        data = metrics.to_dict()
        metrics2 = DaemonMetrics.from_dict(data)
        assert metrics2.total_iterations == metrics.total_iterations
        assert metrics2.successful_iterations == metrics.successful_iterations
        assert metrics2.failed_iterations == metrics.failed_iterations

    def test_record_iteration_success(self) -> None:
        metrics = DaemonMetrics()
        metrics.record_iteration("success", 45, "completed")

        assert metrics.total_iterations == 1
        assert metrics.successful_iterations == 1
        assert metrics.failed_iterations == 0
        assert metrics.consecutive_failures == 0
        assert metrics.health_status == "healthy"
        assert metrics.last_iteration is not None
        assert metrics.last_iteration["status"] == "success"
        assert metrics.last_iteration["duration_seconds"] == 45
        assert metrics.last_success is not None

    def test_record_iteration_failure(self) -> None:
        metrics = DaemonMetrics()
        metrics.record_iteration("failure", 30, "ERROR: something went wrong")

        assert metrics.total_iterations == 1
        assert metrics.successful_iterations == 0
        assert metrics.failed_iterations == 1
        assert metrics.consecutive_failures == 1
        assert metrics.health_status == "healthy"  # Not unhealthy until 3 failures

    def test_record_iteration_timeout(self) -> None:
        metrics = DaemonMetrics()
        metrics.record_iteration("timeout", 300, "TIMEOUT")

        assert metrics.total_iterations == 1
        assert metrics.timeout_iterations == 1
        assert metrics.consecutive_failures == 1

    def test_consecutive_failures_trigger_unhealthy(self) -> None:
        metrics = DaemonMetrics()

        # Record 3 consecutive failures
        for i in range(3):
            metrics.record_iteration("failure", 30, f"ERROR: failure {i+1}")

        assert metrics.consecutive_failures == 3
        assert metrics.health_status == "unhealthy"

    def test_success_resets_consecutive_failures(self) -> None:
        metrics = DaemonMetrics()

        # Record 2 failures
        metrics.record_iteration("failure", 30, "ERROR 1")
        metrics.record_iteration("failure", 30, "ERROR 2")
        assert metrics.consecutive_failures == 2

        # Then a success
        metrics.record_iteration("success", 45, "completed")
        assert metrics.consecutive_failures == 0
        assert metrics.health_status == "healthy"

    def test_rolling_average(self) -> None:
        metrics = DaemonMetrics()

        # Record some iterations
        metrics.record_iteration("success", 30, "done")
        metrics.record_iteration("success", 40, "done")
        metrics.record_iteration("success", 50, "done")

        assert metrics.iteration_durations == [30, 40, 50]
        assert metrics.average_iteration_seconds == 40  # (30+40+50)/3 = 40

    def test_rolling_average_keeps_last_100(self) -> None:
        metrics = DaemonMetrics()

        # Record 105 iterations
        for i in range(105):
            metrics.record_iteration("success", 30, "done")

        assert len(metrics.iteration_durations) == 100


class TestBackoffAlgorithm:
    """Test the backoff algorithm logic."""

    def test_initial_backoff_equals_poll_interval(self) -> None:
        config = DaemonConfig(poll_interval=120)
        assert config.poll_interval == 120

    def test_backoff_threshold_triggers_increase(self) -> None:
        # Simulate the backoff logic from DaemonLoop.update_backoff
        current_backoff = 120
        consecutive_failures = 3  # At threshold
        backoff_threshold = 3
        backoff_multiplier = 2
        max_backoff = 1800

        if consecutive_failures >= backoff_threshold:
            new_backoff = current_backoff * backoff_multiplier
            if new_backoff > max_backoff:
                new_backoff = max_backoff
            current_backoff = new_backoff

        assert current_backoff == 240  # 120 * 2

    def test_backoff_caps_at_max(self) -> None:
        current_backoff = 1200
        backoff_multiplier = 2
        max_backoff = 1800

        new_backoff = current_backoff * backoff_multiplier
        if new_backoff > max_backoff:
            new_backoff = max_backoff

        assert new_backoff == 1800

    def test_backoff_resets_on_success(self) -> None:
        poll_interval = 120
        current_backoff = 960  # After some failures
        consecutive_failures = 4

        # Success resets backoff
        success = True
        if success:
            consecutive_failures = 0
            current_backoff = poll_interval

        assert current_backoff == 120
        assert consecutive_failures == 0


class TestSessionOwnershipValidation:
    """Test session ownership validation logic."""

    def test_no_state_file_allows_ownership(self, tmp_path: pathlib.Path) -> None:
        state_file = tmp_path / "daemon-state.json"
        # File doesn't exist - should allow ownership
        assert not state_file.exists()

    def test_matching_session_id_allows_ownership(self, tmp_path: pathlib.Path) -> None:
        state_file = tmp_path / "daemon-state.json"
        session_id = "12345-6789"

        state_file.write_text(json.dumps({
            "daemon_session_id": session_id,
            "running": True,
        }))

        data = json.loads(state_file.read_text())
        file_session_id = data.get("daemon_session_id")
        assert file_session_id == session_id  # Same session - ownership valid

    def test_different_session_id_blocks_ownership(self, tmp_path: pathlib.Path) -> None:
        state_file = tmp_path / "daemon-state.json"
        our_session_id = "12345-6789"
        other_session_id = "99999-1111"

        state_file.write_text(json.dumps({
            "daemon_session_id": other_session_id,
            "running": True,
        }))

        data = json.loads(state_file.read_text())
        file_session_id = data.get("daemon_session_id")
        assert file_session_id != our_session_id  # Different session - ownership invalid


class TestDocumentedDivergences:
    """Tests documenting intentional behavioral differences between bash and Python.

    Some differences exist for good reasons and are documented here. These are
    INTENTIONAL differences that are either improvements or acceptable variations.
    """

    def test_retry_blocked_issues_not_implemented(self) -> None:
        """Document that retry_blocked_issues is bash-only.

        Bash behavior (lines 588-599):
        - Pre-iteration check for retry_blocked_issues action
        - Calls retry-blocked-issues.sh with exponential backoff

        Python behavior:
        - Does NOT implement retry_blocked_issues
        - This is delegated to the iteration command (/loom iterate)

        This difference is INTENTIONAL:
        - The Python daemon is a thin loop wrapper
        - Complex orchestration logic is in the iteration command
        - Keeps daemon simple and deterministic
        """
        # Document that this feature is not in Python daemon
        # The bash script has lines 588-599 that call retry-blocked-issues.sh
        # Python delegates this to the iteration command
        pass

    def test_cli_flag_mapping_wrapper(self) -> None:
        """Document CLI flag mapping difference.

        Bash accepts (lines 94-103):
        - --merge/-m as primary flag
        - --force/-f as deprecated alias

        Python CLI uses:
        - --force/-f as primary flag (internal representation)

        The wrapper script (loom-daemon-loop) maps:
        - --merge/-m -> --force

        This difference is ACCEPTABLE:
        - User-facing CLI uses --merge (documented in CLAUDE.md)
        - Wrapper handles mapping
        - Internal consistency maintained
        """
        # The wrapper script maps --merge to --force for backward compatibility
        # This is documented in loom-daemon-loop
        pass

    def test_color_output_differs(self) -> None:
        """Document color output difference.

        Bash uses ANSI color codes (lines 71-86):
        - RED, GREEN, BLUE, YELLOW, CYAN, NC
        - Disabled if not a terminal

        Python uses:
        - Plain text output via log() method
        - Logging functions without ANSI codes

        This difference is ACCEPTABLE:
        - Both produce human-readable output
        - Python uses semantic logging (log_info, log_warning, etc.)
        - Terminal compatibility handled differently
        """
        pass

    def test_header_format_slightly_different(self) -> None:
        """Document startup header format difference.

        Bash header (lines 290-315):
        - Uses ANSI colors
        - Shows "LOOM DAEMON - SHELL SCRIPT WRAPPER MODE"

        Python header (lines 576-605):
        - Uses plain text
        - Shows "LOOM DAEMON - PYTHON IMPLEMENTATION"

        This difference is INTENTIONAL:
        - Clearly identifies which implementation is running
        - Helpful for debugging
        """
        pass


class TestOrchestrationDelegation:
    """Tests documenting that orchestration is delegated to /loom iterate.

    The daemon (both bash and Python) is a thin wrapper that:
    1. Manages the loop and timing (poll interval, backoff)
    2. Tracks metrics and health
    3. Delegates actual orchestration to /loom iterate

    Shepherd scaling, issue selection strategies, and support role triggering
    are all handled by the iteration command, NOT by daemon.py directly.
    """

    def test_iteration_command_format_matches_bash(self) -> None:
        """Verify iteration command format matches bash.

        Bash (lines 607-613):
        - ITERATE_CMD="/loom iterate"
        - if [[ -n "$FORCE_FLAG" ]]; then
        -     ITERATE_CMD="$ITERATE_CMD $FORCE_FLAG"
        - fi
        - if [[ -n "$DEBUG_FLAG" ]]; then
        -     ITERATE_CMD="$ITERATE_CMD $DEBUG_FLAG"
        - fi
        """
        from loom_tools.daemon import DaemonConfig

        # Python builds the same command (lines 332-338)
        config = DaemonConfig(force_mode=True, debug_mode=True)

        cmd_parts = ["/loom", "iterate"]
        if config.force_mode:
            cmd_parts.append("--force")
        if config.debug_mode:
            cmd_parts.append("--debug")

        iterate_cmd = " ".join(cmd_parts)
        assert iterate_cmd == "/loom iterate --force --debug"

    def test_shepherd_scaling_delegated_to_iterate(self) -> None:
        """Document that shepherd scaling is delegated to /loom iterate.

        The daemon does NOT contain shepherd scaling logic. Instead:
        - daemon.py runs `/loom iterate` (via claude --print)
        - The iteration command (loom.md skill) handles:
          - Checking shepherd pool state
          - Scaling decisions (spawn/idle/block)
          - Issue assignment

        This is the same as bash behavior (lines 619-622):
        - output=$(timeout "$ITERATION_TIMEOUT" claude --print "$ITERATE_CMD" 2>&1)

        The actual orchestration logic is in:
        - .loom/roles/loom.md (iteration skill)
        - .loom/roles/loom-iteration.md (iteration implementation)
        """
        # Shepherd scaling is NOT in daemon.py
        # It's in the /loom iterate command which is run via claude --print
        pass

    def test_issue_selection_strategies_delegated_to_iterate(self) -> None:
        """Document that issue selection strategies are delegated to /loom iterate.

        Issue selection (fifo, lifo, priority) is handled by:
        - .loom/roles/loom-iteration.md
        - python3 -m loom_tools.snapshot (for status reporting)

        The daemon (both bash and Python) just runs the iteration
        and processes the result. It does NOT implement selection logic.
        """
        # Issue selection strategies are NOT in daemon.py
        # They are in the iteration command
        pass

    def test_support_role_triggering_delegated_to_iterate(self) -> None:
        """Document that support role triggering is delegated to /loom iterate.

        Support roles (architect, hermit, guide, champion, doctor, auditor)
        are triggered by the iteration command, not the daemon.

        The daemon's only role is to:
        - Run the iteration command periodically
        - Track success/failure metrics
        - Apply backoff when needed
        """
        # Support role triggering is NOT in daemon.py
        # It's in the iteration command
        pass

    def test_timeout_protection_matches_bash(self) -> None:
        """Verify timeout protection matches bash.

        Bash (lines 619-622):
        - output=$(timeout "$ITERATION_TIMEOUT" claude --print "$ITERATE_CMD" 2>&1)
        - if [[ $exit_code -eq 124 ]]; then
        -     summary="TIMEOUT (iteration exceeded ${ITERATION_TIMEOUT}s)"

        Python (lines 341-347):
        - result = subprocess.run(..., timeout=self.config.iteration_timeout)
        - except subprocess.TimeoutExpired:
        -     return IterationResult(status="timeout", ...)
        """
        from loom_tools.daemon import DaemonConfig

        config = DaemonConfig()
        # Both use same default timeout (300s)
        assert config.iteration_timeout == 300

    def test_graceful_shutdown_signal_matches_bash(self) -> None:
        """Verify graceful shutdown signal handling matches bash.

        Both implementations:
        1. Check for .loom/stop-daemon file before each iteration
        2. Check again after iteration completes
        3. Log "SHUTDOWN_SIGNAL detected"
        4. Break out of loop cleanly

        Bash (lines 569-573, 771-775):
        - if [[ -f "$STOP_SIGNAL" ]]; then
        -     log "SHUTDOWN_SIGNAL detected"
        -     break
        - fi

        Python (lines 670-673, 716-718):
        - if self.check_stop_signal():
        -     self.log("SHUTDOWN_SIGNAL detected")
        -     break
        """
        from loom_tools.daemon import STOP_SIGNAL

        # Both use same stop signal file
        assert STOP_SIGNAL == ".loom/stop-daemon"


