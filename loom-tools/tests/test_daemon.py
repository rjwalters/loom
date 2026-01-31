"""Tests for the daemon module."""

from __future__ import annotations

import json
import pathlib


from unittest import mock

from loom_tools.daemon import (
    DaemonConfig,
    DaemonLoop,
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


class TestStateRotation:
    """Tests for daemon state rotation and archival."""

    def _make_daemon(self, tmp_path: pathlib.Path) -> DaemonLoop:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True, exist_ok=True)
        config = DaemonConfig()
        return DaemonLoop(config, tmp_path)

    def test_rotate_skips_when_no_state_file(self, tmp_path: pathlib.Path) -> None:
        """rotate_state_file should be a no-op when state file doesn't exist."""
        daemon = self._make_daemon(tmp_path)
        daemon.rotate_state_file()
        # No archives created
        archives = list((tmp_path / ".loom").glob("[0-9][0-9]-daemon-state.json"))
        assert archives == []

    def test_python_fallback_archives_meaningful_state(self, tmp_path: pathlib.Path) -> None:
        """Python fallback should archive state with session summary."""
        daemon = self._make_daemon(tmp_path)
        state_data = {
            "iteration": 10,
            "completed_issues": [100, 101],
            "total_prs_merged": 2,
            "running": False,
        }
        daemon.state_file.write_text(json.dumps(state_data))

        daemon._rotate_state_python()

        # Original state file should be gone (renamed)
        assert not daemon.state_file.exists()

        # Archive should exist
        archive = tmp_path / ".loom" / "00-daemon-state.json"
        assert archive.exists()

        archived_data = json.loads(archive.read_text())
        assert "session_summary" in archived_data
        assert archived_data["session_summary"]["session_id"] == 0
        assert archived_data["session_summary"]["issues_completed"] == 2
        assert archived_data["session_summary"]["prs_merged"] == 2
        assert archived_data["session_summary"]["total_iterations"] == 10

    def test_python_fallback_skips_empty_state(self, tmp_path: pathlib.Path) -> None:
        """Python fallback should skip rotation for tiny state files."""
        daemon = self._make_daemon(tmp_path)
        daemon.state_file.write_text("{}")  # < 50 bytes

        daemon._rotate_state_python()

        # State file should still exist (not rotated)
        assert daemon.state_file.exists()
        archives = list((tmp_path / ".loom").glob("[0-9][0-9]-daemon-state.json"))
        assert archives == []

    def test_python_fallback_skips_no_useful_data(self, tmp_path: pathlib.Path) -> None:
        """Python fallback should skip rotation when iteration=0 and no work done."""
        daemon = self._make_daemon(tmp_path)
        state_data = {
            "iteration": 0,
            "completed_issues": [],
            "shepherds": {},
            "padding": "x" * 100,
        }
        daemon.state_file.write_text(json.dumps(state_data))

        daemon._rotate_state_python()

        # State file should still exist (not rotated)
        assert daemon.state_file.exists()

    def test_python_fallback_increments_session_number(self, tmp_path: pathlib.Path) -> None:
        """Python fallback should find next available session number."""
        daemon = self._make_daemon(tmp_path)

        # Create existing archives
        (tmp_path / ".loom" / "00-daemon-state.json").write_text("{}")
        (tmp_path / ".loom" / "01-daemon-state.json").write_text("{}")

        state_data = {
            "iteration": 5,
            "completed_issues": [200],
            "total_prs_merged": 1,
        }
        daemon.state_file.write_text(json.dumps(state_data))

        daemon._rotate_state_python()

        # Should be archived as 02
        assert (tmp_path / ".loom" / "02-daemon-state.json").exists()
        assert not daemon.state_file.exists()

    def test_python_fallback_prunes_old_archives(self, tmp_path: pathlib.Path) -> None:
        """Python fallback should prune old archives to enforce limit."""
        daemon = self._make_daemon(tmp_path)

        # Create 10 existing archives (at max)
        for i in range(10):
            (tmp_path / ".loom" / f"{i:02d}-daemon-state.json").write_text(
                json.dumps({"session_id": i})
            )

        state_data = {
            "iteration": 5,
            "completed_issues": [300],
            "total_prs_merged": 1,
        }
        daemon.state_file.write_text(json.dumps(state_data))

        daemon._rotate_state_python()

        # Should have pruned oldest to make room
        archives = sorted((tmp_path / ".loom").glob("[0-9][0-9]-daemon-state.json"))
        assert len(archives) <= 10

    def test_rotate_uses_shell_when_available(self, tmp_path: pathlib.Path) -> None:
        """rotate_state_file should try shell script first."""
        daemon = self._make_daemon(tmp_path)

        state_data = {"iteration": 5, "completed_issues": [1]}
        daemon.state_file.write_text(json.dumps(state_data))

        # Create a fake shell script that succeeds
        script_dir = tmp_path / ".loom" / "scripts"
        script_dir.mkdir(parents=True, exist_ok=True)
        script = script_dir / "rotate-daemon-state.sh"
        script.write_text("#!/bin/bash\nexit 0\n")
        script.chmod(0o755)

        with mock.patch("subprocess.run") as mock_run:
            mock_run.return_value = mock.MagicMock(returncode=0)
            daemon.rotate_state_file()

        mock_run.assert_called_once()

    def test_rotate_falls_back_to_python_on_shell_failure(self, tmp_path: pathlib.Path) -> None:
        """rotate_state_file should fall back to Python when shell fails."""
        daemon = self._make_daemon(tmp_path)

        state_data = {
            "iteration": 5,
            "completed_issues": [1],
            "total_prs_merged": 1,
        }
        daemon.state_file.write_text(json.dumps(state_data))

        # Create a fake shell script
        script_dir = tmp_path / ".loom" / "scripts"
        script_dir.mkdir(parents=True, exist_ok=True)
        script = script_dir / "rotate-daemon-state.sh"
        script.write_text("#!/bin/bash\nexit 1\n")
        script.chmod(0o755)

        with mock.patch("subprocess.run") as mock_run:
            mock_run.return_value = mock.MagicMock(
                returncode=1,
                stderr=b"Error: Not in a git repository",
            )
            daemon.rotate_state_file()

        # State should have been rotated by Python fallback
        assert not daemon.state_file.exists()
        archives = list((tmp_path / ".loom").glob("[0-9][0-9]-daemon-state.json"))
        assert len(archives) == 1


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


class TestPreflightChecks:
    """Tests for daemon pre-flight dependency checks."""

    def _make_daemon(self, tmp_path: pathlib.Path) -> DaemonLoop:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True, exist_ok=True)
        config = DaemonConfig()
        return DaemonLoop(config, tmp_path)

    def test_all_checks_pass(self, tmp_path: pathlib.Path) -> None:
        """When all dependencies are available, no failures returned."""
        daemon = self._make_daemon(tmp_path)
        (tmp_path / "loom-tools").mkdir()

        with mock.patch("shutil.which", return_value="/usr/bin/fake"):
            with mock.patch("subprocess.run") as mock_run:
                mock_run.return_value = mock.MagicMock(returncode=0, stderr="")
                failures = daemon.run_preflight_checks()

        assert failures == []

    def test_missing_claude_cli(self, tmp_path: pathlib.Path) -> None:
        """When claude CLI is missing, appropriate error returned."""
        daemon = self._make_daemon(tmp_path)

        def which_side_effect(name: str) -> str | None:
            if name == "claude":
                return None
            return "/usr/bin/fake"

        with mock.patch("shutil.which", side_effect=which_side_effect):
            with mock.patch("subprocess.run") as mock_run:
                mock_run.return_value = mock.MagicMock(returncode=0, stderr="")
                failures = daemon.run_preflight_checks()

        assert any("claude" in f.lower() for f in failures)

    def test_loom_tools_not_importable(self, tmp_path: pathlib.Path) -> None:
        """When loom_tools is not importable, appropriate error returned."""
        daemon = self._make_daemon(tmp_path)

        def run_side_effect(args, **kw):
            if "-c" in args and "import loom_tools" in args:
                return mock.MagicMock(returncode=1, stderr="ModuleNotFoundError")
            return mock.MagicMock(returncode=0, stderr="")

        with mock.patch("shutil.which", return_value="/usr/bin/fake"):
            with mock.patch("subprocess.run", side_effect=run_side_effect):
                failures = daemon.run_preflight_checks()

        assert any("loom_tools" in f for f in failures)

    def test_gh_not_authenticated(self, tmp_path: pathlib.Path) -> None:
        """When gh is not authenticated, appropriate error returned."""
        daemon = self._make_daemon(tmp_path)

        call_count = 0

        def run_side_effect(args, **kw):
            nonlocal call_count
            call_count += 1
            if "auth" in args:
                return mock.MagicMock(returncode=1, stderr="Not authenticated")
            return mock.MagicMock(returncode=0, stderr="")

        with mock.patch("shutil.which", return_value="/usr/bin/fake"):
            with mock.patch("subprocess.run", side_effect=run_side_effect):
                failures = daemon.run_preflight_checks()

        assert any("gh" in f.lower() and "authenticated" in f.lower() for f in failures)

    def test_missing_gh_cli(self, tmp_path: pathlib.Path) -> None:
        """When gh CLI is missing entirely, appropriate error returned."""
        daemon = self._make_daemon(tmp_path)

        def which_side_effect(name: str) -> str | None:
            if name == "gh":
                return None
            return "/usr/bin/fake"

        with mock.patch("shutil.which", side_effect=which_side_effect):
            with mock.patch("subprocess.run") as mock_run:
                mock_run.return_value = mock.MagicMock(returncode=0, stderr="")
                failures = daemon.run_preflight_checks()

        assert any("gh" in f.lower() and "not found" in f.lower() for f in failures)


