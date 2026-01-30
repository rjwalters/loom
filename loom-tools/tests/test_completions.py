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
