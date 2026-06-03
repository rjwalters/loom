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

        Python uses "loom-check-completions" as the source identifier vs legacy bash "check-completions.sh"
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


# ---------------------------------------------------------------------------
# Spawn-loop primary path tests (Phase 3.1.4, #3393)
# ---------------------------------------------------------------------------


class TestSpawnLoopTasks:
    """Tests for the spawn-loop task-driven detection path.

    When ``.loom/spawn-loop-state.json`` is present, the CLI iterates over
    ``tasks[]`` instead of ``daemon-state.json::shepherds``. The polling /
    silent-failure-detection algorithm itself is unchanged — only the source
    of output-file paths.
    """

    def test_spawn_loop_task_completed_via_output_file(self, tmp_path: pathlib.Path) -> None:
        """AGENT_EXIT_CODE=0 in output_file -> completed."""
        from loom_tools.completions import check_spawn_loop_tasks
        from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

        output_file = tmp_path / "sweep-issue-42.log"
        output_file.write_text("Some output\nAGENT_EXIT_CODE=0\nMore output")

        state = SpawnLoopState(
            started_at="2026-06-02T16:00:00Z",
            running=[
                SpawnLoopTask(
                    issue=42,
                    pid=12345,
                    started_at="2026-06-02T16:15:00Z",
                    token="robb-personal",
                    output_file=str(output_file),
                )
            ],
            present=True,
        )

        completed, errored, stale, running = check_spawn_loop_tasks(state)

        assert len(completed) == 1
        assert completed[0].id == "sweep-42"
        assert completed[0].issue == 42
        assert completed[0].category == "sweep"
        assert len(errored) == 0
        assert len(stale) == 0
        assert len(running) == 0

    def test_spawn_loop_task_errored_via_output_file(self, tmp_path: pathlib.Path) -> None:
        """AGENT_EXIT_CODE=1 in output_file -> errored."""
        from loom_tools.completions import check_spawn_loop_tasks
        from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

        output_file = tmp_path / "sweep-issue-99.log"
        output_file.write_text("crashed\nAGENT_EXIT_CODE=2\n")

        state = SpawnLoopState(
            running=[
                SpawnLoopTask(
                    issue=99,
                    pid=12345,
                    output_file=str(output_file),
                )
            ],
            present=True,
        )

        completed, errored, stale, running = check_spawn_loop_tasks(state)

        assert len(errored) == 1
        assert errored[0].id == "sweep-99"
        assert errored[0].reason == "exit_error"

    def test_spawn_loop_task_missing_output_file(self, tmp_path: pathlib.Path) -> None:
        """output_file path that doesn't exist -> errored with reason missing_output."""
        from loom_tools.completions import check_spawn_loop_tasks
        from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

        state = SpawnLoopState(
            running=[
                SpawnLoopTask(
                    issue=42,
                    pid=12345,
                    output_file=str(tmp_path / "does-not-exist.log"),
                )
            ],
            present=True,
        )

        completed, errored, stale, running = check_spawn_loop_tasks(state)

        assert len(errored) == 1
        assert errored[0].reason == "missing_output"

    def test_spawn_loop_task_no_output_file_field_treats_as_running(self) -> None:
        """Pre-#3393 state files lack output_file; treat such tasks as running."""
        from loom_tools.completions import check_spawn_loop_tasks
        from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

        state = SpawnLoopState(
            running=[
                SpawnLoopTask(issue=42, pid=12345, output_file=None),
            ],
            present=True,
        )

        completed, errored, stale, running = check_spawn_loop_tasks(state)

        assert len(running) == 1
        assert running[0].reason == "no_output_file_field"
        assert len(errored) == 0

    def test_spawn_loop_task_running_when_no_marker(self, tmp_path: pathlib.Path) -> None:
        """Fresh output file without AGENT_EXIT_CODE marker -> running."""
        from loom_tools.completions import check_spawn_loop_tasks
        from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

        output_file = tmp_path / "sweep-issue-7.log"
        output_file.write_text("still chugging along...\n")

        state = SpawnLoopState(
            running=[
                SpawnLoopTask(issue=7, pid=12345, output_file=str(output_file)),
            ],
            present=True,
        )

        completed, errored, stale, running = check_spawn_loop_tasks(state)

        assert len(running) == 1
        assert running[0].id == "sweep-7"
        assert len(completed) == 0
        assert len(errored) == 0

    def test_spawn_loop_task_stale_output_mtime(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Output file with very old mtime -> stale."""
        import os
        import time

        from loom_tools.completions import OUTPUT_STALE_THRESHOLD, check_spawn_loop_tasks
        from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

        output_file = tmp_path / "sweep-issue-5.log"
        output_file.write_text("output without exit code\n")

        # Backdate mtime past the staleness threshold.
        old_time = time.time() - OUTPUT_STALE_THRESHOLD - 120
        os.utime(output_file, (old_time, old_time))

        state = SpawnLoopState(
            running=[
                SpawnLoopTask(issue=5, pid=12345, output_file=str(output_file)),
            ],
            present=True,
        )

        completed, errored, stale, running = check_spawn_loop_tasks(state)

        assert len(stale) == 1
        assert stale[0].id == "sweep-5"
        assert "output_stale" in (stale[0].reason or "")

    def test_spawn_loop_task_stale_heartbeat(self) -> None:
        """Stale last_heartbeat field -> stale (forward-compat for #3392)."""
        from datetime import datetime, timedelta, timezone

        from loom_tools.completions import HEARTBEAT_STALE_THRESHOLD, check_spawn_loop_tasks
        from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

        stale_time = (
            datetime.now(timezone.utc) - timedelta(seconds=HEARTBEAT_STALE_THRESHOLD + 60)
        ).strftime("%Y-%m-%dT%H:%M:%SZ")

        state = SpawnLoopState(
            running=[
                SpawnLoopTask(
                    issue=11,
                    pid=12345,
                    last_heartbeat=stale_time,
                    output_file="/tmp/whatever.log",  # not reached
                ),
            ],
            present=True,
        )

        completed, errored, stale, running = check_spawn_loop_tasks(state)

        assert len(stale) == 1
        assert "heartbeat_stale" in (stale[0].reason or "")

    def test_run_check_prefers_spawn_loop_when_present(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """When spawn-loop-state.json is present, daemon-state.json is ignored."""
        from loom_tools.completions import run_check

        # Build a workspace with BOTH state files. The spawn-loop file should win.
        mock_repo = tmp_path / "repo"
        mock_repo.mkdir()
        (mock_repo / ".git").mkdir()
        (mock_repo / ".loom").mkdir()

        # Spawn-loop state with one completed sweep child.
        sweep_log = mock_repo / ".loom" / "logs" / "sweep-issue-42.log"
        sweep_log.parent.mkdir(parents=True)
        sweep_log.write_text("AGENT_EXIT_CODE=0\n")
        spawn_state = mock_repo / ".loom" / "spawn-loop-state.json"
        spawn_state.write_text(
            json.dumps(
                {
                    "started_at": "2026-06-02T16:00:00Z",
                    "running": [
                        {
                            "issue": 42,
                            "pid": 12345,
                            "started_at": "2026-06-02T16:15:00Z",
                            "token": "robb-personal",
                            "output_file": str(sweep_log),
                        }
                    ],
                }
            )
        )

        # Daemon state would normally surface a non-existent shepherd; if the
        # spawn-loop path is taken correctly, we never read this file's
        # shepherd entries and so no spurious "errored" is reported for it.
        daemon_state = mock_repo / ".loom" / "daemon-state.json"
        daemon_state.write_text(
            json.dumps(
                {
                    "started_at": "2026-06-02T16:00:00Z",
                    "running": True,
                    "iteration": 1,
                    "shepherds": {
                        "shepherd-1": {
                            "status": "working",
                            "issue": 999,
                            "task_id": "old",
                            "output_file": "/tmp/nonexistent-old-path.log",
                            "execution_mode": "direct",
                        }
                    },
                    "support_roles": {},
                }
            )
        )

        clear_repo_cache()
        monkeypatch.chdir(mock_repo)

        # Stub out gh_run so the orphan check returns no hits.
        from loom_tools import completions as completions_mod

        class _FakeResult:
            returncode = 0
            stdout = "[]"

        monkeypatch.setattr(
            completions_mod, "gh_run", lambda *a, **kw: _FakeResult()
        )

        report = run_check(verbose=False, recover=False, dry_run=False, json_output=True)

        # The sweep child is the only thing reported — and as completed.
        # The shepherd entry from daemon-state.json must NOT appear.
        ids = [t.id for t in report.completed + report.errored + report.stale + report.running]
        assert "sweep-42" in ids
        assert "shepherd-1" not in ids
        assert len(report.completed) == 1
        assert report.completed[0].category == "sweep"

    def test_run_check_falls_back_to_daemon_state_when_spawn_loop_absent(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """No spawn-loop-state.json -> legacy daemon-state.json path executes."""
        from loom_tools.completions import run_check

        mock_repo = tmp_path / "repo"
        mock_repo.mkdir()
        (mock_repo / ".git").mkdir()
        (mock_repo / ".loom").mkdir()
        (mock_repo / ".loom" / "progress").mkdir()

        # Only a daemon-state file — no spawn-loop file.
        daemon_state = mock_repo / ".loom" / "daemon-state.json"
        daemon_state.write_text(
            json.dumps(
                {
                    "started_at": "2026-06-02T16:00:00Z",
                    "running": True,
                    "iteration": 1,
                    "shepherds": {},
                    "support_roles": {},
                }
            )
        )

        clear_repo_cache()
        monkeypatch.chdir(mock_repo)

        from loom_tools import completions as completions_mod

        class _FakeResult:
            returncode = 0
            stdout = "[]"

        monkeypatch.setattr(
            completions_mod, "gh_run", lambda *a, **kw: _FakeResult()
        )

        report = run_check(verbose=False, recover=False, dry_run=False, json_output=True)

        # No shepherds, no support roles, no orphans -> clean report.
        assert not report.has_failures

    def test_run_check_spawn_loop_orphan_detection_uses_sweep_issue_set(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Orphaned issues == loom:building issues - sweep-tracked issues."""
        from loom_tools.completions import run_check

        mock_repo = tmp_path / "repo"
        mock_repo.mkdir()
        (mock_repo / ".git").mkdir()
        (mock_repo / ".loom").mkdir()

        # Sweep tracks issue 42; gh reports both 42 and 7 as loom:building.
        # 7 is the orphan.
        sweep_log = mock_repo / ".loom" / "logs" / "sweep-issue-42.log"
        sweep_log.parent.mkdir(parents=True)
        sweep_log.write_text("still running\n")

        spawn_state = mock_repo / ".loom" / "spawn-loop-state.json"
        spawn_state.write_text(
            json.dumps(
                {
                    "started_at": "2026-06-02T16:00:00Z",
                    "running": [
                        {
                            "issue": 42,
                            "pid": 12345,
                            "output_file": str(sweep_log),
                        }
                    ],
                }
            )
        )

        clear_repo_cache()
        monkeypatch.chdir(mock_repo)

        from loom_tools import completions as completions_mod

        class _FakeResult:
            returncode = 0
            stdout = json.dumps([{"number": 42}, {"number": 7}])

        monkeypatch.setattr(
            completions_mod, "gh_run", lambda *a, **kw: _FakeResult()
        )

        report = run_check(verbose=False, recover=False, dry_run=False, json_output=True)

        assert report.orphaned == [7]
        assert report.has_failures is True


class TestSpawnLoopSchema:
    """Confirm the SpawnLoopTask model exposes the output_file field added in #3393."""

    def test_spawn_loop_task_from_dict_populates_output_file(self) -> None:
        from loom_tools.models.spawn_loop_state import SpawnLoopTask

        task = SpawnLoopTask.from_dict(
            {
                "issue": 42,
                "pid": 12345,
                "started_at": "2026-06-02T16:00:00Z",
                "token": "robb-personal",
                "output_file": "/abs/path/to/sweep-issue-42.log",
            }
        )
        assert task.output_file == "/abs/path/to/sweep-issue-42.log"

    def test_spawn_loop_task_from_dict_tolerates_missing_output_file(self) -> None:
        from loom_tools.models.spawn_loop_state import SpawnLoopTask

        task = SpawnLoopTask.from_dict({"issue": 42, "pid": 12345})
        assert task.output_file is None

    def test_spawn_loop_task_to_dict_round_trips_output_file(self) -> None:
        from loom_tools.models.spawn_loop_state import SpawnLoopTask

        task = SpawnLoopTask(issue=42, pid=12345, output_file="/abs/log.log")
        d = task.to_dict()
        assert d["output_file"] == "/abs/log.log"

    def test_spawn_loop_task_to_dict_omits_none_output_file(self) -> None:
        from loom_tools.models.spawn_loop_state import SpawnLoopTask

        task = SpawnLoopTask(issue=42, pid=12345)
        d = task.to_dict()
        assert "output_file" not in d
