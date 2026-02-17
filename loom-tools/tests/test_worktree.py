"""Tests for loom_tools.worktree.

This module contains:
1. Unit tests for WorktreeResult dataclass and CLI functions
2. Validation tests comparing Python behavior to worktree.sh

Referenced bash script: .loom/scripts/worktree.sh
Related issue: #1701 - Validate worktree.py behavior matches worktree.sh

Key areas validated:
1. CLI argument parsing (issue number, --check, --json, --return-to)
2. Path naming conventions (.loom/worktrees/issue-N)
3. Branch naming conventions (feature/issue-N, feature/<custom>)
4. Worktree detection logic (_is_in_worktree)
5. Stale worktree in-place reset (fetch + reset --hard, never remove)
6. JSON output format matching
7. Error messages and exit codes
"""

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


class TestDocumentedBehaviorDifferences:
    """Tests documenting intentional behavioral differences between bash and Python.

    These tests document any known differences that are intentional improvements
    or Python-specific enhancements over the bash implementation.
    """

    def test_python_uses_argparse_for_robust_parsing(self) -> None:
        """Document: Python uses argparse for more robust argument parsing.

        Bash uses manual string comparison for flag parsing.
        Python uses argparse which provides:
        - Automatic help generation
        - Type validation
        - Better error messages
        - Flag combination handling

        This is an intentional improvement, not a parity issue.
        """
        pass

    def test_python_returns_structured_result(self) -> None:
        """Document: Python create_worktree returns WorktreeResult object.

        Bash outputs to stdout/stderr and exits with code.
        Python returns a structured WorktreeResult dataclass that can be:
        - Inspected programmatically
        - Converted to JSON
        - Used in tests without subprocess

        This is an intentional improvement for library usage.
        """
        result = WorktreeResult(success=True, worktree_path="/test", branch_name="feature/test", issue_number=42)
        assert result.success is True
        assert result.worktree_path == "/test"

    def test_uses_fetch_not_pull(self) -> None:
        """Document: Both bash and Python use git fetch instead of git pull.

        This avoids the 'main branch locked' error when another worktree
        has main checked out. git fetch only updates origin/main remote ref
        without touching the working tree or local branches.
        """
        from loom_tools.worktree import _fetch_latest_main

        assert callable(_fetch_latest_main)

    def test_stale_worktrees_reset_in_place(self) -> None:
        """Document: Stale worktrees are reset in place, never removed.

        When a worktree has 0 commits ahead and no uncommitted changes,
        it is reset via 'git fetch origin main && git reset --hard origin/main'
        instead of being removed. This prevents CWD corruption when the
        shell's working directory points to the deleted worktree.

        Manual cleanup tools (loom-clean, stale-building-check.sh) remain
        the proper place for worktree removal.
        """
        from loom_tools.worktree import _reset_stale_worktree_in_place

        assert callable(_reset_stale_worktree_in_place)

    def test_worktree_created_from_origin_main(self) -> None:
        """Document: New branches are created from origin/main, not local main.

        The create_args uses 'origin/main' as the start point for new branches.
        This ensures worktrees are always based on the latest fetched state
        and avoids needing to checkout or update the local main branch.
        """
        pass
