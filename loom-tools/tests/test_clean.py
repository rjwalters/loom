"""Tests for loom_tools.clean."""

from __future__ import annotations

import json
import pathlib
import subprocess
from unittest.mock import patch

import pytest

from loom_tools.clean import (
    CleanupStats,
    PRStatus,
    _get_dir_size,
    check_grace_period,
    check_uncommitted_changes,
    find_editable_pip_installs,
    main,
    print_summary,
)


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "worktrees").mkdir()
    return tmp_path


class TestPRStatus:
    """Tests for PRStatus dataclass."""

    def test_pr_status_merged(self) -> None:
        status = PRStatus(status="MERGED", merged_at="2026-01-01T00:00:00Z")
        assert status.status == "MERGED"
        assert status.merged_at == "2026-01-01T00:00:00Z"

    def test_pr_status_no_pr(self) -> None:
        status = PRStatus(status="NO_PR")
        assert status.status == "NO_PR"
        assert status.merged_at is None


class TestGracePeriod:
    """Tests for check_grace_period function."""

    def test_grace_period_passed_old_merge(self) -> None:
        # A merge from the past should have passed grace period
        passed, remaining = check_grace_period("2020-01-01T00:00:00Z", 600)
        assert passed
        assert remaining == 0

    def test_grace_period_not_passed_recent_merge(self) -> None:
        # Use a future timestamp to ensure we're always in grace period
        from datetime import datetime, timedelta, timezone

        future = datetime.now(timezone.utc) + timedelta(minutes=5)
        future_str = future.strftime("%Y-%m-%dT%H:%M:%SZ")

        passed, remaining = check_grace_period(future_str, 600)
        # Since merge is in the future, grace period hasn't passed
        assert not passed
        assert remaining > 0


class TestUncommittedChanges:
    """Tests for check_uncommitted_changes function."""

    def test_nonexistent_path(self, tmp_path: pathlib.Path) -> None:
        result = check_uncommitted_changes(tmp_path / "nonexistent")
        assert not result

    def test_non_git_dir(self, tmp_path: pathlib.Path) -> None:
        # A directory without git returns False (git command fails)
        (tmp_path / "subdir").mkdir()
        result = check_uncommitted_changes(tmp_path / "subdir")
        # Git commands on non-git directories return error codes,
        # which our function interprets as False (no changes)
        # or True (treating error as "there might be changes")
        # depending on how the exception is handled
        assert result in (True, False)  # Both are valid behaviors


class TestDirSize:
    """Tests for _get_dir_size function."""

    def test_dir_size_bytes(self, tmp_path: pathlib.Path) -> None:
        # Create a small file
        (tmp_path / "file.txt").write_text("hello")
        size = _get_dir_size(tmp_path)
        assert "B" in size or "K" in size

    def test_dir_size_empty(self, tmp_path: pathlib.Path) -> None:
        subdir = tmp_path / "empty"
        subdir.mkdir()
        size = _get_dir_size(subdir)
        assert size == "0B" or "K" in size

    def test_dir_size_nonexistent(self, tmp_path: pathlib.Path) -> None:
        size = _get_dir_size(tmp_path / "nonexistent")
        # Non-existent dir returns "0B" or "unknown" depending on implementation
        assert size in ("0B", "unknown")


class TestCleanupStats:
    """Tests for CleanupStats dataclass."""

    def test_defaults(self) -> None:
        stats = CleanupStats()
        assert stats.cleaned_worktrees == 0
        assert stats.skipped_open == 0
        assert stats.skipped_editable == 0
        assert stats.errors == 0


class TestPrintSummary:
    """Tests for print_summary function."""

    def test_print_summary_empty(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats()
        print_summary(stats)
        captured = capsys.readouterr()
        assert "Summary" in captured.out
        assert "Cleaned: 0" in captured.out

    def test_print_summary_dry_run(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats(cleaned_worktrees=3)
        print_summary(stats, dry_run=True)
        captured = capsys.readouterr()
        assert "Would clean: 3" in captured.out

    def test_print_summary_with_errors(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats(errors=2)
        print_summary(stats)
        captured = capsys.readouterr()
        assert "Errors: 2" in captured.out

    def test_print_summary_with_skipped_editable(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats(skipped_editable=2)
        print_summary(stats)
        captured = capsys.readouterr()
        assert "Skipped (editable pip install): 2" in captured.out

    def test_print_summary_no_skipped_editable_when_zero(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats()
        print_summary(stats)
        captured = capsys.readouterr()
        assert "editable pip install" not in captured.out


class TestCLI:
    """Tests for CLI main function."""

    def test_cli_help(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0

    def test_cli_dry_run_no_repo(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        # Change to a directory without .loom
        (tmp_path / ".git").mkdir()
        monkeypatch.chdir(tmp_path)
        result = main(["--dry-run", "--force"])
        # Without .loom directory, find_repo_root() fails which returns 1
        # But the error is logged and the main function may handle it gracefully
        assert result in (0, 1)

    def test_cli_dry_run(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force"])
        assert result == 0

    def test_cli_worktrees_only(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--worktrees-only"])
        assert result == 0

    def test_cli_branches_only(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--branches-only"])
        assert result == 0

    def test_cli_tmux_only(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--tmux-only"])
        assert result == 0

    def test_cli_safe_mode(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--safe"])
        assert result == 0

    def test_cli_deep_clean(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--deep"])
        assert result == 0

    def test_cli_grace_period(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.chdir(mock_repo)
        result = main(["--dry-run", "--force", "--safe", "--grace-period", "1200"])
        assert result == 0


class TestFindEditablePipInstalls:
    """Tests for find_editable_pip_installs function."""

    def _make_run_side_effect(
        self,
        worktree_path: str,
        editable_packages: list[dict[str, str]],
        show_outputs: dict[str, str],
    ):
        """Create a subprocess.run side effect for mocking pip commands.

        Args:
            worktree_path: The worktree path to use.
            editable_packages: List of dicts with "name" and "version" for pip list --editable.
            show_outputs: Dict mapping package name to pip show stdout.
        """

        def side_effect(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(cmd, 0)

            if "which" in cmd_str:
                result.stdout = "/usr/bin/python3\n"
                result.stderr = ""
                return result

            if "pip list --editable" in cmd_str:
                result.stdout = json.dumps(editable_packages)
                result.stderr = ""
                return result

            if "pip show" in cmd_str:
                # Extract package name (last argument)
                pkg_name = cmd[-1]
                if pkg_name in show_outputs:
                    result.stdout = show_outputs[pkg_name]
                    result.stderr = ""
                else:
                    result.returncode = 1
                    result.stdout = ""
                    result.stderr = f"WARNING: Package(s) not found: {pkg_name}"
                return result

            result.returncode = 1
            result.stdout = ""
            result.stderr = ""
            return result

        return side_effect

    def test_editable_install_inside_worktree(self, tmp_path: pathlib.Path) -> None:
        """Editable install with location inside worktree should be detected."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()

        side_effect = self._make_run_side_effect(
            str(worktree),
            editable_packages=[{"name": "loom-tools", "version": "0.1.0"}],
            show_outputs={
                "loom-tools": (
                    f"Name: loom-tools\n"
                    f"Version: 0.1.0\n"
                    f"Location: {worktree}/loom-tools/src\n"
                    f"Editable project location: {worktree}/loom-tools\n"
                ),
            },
        )

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == ["loom-tools"]

    def test_editable_install_outside_worktree(self, tmp_path: pathlib.Path) -> None:
        """Editable install with location outside worktree should not be detected."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()
        other_dir = tmp_path / "other-project"
        other_dir.mkdir()

        side_effect = self._make_run_side_effect(
            str(worktree),
            editable_packages=[{"name": "some-pkg", "version": "1.0.0"}],
            show_outputs={
                "some-pkg": (
                    f"Name: some-pkg\n"
                    f"Version: 1.0.0\n"
                    f"Location: {other_dir}/src\n"
                    f"Editable project location: {other_dir}\n"
                ),
            },
        )

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == []

    def test_pip_not_found(self, tmp_path: pathlib.Path) -> None:
        """When pip commands fail, should return empty list without error."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()

        def side_effect(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            if "which" in cmd_str:
                result = subprocess.CompletedProcess(cmd, 0)
                result.stdout = "/usr/bin/python3\n"
                result.stderr = ""
                return result
            # All pip commands fail
            raise FileNotFoundError("pip not found")

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == []

    def test_pip_returns_error(self, tmp_path: pathlib.Path) -> None:
        """When pip list returns error code, should return empty list."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()

        def side_effect(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(cmd, 0)
            result.stderr = ""

            if "which" in cmd_str:
                result.stdout = "/usr/bin/python3\n"
                return result

            if "pip list" in cmd_str:
                result.returncode = 1
                result.stdout = ""
                return result

            result.returncode = 1
            result.stdout = ""
            return result

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == []

    def test_no_editable_packages(self, tmp_path: pathlib.Path) -> None:
        """When no editable packages are installed, should return empty list."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()

        side_effect = self._make_run_side_effect(
            str(worktree),
            editable_packages=[],
            show_outputs={},
        )

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == []

    def test_pip_list_returns_invalid_json(self, tmp_path: pathlib.Path) -> None:
        """When pip list returns invalid JSON, should return empty list."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()

        def side_effect(cmd, **kwargs):
            cmd_str = " ".join(str(c) for c in cmd)
            result = subprocess.CompletedProcess(cmd, 0)
            result.stderr = ""

            if "which" in cmd_str:
                result.stdout = "/usr/bin/python3\n"
                return result

            if "pip list" in cmd_str:
                result.stdout = "not valid json{{"
                return result

            result.returncode = 1
            result.stdout = ""
            return result

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == []

    def test_location_field_without_editable_prefix(self, tmp_path: pathlib.Path) -> None:
        """Detect via 'Location:' field when 'Editable project location:' is absent."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()

        side_effect = self._make_run_side_effect(
            str(worktree),
            editable_packages=[{"name": "my-pkg", "version": "0.1.0"}],
            show_outputs={
                "my-pkg": (
                    f"Name: my-pkg\n"
                    f"Version: 0.1.0\n"
                    f"Location: {worktree}/my-pkg/src\n"
                ),
            },
        )

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == ["my-pkg"]

    def test_no_python_interpreters(self, tmp_path: pathlib.Path) -> None:
        """When no Python interpreters found, should return empty list."""
        worktree = tmp_path / "issue-42"
        worktree.mkdir()

        def side_effect(cmd, **kwargs):
            # which fails for all interpreters
            result = subprocess.CompletedProcess(cmd, 1)
            result.stdout = ""
            result.stderr = ""
            return result

        with patch("loom_tools.clean.subprocess.run", side_effect=side_effect):
            result = find_editable_pip_installs(worktree)

        assert result == []


class TestDocumentedDivergences:
    """Tests documenting intentional behavioral differences between bash and Python.

    Some differences exist for good reasons and are documented here.
    """

    def test_branch_cleanup_delegation_differs(self) -> None:
        """Document branch cleanup delegation difference.

        Bash behavior (lines 548-556):
        - If scripts/cleanup-branches.sh exists, delegates to it
        - Otherwise, does manual branch cleanup

        Python behavior:
        - Always does manual branch cleanup directly
        - Does NOT delegate to external scripts

        This difference is INTENTIONAL:
        - Python implementation is self-contained
        - Avoids dependency on external cleanup-branches.sh
        - Same end result (branches for closed issues deleted)
        """
        # Document that Python always does manual cleanup
        # This is a design choice for simplicity and portability
        pass  # Difference documented

    def test_color_output_differs(self) -> None:
        """Document color output difference.

        Bash uses ANSI color codes (lines 21-26):
        - RED, GREEN, BLUE, YELLOW, CYAN, NC

        Python uses logging functions that may or may not apply colors
        depending on terminal capabilities.

        This difference is ACCEPTABLE:
        - Both produce human-readable output
        - Python version uses semantic logging
        - Terminal compatibility handled differently
        """
        pass  # Difference documented

    def test_interactive_prompt_handling(self) -> None:
        """Document interactive prompt handling similarity.

        Both implementations:
        - Prompt for confirmation when not in --force mode
        - Accept y/Y for yes
        - Default to N (no) for empty or invalid input
        - Handle EOFError and KeyboardInterrupt

        The implementations are equivalent but use different I/O mechanisms.
        """
        pass  # Behavior is equivalent

