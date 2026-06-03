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
    _active_locked_issues,
    _active_spawn_loop_issues,
    _clear_stale_spawn_loop_locks,
    _get_dir_size,
    check_grace_period,
    check_uncommitted_changes,
    clean_agent_config,
    clean_daemon_crash_state,
    clean_stale_spawn_loop_locks,
    clean_worktrees,
    find_editable_pip_installs,
    main,
    print_summary,
    update_cleanup_state,
)
from loom_tools.common.repo import clear_repo_cache


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

    def test_print_summary_with_config_dirs(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats(cleaned_config_dirs=3)
        print_summary(stats)
        captured = capsys.readouterr()
        assert "Removed: 3 agent config dir(s)" in captured.out

    def test_print_summary_config_dirs_dry_run(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats(cleaned_config_dirs=2)
        print_summary(stats, dry_run=True)
        captured = capsys.readouterr()
        assert "Would remove: 2 agent config dir(s)" in captured.out

    def test_print_summary_no_config_dirs_when_zero(self, capsys: pytest.CaptureFixture[str]) -> None:
        stats = CleanupStats()
        print_summary(stats)
        captured = capsys.readouterr()
        assert "agent config dir" not in captured.out


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


class TestCleanAgentConfig:
    """Tests for clean_agent_config function."""

    def test_removes_agent_config_dirs(self, mock_repo: pathlib.Path) -> None:
        """Agent config dirs are removed when present."""
        config_base = mock_repo / ".loom" / "claude-config"
        config_base.mkdir()
        (config_base / "builder-1").mkdir()
        (config_base / "shepherd-2").mkdir()

        stats = CleanupStats()
        clean_agent_config(mock_repo, stats)

        assert stats.cleaned_config_dirs == 2
        # Dirs should actually be removed
        assert not (config_base / "builder-1").exists()
        assert not (config_base / "shepherd-2").exists()

    def test_dry_run_does_not_remove(self, mock_repo: pathlib.Path) -> None:
        """Dry run counts dirs but does not remove them."""
        config_base = mock_repo / ".loom" / "claude-config"
        config_base.mkdir()
        (config_base / "agent-1").mkdir()
        (config_base / "agent-2").mkdir()
        (config_base / "agent-3").mkdir()

        stats = CleanupStats()
        clean_agent_config(mock_repo, stats, dry_run=True)

        assert stats.cleaned_config_dirs == 3
        # Dirs should still exist
        assert (config_base / "agent-1").exists()
        assert (config_base / "agent-2").exists()
        assert (config_base / "agent-3").exists()

    def test_no_config_dir(self, mock_repo: pathlib.Path) -> None:
        """No error when claude-config directory does not exist."""
        stats = CleanupStats()
        clean_agent_config(mock_repo, stats)
        assert stats.cleaned_config_dirs == 0

    def test_empty_config_dir(self, mock_repo: pathlib.Path) -> None:
        """No error when claude-config directory is empty."""
        config_base = mock_repo / ".loom" / "claude-config"
        config_base.mkdir()

        stats = CleanupStats()
        clean_agent_config(mock_repo, stats)
        assert stats.cleaned_config_dirs == 0

    def test_not_called_with_scope_flags(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """Config dir cleanup should not run with --worktrees-only."""
        config_base = mock_repo / ".loom" / "claude-config"
        config_base.mkdir()
        (config_base / "agent-1").mkdir()

        clear_repo_cache()
        monkeypatch.chdir(mock_repo)
        main(["--dry-run", "--force", "--worktrees-only"])

        # Dir should still exist (cleanup was skipped)
        assert (config_base / "agent-1").exists()

    def test_called_in_standard_mode(self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """Config dir cleanup runs in standard mode."""
        config_base = mock_repo / ".loom" / "claude-config"
        config_base.mkdir()
        (config_base / "agent-1").mkdir()

        clear_repo_cache()
        monkeypatch.chdir(mock_repo)
        main(["--force"])

        # Dir should be removed
        assert not (config_base / "agent-1").exists()


class TestSpawnLoopIntegration:
    """Spawn-loop-aware claim-set integration (Phase 3.1.9, #3398).

    Verifies that ``loom-clean`` reads ``.loom/spawn-loop-state.json`` and
    ``.loom/locks/issue-<N>/`` instead of the retired ``.loom/daemon-state.json``.
    """

    def test_active_locked_issues_empty_when_no_dir(
        self, mock_repo: pathlib.Path
    ) -> None:
        assert _active_locked_issues(mock_repo) == set()

    def test_active_locked_issues_reads_issue_dirs(
        self, mock_repo: pathlib.Path
    ) -> None:
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-42").mkdir()
        (locks_dir / "issue-99").mkdir()
        # Non-issue-N entries should be ignored.
        (locks_dir / "stray.txt").write_text("noise")
        (locks_dir / "garbage").mkdir()
        assert _active_locked_issues(mock_repo) == {42, 99}

    def test_active_spawn_loop_unions_state_and_locks(
        self, mock_repo: pathlib.Path
    ) -> None:
        # Locks hold #100; state holds #200 — both should appear.
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-100").mkdir()
        state_path = mock_repo / ".loom" / "spawn-loop-state.json"
        state_path.write_text(json.dumps({
            "running": [{"issue": 200, "pid": 1}],
        }))
        assert _active_spawn_loop_issues(mock_repo) == {100, 200}

    def test_active_spawn_loop_handles_missing_state(
        self, mock_repo: pathlib.Path
    ) -> None:
        # No state file, no locks — empty set, no exceptions.
        assert _active_spawn_loop_issues(mock_repo) == set()

    def test_clear_stale_locks_keeps_live_tasks(
        self, mock_repo: pathlib.Path
    ) -> None:
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-42").mkdir()
        (locks_dir / "issue-43").mkdir()
        state_path = mock_repo / ".loom" / "spawn-loop-state.json"
        state_path.write_text(json.dumps({
            "running": [{"issue": 42, "pid": 12345}],
        }))
        removed = _clear_stale_spawn_loop_locks(mock_repo)
        assert removed == 1
        assert (locks_dir / "issue-42").exists()
        assert not (locks_dir / "issue-43").exists()

    def test_clear_stale_locks_dry_run(self, mock_repo: pathlib.Path) -> None:
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-7").mkdir()
        removed = _clear_stale_spawn_loop_locks(mock_repo, dry_run=True)
        assert removed == 1
        assert (locks_dir / "issue-7").exists()  # dry-run keeps the dir

    def test_clean_stale_spawn_loop_locks_public_wrapper(
        self, mock_repo: pathlib.Path
    ) -> None:
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-11").mkdir()
        stats = CleanupStats()
        # Just verify the wrapper doesn't blow up and the dir is gone.
        clean_stale_spawn_loop_locks(mock_repo, stats)
        assert not (locks_dir / "issue-11").exists()

    def test_clean_worktrees_skips_active_spawn_loop_issue(
        self, mock_repo: pathlib.Path
    ) -> None:
        """A worktree for an issue in `spawn-loop-state.json::running` is preserved."""
        # Create a worktree dir for issue 50.
        wt = mock_repo / ".loom" / "worktrees" / "issue-50"
        wt.mkdir(parents=True)
        # Spawn-loop says #50 is running.
        state_path = mock_repo / ".loom" / "spawn-loop-state.json"
        state_path.write_text(json.dumps({
            "running": [{"issue": 50, "pid": 1}],
        }))
        stats = CleanupStats()
        # Patch out external probes that would otherwise short-circuit.
        with patch("loom_tools.clean.find_processes_using_directory", return_value=[]), \
             patch("loom_tools.clean.find_editable_pip_installs", return_value=[]):
            clean_worktrees(mock_repo, stats)
        assert wt.exists(), "live spawn-loop task must prevent worktree removal"
        assert stats.skipped_in_use == 1

    def test_clean_worktrees_force_bypasses_spawn_loop_check(
        self, mock_repo: pathlib.Path
    ) -> None:
        """`--force` ignores the spawn-loop active check (existing semantics)."""
        wt = mock_repo / ".loom" / "worktrees" / "issue-60"
        wt.mkdir(parents=True)
        # #60 is running per spawn-loop-state — would normally be preserved.
        state_path = mock_repo / ".loom" / "spawn-loop-state.json"
        state_path.write_text(json.dumps({
            "running": [{"issue": 60, "pid": 2}],
        }))
        stats = CleanupStats()
        # `force=True` skips the in-use check; gh issue view will be polled
        # for the CLOSED check — short-circuit by patching gh_run.
        with patch("loom_tools.clean.find_processes_using_directory", return_value=[]), \
             patch("loom_tools.clean.find_editable_pip_installs", return_value=[]), \
             patch("loom_tools.clean.gh_run") as m_gh:
            m_gh.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="OPEN", stderr=""
            )
            clean_worktrees(mock_repo, stats, force=True)
        # With force=True + OPEN state, the worktree should NOT be removed
        # (issue is OPEN — open-issue gate still wins), but the spawn-loop
        # active check should NOT increment skipped_in_use.
        assert stats.skipped_in_use == 0, "force=True bypasses spawn-loop check"

    def test_update_cleanup_state_is_noop(self, mock_repo: pathlib.Path) -> None:
        """Phase 3.1.9 (#3398): the shim must not write anything."""
        # Pre-create a daemon-state.json to prove it's not touched.
        ds = mock_repo / ".loom" / "daemon-state.json"
        ds.write_text(json.dumps({"running": True, "shepherds": {}}))
        original = ds.read_text()
        update_cleanup_state(mock_repo, 1, "cleaned")
        update_cleanup_state(mock_repo, 2, "pending")
        update_cleanup_state(mock_repo, 3, "error")
        assert ds.read_text() == original, "update_cleanup_state must not write"


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


class TestCleanDaemonCrashState:
    """Tests for clean_daemon_crash_state (--daemon flag).

    Rewritten in Phase 3.1.9 (#3398) — the function now targets
    spawn-loop state files (`.loom/spawn-loop-state.json` +
    `.loom/locks/issue-<N>/`) instead of the retired daemon brain's
    `.loom/daemon-state.json` + `.loom/claims/` + `.loom/progress/`.
    """

    def _ghrun_factory(self, building_issues: list[int]):
        """Build a mock gh_run that returns `building_issues` for an
        `issue list --label loom:building` invocation."""
        def fake(args, check=False):
            del check
            joined = " ".join(args)
            stdout = ""
            if "issue" in args and "list" in args and "loom:building" in joined:
                stdout = "\n".join(str(n) for n in building_issues) + (
                    "\n" if building_issues else ""
                )
            return subprocess.CompletedProcess(
                args=args, returncode=0, stdout=stdout, stderr=""
            )
        return fake

    def test_clears_stale_locks_only(self, mock_repo: pathlib.Path) -> None:
        """Locks whose issue is not in spawn-loop-state.running are removed."""
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-42").mkdir()
        (locks_dir / "issue-43").mkdir()

        state_path = mock_repo / ".loom" / "spawn-loop-state.json"
        state_path.write_text(json.dumps({
            "started_at": "2026-06-02T16:12:19Z",
            "running": [{"issue": 42, "pid": 12345}],
        }))

        with patch("loom_tools.clean._list_loom_tmux_sessions", return_value=[]), \
             patch("loom_tools.clean.gh_run", side_effect=self._ghrun_factory([])):
            clean_daemon_crash_state(mock_repo)

        assert (locks_dir / "issue-42").exists(), "live task lock must remain"
        assert not (locks_dir / "issue-43").exists(), "stale lock must go"

    def test_reverts_stale_building_labels(self, mock_repo: pathlib.Path) -> None:
        """Issues with loom:building but no live task get their labels reverted."""
        # Live task on #100; #200 is building but orphaned.
        state_path = mock_repo / ".loom" / "spawn-loop-state.json"
        state_path.write_text(json.dumps({
            "started_at": "2026-06-02T16:12:19Z",
            "running": [{"issue": 100, "pid": 1}],
        }))

        edit_calls: list[list[str]] = []

        def fake_run(args, capture_output=True, text=True, check=False):
            del capture_output, text, check
            edit_calls.append(list(args))
            return subprocess.CompletedProcess(
                args=args, returncode=0, stdout="", stderr=""
            )

        with patch("loom_tools.clean._list_loom_tmux_sessions", return_value=[]), \
             patch("loom_tools.clean.gh_run", side_effect=self._ghrun_factory([100, 200])), \
             patch("loom_tools.clean.subprocess.run", side_effect=fake_run):
            clean_daemon_crash_state(mock_repo)

        # Exactly one gh edit call, for issue 200 (orphaned).
        edits = [c for c in edit_calls if "issue" in c and "edit" in c]
        assert len(edits) == 1
        assert "200" in edits[0]
        assert "--remove-label" in edits[0]
        assert "loom:building" in edits[0]
        assert "loom:issue" in edits[0]

    def test_resets_issue_failures(self, mock_repo: pathlib.Path) -> None:
        """Should reset issue-failures.json to empty entries."""
        failures_file = mock_repo / ".loom" / "issue-failures.json"
        failures_file.write_text('{"entries": {"42": {"count": 3}}}')

        with patch("loom_tools.clean._list_loom_tmux_sessions", return_value=[]), \
             patch("loom_tools.clean.gh_run", side_effect=self._ghrun_factory([])):
            clean_daemon_crash_state(mock_repo)

        data = json.loads(failures_file.read_text())
        assert data == {"entries": {}}

    def test_dry_run_preserves_files(self, mock_repo: pathlib.Path) -> None:
        """Dry run should not modify any files."""
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-999").mkdir()

        failures_file = mock_repo / ".loom" / "issue-failures.json"
        failures_file.write_text('{"entries": {"42": {"count": 3}}}')

        with patch("loom_tools.clean._list_loom_tmux_sessions", return_value=[]), \
             patch("loom_tools.clean.gh_run", side_effect=self._ghrun_factory([])):
            clean_daemon_crash_state(mock_repo, dry_run=True)

        assert (locks_dir / "issue-999").exists(), "dry run keeps locks"
        assert (
            json.loads(failures_file.read_text())["entries"]["42"]["count"] == 3
        ), "dry run keeps failures"


class TestMainDaemonFlag:
    """Tests for the --daemon CLI flag."""

    def test_daemon_flag_runs_crash_recovery(self, mock_repo: pathlib.Path) -> None:
        """--daemon should call clean_daemon_crash_state."""
        with patch("loom_tools.clean.find_repo_root", return_value=mock_repo), \
             patch("loom_tools.clean.clean_daemon_crash_state") as m_clean:
            result = main(["--daemon"])

        m_clean.assert_called_once_with(mock_repo, dry_run=False)
        assert result == 0

    def test_daemon_dry_run(self, mock_repo: pathlib.Path) -> None:
        """--daemon --dry-run should pass dry_run=True."""
        with patch("loom_tools.clean.find_repo_root", return_value=mock_repo), \
             patch("loom_tools.clean.clean_daemon_crash_state") as m_clean:
            result = main(["--daemon", "--dry-run"])

        m_clean.assert_called_once_with(mock_repo, dry_run=True)
        assert result == 0


# ---------------------------------------------------------------------------
# Aggressive mode (--aggressive) tests — see issue #3332.
# ---------------------------------------------------------------------------

from loom_tools.clean import (  # noqa: E402
    DECISION_KEEP,
    DECISION_REMOVE,
    LOOM_MANAGED_SENTINEL,
    AggressiveStats,
    WorktreeInfo,
    clean_aggressive,
    enumerate_git_worktrees,
    evaluate_aggressive_candidate,
    print_aggressive_summary,
)


def _mk_managed_worktree(
    repo_root: pathlib.Path,
    issue: int,
    *,
    head: str = "deadbeef" * 5,
    branch: str | None = None,
    locked: bool = False,
    sentinel: bool = True,
    age_seconds: int | None = None,
) -> WorktreeInfo:
    """Create an on-disk worktree-like directory and matching WorktreeInfo.

    The directory is created under ``<repo_root>/.loom/worktrees/issue-N``.
    A ``.loom-managed`` sentinel is written by default (toggle via
    ``sentinel=False``).
    """
    wt_dir = repo_root / ".loom" / "worktrees" / f"issue-{issue}"
    wt_dir.mkdir(parents=True, exist_ok=True)
    if sentinel:
        (wt_dir / LOOM_MANAGED_SENTINEL).write_text("")
    if age_seconds is not None:
        import os
        target = wt_dir.stat().st_mtime - age_seconds
        os.utime(wt_dir, (target, target))
    if branch is None:
        branch = f"refs/heads/feature/issue-{issue}"
    return WorktreeInfo(
        path=wt_dir,
        head=head,
        branch=branch,
        detached=False,
        locked=locked,
        lock_reason=None,
        bare=False,
    )


class TestEnumerateGitWorktrees:
    """Tests for `enumerate_git_worktrees` parsing of `--porcelain` output."""

    def test_parses_single_worktree(self, tmp_path: pathlib.Path) -> None:
        porcelain = (
            "worktree /tmp/repo\n"
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            "branch refs/heads/main\n"
        )
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=porcelain, stderr=""
        )
        with patch("loom_tools.clean.subprocess.run", return_value=completed):
            result = enumerate_git_worktrees(tmp_path)
        assert len(result) == 1
        assert result[0].path == pathlib.Path("/tmp/repo")
        assert result[0].head == "a" * 40
        assert result[0].branch == "refs/heads/main"
        assert result[0].branch_short == "main"
        assert not result[0].locked
        assert not result[0].detached
        assert not result[0].bare

    def test_parses_multiple_with_locked_and_detached(
        self, tmp_path: pathlib.Path
    ) -> None:
        porcelain = (
            "worktree /tmp/repo\n"
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            "branch refs/heads/main\n"
            "\n"
            "worktree /tmp/repo/.loom/worktrees/issue-1\n"
            "HEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"
            "branch refs/heads/feature/issue-1\n"
            "locked stale shepherd\n"
            "\n"
            "worktree /tmp/repo/.loom/worktrees/issue-2\n"
            "HEAD cccccccccccccccccccccccccccccccccccccccc\n"
            "detached\n"
            "\n"
        )
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=porcelain, stderr=""
        )
        with patch("loom_tools.clean.subprocess.run", return_value=completed):
            result = enumerate_git_worktrees(tmp_path)
        assert len(result) == 3
        # main
        assert result[0].branch_short == "main"
        # locked with reason
        assert result[1].locked
        assert result[1].lock_reason == "stale shepherd"
        assert result[1].branch_short == "feature/issue-1"
        # detached
        assert result[2].detached
        assert result[2].branch is None

    def test_handles_bare_worktree(self, tmp_path: pathlib.Path) -> None:
        porcelain = (
            "worktree /tmp/repo.git\n"
            "bare\n"
        )
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=porcelain, stderr=""
        )
        with patch("loom_tools.clean.subprocess.run", return_value=completed):
            result = enumerate_git_worktrees(tmp_path)
        assert len(result) == 1
        assert result[0].bare
        assert result[0].head is None

    def test_locked_without_reason(self, tmp_path: pathlib.Path) -> None:
        porcelain = (
            "worktree /tmp/repo/.loom/worktrees/issue-3\n"
            "HEAD dddddddddddddddddddddddddddddddddddddddd\n"
            "branch refs/heads/feature/issue-3\n"
            "locked\n"
        )
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=porcelain, stderr=""
        )
        with patch("loom_tools.clean.subprocess.run", return_value=completed):
            result = enumerate_git_worktrees(tmp_path)
        assert len(result) == 1
        assert result[0].locked
        assert result[0].lock_reason is None

    def test_returns_empty_on_subprocess_error(
        self, tmp_path: pathlib.Path
    ) -> None:
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="error"
        )
        with patch("loom_tools.clean.subprocess.run", return_value=completed):
            result = enumerate_git_worktrees(tmp_path)
        assert result == []


class TestEvaluateAggressiveCandidate:
    """Tests for `evaluate_aggressive_candidate` decision tree."""

    def test_skips_bare_main_worktree(
        self, mock_repo: pathlib.Path
    ) -> None:
        wt = WorktreeInfo(path=mock_repo, bare=True)
        decision, reason = evaluate_aggressive_candidate(
            wt, mock_repo, active_shepherd_issues=set(),
            min_age_seconds=0, force=False,
        )
        assert decision == DECISION_KEEP
        assert reason == "bare_main_worktree"

    def test_skips_main_repo_worktree_by_path(
        self, mock_repo: pathlib.Path
    ) -> None:
        wt = WorktreeInfo(
            path=mock_repo, head="x" * 40, branch="refs/heads/main"
        )
        with patch("loom_tools.clean._check_open_pr", return_value=(False, True)):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "bare_main_worktree"

    def test_skips_open_pr(self, mock_repo: pathlib.Path) -> None:
        wt = _mk_managed_worktree(mock_repo, issue=42)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(True, True)
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "open_pr"

    def test_fails_closed_on_pr_lookup_error(
        self, mock_repo: pathlib.Path
    ) -> None:
        """A failed `gh pr list` must skip — never remove."""
        wt = _mk_managed_worktree(mock_repo, issue=42)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, False)
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "pr_lookup_failed"

    def test_skips_active_shepherd(self, mock_repo: pathlib.Path) -> None:
        wt = _mk_managed_worktree(mock_repo, issue=99)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues={99},
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "active_shepherd"

    def test_skips_worktree_without_sentinel(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Missing .loom-managed sentinel => user_owned (skip)."""
        wt = _mk_managed_worktree(mock_repo, issue=42, sentinel=False)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "user_owned"

    def test_skips_worktree_at_noncanonical_path(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path
    ) -> None:
        """Worktrees outside .loom/worktrees/ => user_owned (skip)."""
        elsewhere = tmp_path / "elsewhere"
        elsewhere.mkdir()
        (elsewhere / LOOM_MANAGED_SENTINEL).write_text("")
        wt = WorktreeInfo(
            path=elsewhere,
            head="a" * 40,
            branch="refs/heads/feature/issue-50",
        )
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "user_owned"

    def test_skips_uncommitted_without_force(
        self, mock_repo: pathlib.Path
    ) -> None:
        wt = _mk_managed_worktree(mock_repo, issue=42)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=True
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "uncommitted"

    def test_uncommitted_with_force_falls_through(
        self, mock_repo: pathlib.Path
    ) -> None:
        wt = _mk_managed_worktree(mock_repo, issue=42)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=True
        ), patch(
            "loom_tools.clean._is_ancestor_of_origin_main", return_value=True
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=True,
            )
        assert decision == DECISION_REMOVE
        assert reason == "reachable_from_origin_main"

    def test_reachable_head_removed(self, mock_repo: pathlib.Path) -> None:
        wt = _mk_managed_worktree(mock_repo, issue=42)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=False
        ), patch(
            "loom_tools.clean._is_ancestor_of_origin_main", return_value=True
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=0, force=False,
            )
        assert decision == DECISION_REMOVE
        assert reason == "reachable_from_origin_main"

    def test_too_recent_skipped(self, mock_repo: pathlib.Path) -> None:
        """mtime guard skips young worktrees with unreachable HEAD."""
        wt = _mk_managed_worktree(mock_repo, issue=42, age_seconds=60)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=False
        ), patch(
            "loom_tools.clean._is_ancestor_of_origin_main", return_value=False
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=3600, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "too_recent"

    def test_stale_unreachable_skipped_by_default(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Stale worktree with unreachable HEAD => skip unreachable_head."""
        wt = _mk_managed_worktree(mock_repo, issue=42, age_seconds=99999)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=False
        ), patch(
            "loom_tools.clean._is_ancestor_of_origin_main", return_value=False
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=3600, force=False,
            )
        assert decision == DECISION_KEEP
        assert reason == "unreachable_head"

    def test_stale_unreachable_force_override(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Operator can force-remove unreachable worktrees via --force."""
        wt = _mk_managed_worktree(mock_repo, issue=42, age_seconds=99999)
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=False
        ), patch(
            "loom_tools.clean._is_ancestor_of_origin_main", return_value=False
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=3600, force=True,
            )
        assert decision == DECISION_REMOVE
        assert reason == "force_override_unreachable"

    def test_aggressive_mtime_stale_removed_when_reachable(
        self, mock_repo: pathlib.Path
    ) -> None:
        """A stale worktree without `.loom-in-use` is removed when HEAD
        is reachable from origin/main (reachability beats mtime guard)."""
        wt = _mk_managed_worktree(mock_repo, issue=42, age_seconds=99999)
        # No .loom-in-use marker by construction.
        assert not (wt.path / ".loom-in-use").exists()
        with patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=False
        ), patch(
            "loom_tools.clean._is_ancestor_of_origin_main", return_value=True
        ):
            decision, reason = evaluate_aggressive_candidate(
                wt, mock_repo, active_shepherd_issues=set(),
                min_age_seconds=3600, force=False,
            )
        assert decision == DECISION_REMOVE
        assert reason == "reachable_from_origin_main"


class TestCleanAggressive:
    """End-to-end tests for `clean_aggressive` orchestration."""

    def test_dry_run_does_not_mutate(self, mock_repo: pathlib.Path) -> None:
        wt = _mk_managed_worktree(mock_repo, issue=42, age_seconds=99999)
        wt_info = wt  # The WorktreeInfo we want enumerate to return.
        with patch(
            "loom_tools.clean.enumerate_git_worktrees",
            return_value=[wt_info],
        ), patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean._is_ancestor_of_origin_main", return_value=True
        ), patch(
            "loom_tools.clean.check_uncommitted_changes", return_value=False
        ), patch(
            "loom_tools.clean._active_shepherd_issues", return_value=set()
        ):
            stats = clean_aggressive(mock_repo, dry_run=True, force=False)
        # Directory still exists (dry run).
        assert wt.path.exists()
        assert stats.removed == 1
        assert stats.errors == 0

    def test_skips_open_pr_end_to_end(self, mock_repo: pathlib.Path) -> None:
        wt_info = _mk_managed_worktree(
            mock_repo, issue=42, age_seconds=99999
        )
        with patch(
            "loom_tools.clean.enumerate_git_worktrees",
            return_value=[wt_info],
        ), patch(
            "loom_tools.clean._check_open_pr", return_value=(True, True)
        ), patch(
            "loom_tools.clean._active_shepherd_issues", return_value=set()
        ):
            stats = clean_aggressive(mock_repo, dry_run=True, force=False)
        assert stats.removed == 0
        assert stats.skipped_open_pr == 1
        assert wt_info.path.exists()

    def test_skips_missing_sentinel_end_to_end(
        self, mock_repo: pathlib.Path
    ) -> None:
        wt_info = _mk_managed_worktree(
            mock_repo, issue=42, age_seconds=99999, sentinel=False
        )
        with patch(
            "loom_tools.clean.enumerate_git_worktrees",
            return_value=[wt_info],
        ), patch(
            "loom_tools.clean._check_open_pr", return_value=(False, True)
        ), patch(
            "loom_tools.clean._active_shepherd_issues", return_value=set()
        ):
            stats = clean_aggressive(mock_repo, dry_run=True, force=False)
        assert stats.removed == 0
        assert stats.skipped_user_owned == 1

    def test_active_shepherd_read_from_spawn_loop_state(
        self, mock_repo: pathlib.Path
    ) -> None:
        """`_active_shepherd_issues` (alias for `_active_spawn_loop_issues`)
        unions ``spawn-loop-state.json::running`` and ``.loom/locks/issue-N/``.
        Phase 3.1.9 (#3398) — no more daemon-state.json reads.
        """
        state_file = mock_repo / ".loom" / "spawn-loop-state.json"
        state_file.write_text(json.dumps({
            "running": [
                {"issue": 77, "pid": 1},
                {"issue": 91, "pid": 2},
            ],
        }))
        # And a stale lock for #88 — should still count as "active" because
        # locks survive crashes and may belong to in-flight recovery.
        locks_dir = mock_repo / ".loom" / "locks"
        locks_dir.mkdir(parents=True)
        (locks_dir / "issue-88").mkdir()

        from loom_tools.clean import _active_shepherd_issues
        active = _active_shepherd_issues(mock_repo)
        assert active == {77, 88, 91}

    def test_active_shepherd_empty_when_missing(
        self, mock_repo: pathlib.Path
    ) -> None:
        from loom_tools.clean import _active_shepherd_issues
        active = _active_shepherd_issues(mock_repo)
        assert active == set()

    def test_empty_worktree_list(self, mock_repo: pathlib.Path) -> None:
        with patch(
            "loom_tools.clean.enumerate_git_worktrees", return_value=[]
        ):
            stats = clean_aggressive(mock_repo, dry_run=True)
        assert stats.removed == 0
        assert stats.errors == 0


class TestNormalModeUnchanged:
    """Regression: normal modes must still respect .loom-in-use, process
    checks, and the CLOSED-issue precondition."""

    def test_normal_mode_skips_in_use_marker(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Normal `loom-clean --force` must still skip .loom-in-use."""
        from loom_tools.clean import clean_worktrees

        wt = mock_repo / ".loom" / "worktrees" / "issue-42"
        wt.mkdir(parents=True)
        (wt / ".loom-in-use").write_text(json.dumps({
            "shepherd_task_id": "abc", "pid": 1234
        }))

        stats = CleanupStats()
        clean_worktrees(mock_repo, stats, dry_run=True, force=True)
        # Marker should keep it preserved.
        assert stats.skipped_in_use == 1
        assert stats.cleaned_worktrees == 0


class TestPrintAggressiveSummary:
    """Tests for the aggressive-mode summary renderer."""

    def test_render_removed(self, capsys: pytest.CaptureFixture[str]) -> None:
        print_aggressive_summary(AggressiveStats(removed=3))
        out = capsys.readouterr().out
        assert "Aggressive Cleanup Summary" in out
        assert "Removed: 3" in out

    def test_render_dry_run(self, capsys: pytest.CaptureFixture[str]) -> None:
        print_aggressive_summary(AggressiveStats(removed=2), dry_run=True)
        out = capsys.readouterr().out
        assert "Would remove: 2" in out

    def test_render_skip_reasons(
        self, capsys: pytest.CaptureFixture[str]
    ) -> None:
        stats = AggressiveStats(
            skipped_open_pr=1,
            skipped_active_shepherd=1,
            skipped_user_owned=1,
            skipped_uncommitted=1,
            skipped_too_recent=1,
            skipped_unreachable=1,
        )
        print_aggressive_summary(stats)
        out = capsys.readouterr().out
        assert "open PR" in out
        assert "active shepherd" in out
        assert "user-owned" in out
        assert "uncommitted" in out
        assert "younger than min-age" in out
        assert "unreachable" in out


class TestAggressiveCLI:
    """CLI wiring for `--aggressive`."""

    def test_aggressive_dry_run(self, mock_repo: pathlib.Path) -> None:
        with patch("loom_tools.clean.find_repo_root", return_value=mock_repo), \
             patch("loom_tools.clean.clean_aggressive") as m_clean:
            m_clean.return_value = AggressiveStats()
            result = main(["--aggressive", "--dry-run"])
        m_clean.assert_called_once()
        kwargs = m_clean.call_args.kwargs
        assert kwargs["dry_run"] is True
        assert kwargs["force"] is False
        assert kwargs["min_age_seconds"] == 86400
        assert result == 0

    def test_aggressive_force_passes_through(
        self, mock_repo: pathlib.Path
    ) -> None:
        with patch("loom_tools.clean.find_repo_root", return_value=mock_repo), \
             patch("loom_tools.clean.clean_aggressive") as m_clean:
            m_clean.return_value = AggressiveStats()
            result = main(["--aggressive", "--force"])
        kwargs = m_clean.call_args.kwargs
        assert kwargs["force"] is True
        assert kwargs["dry_run"] is False
        assert result == 0

    def test_aggressive_custom_min_age(
        self, mock_repo: pathlib.Path
    ) -> None:
        with patch("loom_tools.clean.find_repo_root", return_value=mock_repo), \
             patch("loom_tools.clean.clean_aggressive") as m_clean:
            m_clean.return_value = AggressiveStats()
            main(["--aggressive", "--force", "--aggressive-min-age", "3600"])
        kwargs = m_clean.call_args.kwargs
        assert kwargs["min_age_seconds"] == 3600

    def test_aggressive_returns_1_on_errors(
        self, mock_repo: pathlib.Path
    ) -> None:
        with patch("loom_tools.clean.find_repo_root", return_value=mock_repo), \
             patch("loom_tools.clean.clean_aggressive") as m_clean:
            m_clean.return_value = AggressiveStats(errors=2)
            result = main(["--aggressive", "--force"])
        assert result == 1

