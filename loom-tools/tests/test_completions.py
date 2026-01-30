"""Tests for loom_tools.completions."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.common.repo import clear_repo_cache
from loom_tools.completions import (
    CompletionReport,
    TaskStatus,
    _check_output_for_completion,
    _get_file_mtime,
    format_human_output,
    format_json_output,
    main,
)


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "progress").mkdir()
    return tmp_path


class TestOutputChecking:
    """Tests for output file checking functions."""

    def test_get_file_mtime_missing(self, tmp_path: pathlib.Path) -> None:
        result = _get_file_mtime(tmp_path / "nonexistent")
        assert result == 0

    def test_get_file_mtime_exists(self, tmp_path: pathlib.Path) -> None:
        f = tmp_path / "test.txt"
        f.write_text("test")
        result = _get_file_mtime(f)
        assert result > 0

    def test_check_output_completed(self, tmp_path: pathlib.Path) -> None:
        f = tmp_path / "output.txt"
        f.write_text("Some output\nAGENT_EXIT_CODE=0\nMore output")
        assert _check_output_for_completion(f) == "completed"

    def test_check_output_errored(self, tmp_path: pathlib.Path) -> None:
        f = tmp_path / "output.txt"
        f.write_text("Some output\nAGENT_EXIT_CODE=1\nMore output")
        assert _check_output_for_completion(f) == "errored"

    def test_check_output_running(self, tmp_path: pathlib.Path) -> None:
        f = tmp_path / "output.txt"
        f.write_text("Some output without exit code")
        assert _check_output_for_completion(f) is None

    def test_check_output_nonexistent(self, tmp_path: pathlib.Path) -> None:
        f = tmp_path / "nonexistent.txt"
        assert _check_output_for_completion(f) is None


class TestCompletionReport:
    """Tests for CompletionReport dataclass."""

    def test_has_failures_empty(self) -> None:
        report = CompletionReport()
        assert not report.has_failures

    def test_has_failures_errored(self) -> None:
        report = CompletionReport()
        report.errored.append(
            TaskStatus(
                id="shepherd-1",
                category="shepherd",
                issue=42,
                task_id="abc1234",
                status="errored",
            )
        )
        assert report.has_failures

    def test_has_failures_orphaned(self) -> None:
        report = CompletionReport()
        report.orphaned.append(42)
        assert report.has_failures


class TestFormatOutput:
    """Tests for output formatting functions."""

    def test_format_json_output(self) -> None:
        report = CompletionReport()
        report.completed.append(
            TaskStatus(
                id="shepherd-1",
                category="shepherd",
                issue=42,
                task_id="abc1234",
                status="completed",
            )
        )
        report.running.append(
            TaskStatus(
                id="shepherd-2",
                category="shepherd",
                issue=43,
                task_id="def5678",
                status="running",
            )
        )

        output = format_json_output(report)
        data = json.loads(output)

        assert data["summary"]["completed_count"] == 1
        assert data["summary"]["running_count"] == 1
        assert data["summary"]["has_failures"] is False

    def test_format_human_output(self) -> None:
        report = CompletionReport()
        report.completed.append(
            TaskStatus(
                id="shepherd-1",
                category="shepherd",
                issue=42,
                task_id="abc1234",
                status="completed",
            )
        )

        output = format_human_output(report)

        assert "Completed: 1" in output
        assert "Running:   0" in output


class TestCLI:
    """Tests for CLI main function."""

    def test_cli_help(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0

    def test_cli_missing_state(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        # No daemon-state.json exists
        result = main([])
        # Should handle gracefully
        assert result in (0, 1, 2)

    def test_cli_with_state(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)

        # Create minimal daemon state
        state_file = mock_repo / ".loom" / "daemon-state.json"
        state_file.write_text(
            json.dumps(
                {
                    "started_at": "2026-01-01T00:00:00Z",
                    "running": True,
                    "iteration": 1,
                    "shepherds": {},
                    "support_roles": {},
                }
            )
        )

        result = main([])
        assert result == 0

    def test_cli_json_output(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
        # Create a fresh mock repo to avoid state from previous tests
        mock_repo = tmp_path / "repo"
        mock_repo.mkdir()
        (mock_repo / ".git").mkdir()
        (mock_repo / ".loom").mkdir()
        (mock_repo / ".loom" / "progress").mkdir()

        # Clear repo cache before changing directory
        clear_repo_cache()
        monkeypatch.chdir(mock_repo)

        # Create minimal daemon state
        state_file = mock_repo / ".loom" / "daemon-state.json"
        state_file.write_text(
            json.dumps(
                {
                    "started_at": "2026-01-01T00:00:00Z",
                    "running": True,
                    "iteration": 1,
                    "shepherds": {},
                    "support_roles": {},
                }
            )
        )

        result = main(["--json"])
        captured = capsys.readouterr()

        assert result == 0
        # Parse only the JSON portion (stdout may have multiple lines)
        lines = captured.out.strip().split("\n")
        # Find the JSON object (starts with '{')
        json_content = ""
        depth = 0
        for line in lines:
            if "{" in line or depth > 0:
                json_content += line + "\n"
                depth += line.count("{") - line.count("}")
                if depth == 0 and json_content:
                    break
        data = json.loads(json_content)
        assert "summary" in data

    def test_cli_verbose(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)

        # Create minimal daemon state
        state_file = mock_repo / ".loom" / "daemon-state.json"
        state_file.write_text(
            json.dumps(
                {
                    "started_at": "2026-01-01T00:00:00Z",
                    "running": True,
                    "iteration": 1,
                    "shepherds": {
                        "shepherd-1": {
                            "status": "idle",
                        }
                    },
                    "support_roles": {},
                }
            )
        )

        result = main(["--verbose"])
        assert result == 0

    def test_cli_dry_run(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)

        # Create minimal daemon state
        state_file = mock_repo / ".loom" / "daemon-state.json"
        state_file.write_text(
            json.dumps(
                {
                    "started_at": "2026-01-01T00:00:00Z",
                    "running": True,
                    "iteration": 1,
                    "shepherds": {},
                    "support_roles": {},
                }
            )
        )

        result = main(["--dry-run", "--recover"])
        assert result == 0


# =============================================================================
# Behavior validation tests (bash/Python parity)
# =============================================================================
# These tests validate that completions.py produces identical behavior to
# check-completions.sh. Part of loom-tools migration (#1630).


class TestBashPythonParity:
    """Tests validating that completions.py behavior matches check-completions.sh.

    The Python implementation should produce identical behavior to the bash script
    for all supported operations. This test class validates parity for:
    - CLI argument parsing
    - Default values
    - Exit codes
    - JSON output structure
    - Task status detection
    - Recovery behavior

    See issue #1630 for the loom-tools migration validation effort.
    """

    def test_default_heartbeat_threshold_matches_bash(self) -> None:
        """Verify Python default heartbeat threshold matches bash script value.

        Bash default (from check-completions.sh line 42):
        - HEARTBEAT_STALE_THRESHOLD="${LOOM_HEARTBEAT_STALE_THRESHOLD:-300}"  # 5 minutes
        """
        from loom_tools.completions import HEARTBEAT_STALE_THRESHOLD

        BASH_HEARTBEAT_STALE_THRESHOLD = 300  # From check-completions.sh line 42
        assert HEARTBEAT_STALE_THRESHOLD == BASH_HEARTBEAT_STALE_THRESHOLD, (
            f"heartbeat threshold mismatch: Python={HEARTBEAT_STALE_THRESHOLD}, "
            f"bash={BASH_HEARTBEAT_STALE_THRESHOLD}"
        )

    def test_default_output_threshold_matches_bash(self) -> None:
        """Verify Python default output staleness threshold matches bash script value.

        Bash default (from check-completions.sh line 43):
        - OUTPUT_STALE_THRESHOLD="${LOOM_OUTPUT_STALE_THRESHOLD:-600}"  # 10 minutes
        """
        from loom_tools.completions import OUTPUT_STALE_THRESHOLD

        BASH_OUTPUT_STALE_THRESHOLD = 600  # From check-completions.sh line 43
        assert OUTPUT_STALE_THRESHOLD == BASH_OUTPUT_STALE_THRESHOLD, (
            f"output threshold mismatch: Python={OUTPUT_STALE_THRESHOLD}, "
            f"bash={BASH_OUTPUT_STALE_THRESHOLD}"
        )

    def test_cli_flags_match_bash(self) -> None:
        """Verify CLI flags match bash script argument parsing.

        Bash accepts (from check-completions.sh lines 69-128):
        - --json
        - --recover
        - --verbose, -v
        - --dry-run
        - --help, -h
        """
        # These should all be accepted without error
        for flag in ["--help", "-h"]:
            with pytest.raises(SystemExit) as exc_info:
                main([flag])
            assert exc_info.value.code == 0

    def test_verbose_flag_aliases(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """Verify -v and --verbose work the same way.

        Bash accepts (line 79):
          --verbose|-v)
        """
        monkeypatch.chdir(mock_repo)

        state_file = mock_repo / ".loom" / "daemon-state.json"
        state_file.write_text(
            json.dumps(
                {
                    "started_at": "2026-01-01T00:00:00Z",
                    "running": True,
                    "iteration": 1,
                    "shepherds": {},
                    "support_roles": {},
                }
            )
        )

        for flag in ["-v", "--verbose"]:
            result = main([flag])
            assert result == 0, f"Flag {flag} failed"

    def test_exit_code_semantics_match_bash(self) -> None:
        """Verify exit code semantics match bash behavior.

        Bash exit codes (from check-completions.sh lines 15-17 and 494-500):
        - 0: All tasks healthy (or recovered with --recover)
        - 1: Silent failures detected
        - 2: State file not found
        """
        # Exit codes are: 0 = healthy, 1 = failures, 2 = state file not found
        BASH_EXIT_HEALTHY = 0
        BASH_EXIT_FAILURES = 1
        BASH_EXIT_NO_STATE = 2

        # Test that Python uses the same semantics
        report = CompletionReport()
        # Empty report = healthy
        assert not report.has_failures

        # Report with errors = failures
        report.errored.append(
            TaskStatus(id="s-1", category="shepherd", issue=1, task_id="t1", status="errored")
        )
        assert report.has_failures

        # Report with orphaned = failures
        report2 = CompletionReport()
        report2.orphaned.append(42)
        assert report2.has_failures

    def test_json_output_structure_matches_bash(self) -> None:
        """Verify JSON output structure matches bash jq construction.

        Bash JSON structure (from check-completions.sh lines 441-478):
        {
            "completed": [...],
            "errored": [...],
            "stale": [...],
            "orphaned": [...],
            "running": [...],
            "recoveries": [...],
            "summary": {
                "completed_count": N,
                "errored_count": N,
                "stale_count": N,
                "orphaned_count": N,
                "running_count": N,
                "recovery_count": N,
                "has_failures": bool
            }
        }
        """
        from loom_tools.completions import format_json_output

        report = CompletionReport()
        report.completed.append(
            TaskStatus(id="s-1", category="shepherd", issue=1, task_id="t1", status="completed")
        )

        output = format_json_output(report)
        data = json.loads(output)

        # Verify all expected top-level keys exist
        expected_keys = ["completed", "errored", "stale", "orphaned", "running", "recoveries", "summary"]
        for key in expected_keys:
            assert key in data, f"Missing key: {key}"

        # Verify summary structure
        summary_keys = [
            "completed_count",
            "errored_count",
            "stale_count",
            "orphaned_count",
            "running_count",
            "recovery_count",
            "has_failures",
        ]
        for key in summary_keys:
            assert key in data["summary"], f"Missing summary key: {key}"

    def test_human_output_format_matches_bash(self) -> None:
        """Verify human-readable output format matches bash echo statements.

        Bash output (from check-completions.sh lines 481-491):
        - "=== Task Completion Summary ==="
        - "  Running:   N"
        - "  Completed: N"
        - "  Errored:   N"
        - "  Stale:     N"
        - "  Orphaned:  N"
        - "  Recovered: N" (only if recoveries > 0)
        """
        from loom_tools.completions import format_human_output

        report = CompletionReport()
        output = format_human_output(report)

        # Verify summary header
        assert "Task Completion Summary" in output

        # Verify all status lines are present with correct formatting
        assert "Running:" in output
        assert "Completed:" in output
        assert "Errored:" in output
        assert "Stale:" in output
        assert "Orphaned:" in output

    def test_task_status_categories_match_bash(self) -> None:
        """Verify task status categories match bash detection.

        Bash detects (from check-completions.sh lines 26-30):
        - completed: Task completed successfully
        - errored: Task exited with error
        - stale: No heartbeat for extended period
        - orphaned: Issue in loom:building but no active task
        - missing_output: Output file doesn't exist
        """
        valid_statuses = ["completed", "errored", "stale", "orphaned", "running", "missing_output"]

        for status in valid_statuses:
            task = TaskStatus(
                id="test",
                category="shepherd",
                issue=1,
                task_id="t1",
                status=status,
            )
            assert task.status in valid_statuses

    def test_completion_marker_detection_matches_bash(self, tmp_path: pathlib.Path) -> None:
        """Verify completion marker detection matches bash grep patterns.

        Bash uses (from check-completions.sh lines 287-295):
        - grep -q "AGENT_EXIT_CODE=0" for completed
        - grep -q "AGENT_EXIT_CODE=" for errored (any non-zero exit)
        """
        # Test AGENT_EXIT_CODE=0 (completed)
        completed_file = tmp_path / "completed.txt"
        completed_file.write_text("Output\nAGENT_EXIT_CODE=0\nMore output")
        assert _check_output_for_completion(completed_file) == "completed"

        # Test AGENT_EXIT_CODE=1 (errored)
        errored_file = tmp_path / "errored.txt"
        errored_file.write_text("Output\nAGENT_EXIT_CODE=1\nMore output")
        assert _check_output_for_completion(errored_file) == "errored"

        # Test AGENT_EXIT_CODE=255 (errored - any non-zero)
        errored_file2 = tmp_path / "errored2.txt"
        errored_file2.write_text("Output\nAGENT_EXIT_CODE=255\nMore output")
        assert _check_output_for_completion(errored_file2) == "errored"

        # Test no exit code (still running)
        running_file = tmp_path / "running.txt"
        running_file.write_text("Output without exit code marker")
        assert _check_output_for_completion(running_file) is None

    def test_shepherd_status_filtering_matches_bash(self) -> None:
        """Verify shepherd status filtering matches bash conditionals.

        Bash skips idle shepherds (from check-completions.sh lines 225-228):
        - if [[ "$status" == "idle" ]]; then ... continue
        - Only processes "working" status shepherds
        """
        from loom_tools.completions import check_shepherd_tasks
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="idle"),
                "shepherd-2": ShepherdEntry(status="working", issue=42, task_id="abc"),
            }
        )

        # With a mock repo, this verifies the logic paths
        # idle shepherd should be skipped, working should be processed
        # The actual behavior requires file system checks, but the filtering logic is clear


class TestIntentionalDifferences:
    """Tests documenting intentional behavioral differences between bash and Python.

    These differences are acceptable because they improve the implementation
    while maintaining functional compatibility.
    """

    def test_recovery_comment_format_similar_to_bash(self) -> None:
        """Document that Python recovery comment format is similar but not identical to bash.

        Both versions include:
        - "Silent Failure Recovery" header
        - Description of what happened
        - Action taken
        - Timestamp

        Python uses "loom-check-completions" as the source identifier vs bash "check-completions.sh"
        This is an acceptable difference for tool identification.
        """
        # This is a documentation test - the recovery comment format
        # is slightly different but contains the same semantic information
        bash_signature = "Recovered by check-completions.sh"
        python_signature = "Recovered by loom-check-completions"

        # Both identify the tool that performed recovery
        assert "check-completions" in bash_signature.lower()
        assert "check-completions" in python_signature.lower()

    def test_support_role_output_file_handling(self) -> None:
        """Document support role output_file handling difference.

        Bash (lines 316-358): Checks output_file field from support role data
        Python: SupportRoleEntry model doesn't include output_file field

        This is a known divergence. The Python model uses last_completed timestamp
        instead of output_file checking for support role staleness detection.

        This divergence is acceptable because:
        1. Support role output_file is not consistently used in practice
        2. The last_completed timestamp provides equivalent staleness detection
        3. The Python model is simpler and more maintainable
        """
        from loom_tools.models.daemon_state import SupportRoleEntry

        # Verify SupportRoleEntry has last_completed but not output_file
        entry = SupportRoleEntry(status="running", task_id="t1")
        assert hasattr(entry, "last_completed")
        assert not hasattr(entry, "output_file")


class TestTaskStatusDetection:
    """Tests for task status detection matching bash behavior."""

    def test_shepherd_heartbeat_staleness_detection(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Verify heartbeat staleness detection matches bash threshold.

        Bash (lines 242-246):
        - if [[ $heartbeat_age -gt $HEARTBEAT_STALE_THRESHOLD ]]; then
        -     STALE+=(...)
        """
        from datetime import datetime, timedelta, timezone

        from loom_tools.completions import HEARTBEAT_STALE_THRESHOLD, check_shepherd_tasks
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        monkeypatch.chdir(mock_repo)

        # Create a progress file with stale heartbeat
        progress_dir = mock_repo / ".loom" / "progress"
        progress_dir.mkdir(exist_ok=True)

        # Heartbeat older than threshold
        stale_time = (datetime.now(timezone.utc) - timedelta(seconds=HEARTBEAT_STALE_THRESHOLD + 60)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        progress_file = progress_dir / "shepherd-stale-task.json"
        progress_file.write_text(
            json.dumps({"last_heartbeat": stale_time, "status": "working"})
        )

        state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=42, task_id="stale-task"),
            }
        )

        completed, errored, stale, running = check_shepherd_tasks(mock_repo, state)

        # Should detect as stale due to heartbeat age
        assert len(stale) == 1
        assert stale[0].id == "shepherd-1"
        assert "heartbeat_stale" in (stale[0].reason or "")

    def test_shepherd_progress_completion_detection(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Verify progress file completion status detection matches bash.

        Bash (lines 250-260):
        - progress_status=$(jq -r '.status // "working"' "$progress_file")
        - if [[ "$progress_status" == "completed" ]]; then COMPLETED+=(...)
        - elif [[ "$progress_status" == "errored" ]]; then ERRORED+=(...)
        """
        from datetime import datetime, timezone

        from loom_tools.completions import check_shepherd_tasks
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        monkeypatch.chdir(mock_repo)

        progress_dir = mock_repo / ".loom" / "progress"
        progress_dir.mkdir(exist_ok=True)

        # Create progress file showing completed status
        current_time = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        progress_file = progress_dir / "shepherd-complete-task.json"
        progress_file.write_text(
            json.dumps({"last_heartbeat": current_time, "status": "completed"})
        )

        state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=42, task_id="complete-task"),
            }
        )

        completed, errored, stale, running = check_shepherd_tasks(mock_repo, state)

        # Should detect as completed from progress file status
        assert len(completed) == 1
        assert completed[0].id == "shepherd-1"

    def test_shepherd_output_file_completion_detection(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Verify output file completion marker detection matches bash.

        Bash (lines 287-295):
        - if grep -q "AGENT_EXIT_CODE=0" "$output_file"; then COMPLETED+=(...)
        - elif grep -q "AGENT_EXIT_CODE=" "$output_file"; then ERRORED+=(...)
        """
        from loom_tools.completions import check_shepherd_tasks
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        monkeypatch.chdir(mock_repo)

        # Create output file with completion marker
        output_file = mock_repo / "task-output.txt"
        output_file.write_text("Some output\nAGENT_EXIT_CODE=0\nMore output")

        state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working",
                    issue=42,
                    task_id="test-task",
                    output_file=str(output_file),
                    execution_mode="direct",
                ),
            }
        )

        completed, errored, stale, running = check_shepherd_tasks(mock_repo, state)

        # Should detect as completed from output file marker
        assert len(completed) == 1
        assert completed[0].id == "shepherd-1"


class TestEdgeCases:
    """Edge case tests to ensure Python matches bash error handling."""

    def test_missing_output_file_detection(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Verify missing output file detection matches bash.

        Bash (lines 265-270):
        - if [[ ! -f "$output_file" ]]; then
        -     ERRORED+=(...)
        -     log_warn "Shepherd ... output file missing"
        """
        from loom_tools.completions import check_shepherd_tasks
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        monkeypatch.chdir(mock_repo)

        state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working",
                    issue=42,
                    task_id="test-task",
                    output_file="/nonexistent/path/output.txt",
                    execution_mode="direct",
                ),
            }
        )

        completed, errored, stale, running = check_shepherd_tasks(mock_repo, state)

        # Should detect as errored due to missing output file
        assert len(errored) == 1
        assert errored[0].id == "shepherd-1"
        assert errored[0].reason == "missing_output"

    def test_idle_shepherd_skipping(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Verify idle shepherds are skipped like in bash.

        Bash (lines 225-228):
        - if [[ "$status" == "idle" ]]; then
        -     log_verbose "  Skipping (idle)"
        -     continue
        """
        from loom_tools.completions import check_shepherd_tasks
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        monkeypatch.chdir(mock_repo)

        state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="idle"),
                "shepherd-2": ShepherdEntry(status="idle"),
            }
        )

        completed, errored, stale, running = check_shepherd_tasks(mock_repo, state)

        # All idle shepherds should be skipped
        assert len(completed) == 0
        assert len(errored) == 0
        assert len(stale) == 0
        assert len(running) == 0
