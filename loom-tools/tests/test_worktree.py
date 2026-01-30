"""Tests for loom_tools.worktree."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.worktree import (
    WorktreeResult,
    main,
)


class TestWorktreeResult:
    """Tests for WorktreeResult dataclass."""

    def test_to_dict_success(self) -> None:
        result = WorktreeResult(
            success=True,
            worktree_path="/path/to/worktree",
            branch_name="feature/issue-42",
            issue_number=42,
        )
        d = result.to_dict()

        assert d["success"] is True
        assert d["worktreePath"] == "/path/to/worktree"
        assert d["branchName"] == "feature/issue-42"
        assert d["issueNumber"] == 42

    def test_to_dict_failure(self) -> None:
        result = WorktreeResult(
            success=False,
            error="Something went wrong",
        )
        d = result.to_dict()

        assert d["success"] is False
        assert d["error"] == "Something went wrong"
        assert "worktreePath" not in d

    def test_to_dict_with_return_to(self) -> None:
        result = WorktreeResult(
            success=True,
            worktree_path="/path/to/worktree",
            branch_name="feature/issue-42",
            issue_number=42,
            return_to="/original/path",
        )
        d = result.to_dict()

        assert d["returnTo"] == "/original/path"


class TestCLI:
    """Tests for CLI main function."""

    def test_cli_help(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0

    def test_cli_no_args(self, capsys: pytest.CaptureFixture[str]) -> None:
        result = main([])
        assert result == 0
        captured = capsys.readouterr()
        assert "usage" in captured.out.lower() or "loom-worktree" in captured.out.lower()

    def test_cli_invalid_issue_number(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
        # Create minimal git repo
        (tmp_path / ".git").mkdir()
        monkeypatch.chdir(tmp_path)
        result = main(["not-a-number"])
        assert result == 1
        captured = capsys.readouterr()
        assert "must be numeric" in captured.err.lower() or "error" in captured.err.lower()

    def test_cli_invalid_issue_number_json(self, capsys: pytest.CaptureFixture[str]) -> None:
        result = main(["--json", "not-a-number"])
        assert result == 1
        captured = capsys.readouterr()
        data = json.loads(captured.out)
        assert data["success"] is False
        assert "error" in data

    def test_cli_invalid_return_to(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
        # Create minimal git repo
        (tmp_path / ".git").mkdir()
        monkeypatch.chdir(tmp_path)
        result = main(["--return-to", "/nonexistent/path", "42"])
        assert result == 1
        captured = capsys.readouterr()
        assert "does not exist" in captured.err.lower() or "error" in captured.err.lower()

    def test_cli_check_not_in_worktree(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
        # Create a simple git repo
        (tmp_path / ".git").mkdir()
        monkeypatch.chdir(tmp_path)

        result = main(["--check"])
        captured = capsys.readouterr()

        # Should indicate not in a worktree
        assert result == 1 or "not" in captured.out.lower()
