"""Tests for ShepherdContext, focusing on _cleanup_stale_progress_for_issue."""

from __future__ import annotations

import json
import logging
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ExecutionMode, ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext, _HEARTBEAT_FRESH_THRESHOLD
from loom_tools.shepherd.errors import IssueBlockedError, IssueClosedError, IssueIsEpicError


def _make_context(tmp_path: Path, issue: int = 42, task_id: str = "abc1234") -> ShepherdContext:
    """Create a ShepherdContext with a tmp_path-based repo_root."""
    config = ShepherdConfig(issue=issue, task_id=task_id)
    (tmp_path / ".loom" / "progress").mkdir(parents=True, exist_ok=True)
    return ShepherdContext(config=config, repo_root=tmp_path)


def _write_progress(progress_dir: Path, filename: str, data: dict) -> Path:
    """Write a progress JSON file and return its path."""
    path = progress_dir / filename
    path.write_text(json.dumps(data))
    return path


class TestCleanupStaleProgressForIssue:
    """Tests for ShepherdContext._cleanup_stale_progress_for_issue()."""

    def test_removes_stale_file_same_issue_different_task(self, tmp_path: Path) -> None:
        """Stale progress file for same issue with different task_id is removed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        _write_progress(progress_dir, "shepherd-old1234.json", {"issue": 42, "task_id": "old1234"})

        _make_context(tmp_path, issue=42, task_id="new5678")

        assert not (progress_dir / "shepherd-old1234.json").exists()

    def test_preserves_file_for_different_issue(self, tmp_path: Path) -> None:
        """Progress file for a different issue is not removed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        _write_progress(progress_dir, "shepherd-other99.json", {"issue": 99, "task_id": "other99"})

        _make_context(tmp_path, issue=42, task_id="new5678")

        assert (progress_dir / "shepherd-other99.json").exists()

    def test_preserves_own_progress_file(self, tmp_path: Path) -> None:
        """Progress file with the same task_id is not removed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        _write_progress(progress_dir, "shepherd-abc1234.json", {"issue": 42, "task_id": "abc1234"})

        _make_context(tmp_path, issue=42, task_id="abc1234")

        assert (progress_dir / "shepherd-abc1234.json").exists()

    def test_handles_missing_progress_directory(self, tmp_path: Path) -> None:
        """No error when .loom/progress/ directory does not exist."""
        # Don't create the progress dir — _make_context creates it, so call
        # the method again after removing the directory.
        config = ShepherdConfig(issue=42, task_id="abc1234")
        (tmp_path / ".loom" / "progress").mkdir(parents=True, exist_ok=True)
        ctx = ShepherdContext(config=config, repo_root=tmp_path)
        # Remove the directory after construction to test re-invocation
        (tmp_path / ".loom" / "progress").rmdir()

        # Should not raise
        ctx._cleanup_stale_progress_for_issue()

    def test_handles_malformed_json(self, tmp_path: Path) -> None:
        """Malformed JSON files are skipped without error."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        (progress_dir / "shepherd-bad.json").write_text("{not valid json!!!")

        # Should not raise
        _make_context(tmp_path, issue=42, task_id="new5678")

        # Malformed file is left in place (not deleted, not crashed)
        assert (progress_dir / "shepherd-bad.json").exists()

    def test_handles_oserror_on_read(self, tmp_path: Path) -> None:
        """Unreadable files are skipped gracefully."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        bad_file = progress_dir / "shepherd-noperm.json"
        bad_file.write_text(json.dumps({"issue": 42, "task_id": "old"}))
        bad_file.chmod(0o000)

        try:
            # Should not raise
            _make_context(tmp_path, issue=42, task_id="new5678")
        finally:
            # Restore permissions for cleanup
            bad_file.chmod(0o644)

    def test_handles_oserror_on_unlink(self, tmp_path: Path) -> None:
        """Undeletable files do not cause an exception."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        stale_file = progress_dir / "shepherd-stale.json"
        stale_file.write_text(json.dumps({"issue": 42, "task_id": "old"}))

        # Make the directory read-only so unlink fails
        progress_dir.chmod(0o555)

        try:
            # Should not raise — OSError on unlink is caught
            _make_context(tmp_path, issue=42, task_id="new5678")
        finally:
            # Restore permissions for cleanup
            progress_dir.chmod(0o755)


class TestCleanupHeartbeatFreshness:
    """Tests for heartbeat freshness check in _cleanup_stale_progress_for_issue()."""

    def test_fresh_heartbeat_preserves_file(self, tmp_path: Path) -> None:
        """Progress file with a fresh heartbeat is NOT removed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        fresh_ts = (datetime.now(timezone.utc) - timedelta(seconds=60)).isoformat()
        _write_progress(
            progress_dir,
            "shepherd-live123.json",
            {"issue": 42, "task_id": "live123", "last_heartbeat": fresh_ts},
        )

        _make_context(tmp_path, issue=42, task_id="new5678")

        assert (progress_dir / "shepherd-live123.json").exists()

    def test_stale_heartbeat_removes_file(self, tmp_path: Path) -> None:
        """Progress file with a stale heartbeat IS removed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        stale_ts = (
            datetime.now(timezone.utc) - timedelta(seconds=_HEARTBEAT_FRESH_THRESHOLD + 60)
        ).isoformat()
        _write_progress(
            progress_dir,
            "shepherd-dead123.json",
            {"issue": 42, "task_id": "dead123", "last_heartbeat": stale_ts},
        )

        _make_context(tmp_path, issue=42, task_id="new5678")

        assert not (progress_dir / "shepherd-dead123.json").exists()

    def test_no_heartbeat_removes_file(self, tmp_path: Path) -> None:
        """Progress file with no last_heartbeat field is removed (backward compat)."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        _write_progress(
            progress_dir,
            "shepherd-old123.json",
            {"issue": 42, "task_id": "old123"},
        )

        _make_context(tmp_path, issue=42, task_id="new5678")

        assert not (progress_dir / "shepherd-old123.json").exists()

    def test_unparseable_heartbeat_removes_file(self, tmp_path: Path) -> None:
        """Progress file with an invalid heartbeat timestamp is treated as stale."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        _write_progress(
            progress_dir,
            "shepherd-bad123.json",
            {"issue": 42, "task_id": "bad123", "last_heartbeat": "not-a-timestamp"},
        )

        _make_context(tmp_path, issue=42, task_id="new5678")

        assert not (progress_dir / "shepherd-bad123.json").exists()

    def test_fresh_heartbeat_logs_skip_message(self, tmp_path: Path, caplog) -> None:
        """Skipping a file with a fresh heartbeat logs an info message."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        fresh_ts = (datetime.now(timezone.utc) - timedelta(seconds=30)).isoformat()
        _write_progress(
            progress_dir,
            "shepherd-live456.json",
            {"issue": 42, "task_id": "live456", "last_heartbeat": fresh_ts},
        )

        with caplog.at_level(logging.INFO):
            _make_context(tmp_path, issue=42, task_id="new5678")

        assert "Skipping progress file" in caplog.text
        assert "heartbeat is fresh" in caplog.text


class TestReportMilestone:
    """Tests for ShepherdContext.report_milestone() behavior."""

    def test_started_event_sets_initialized_flag(self, tmp_path: Path) -> None:
        """The 'started' event sets _progress_initialized to True on success."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")

        # Initially not initialized
        assert not ctx._progress_initialized

        # Report started event
        result = ctx.report_milestone("started", issue=42, mode="default")

        # Should succeed and set the flag
        assert result is True
        assert ctx._progress_initialized is True

    def test_subsequent_events_skip_when_not_initialized(self, tmp_path: Path) -> None:
        """Non-started events are silently skipped when progress is not initialized."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")

        # Don't call started, so _progress_initialized is False
        assert not ctx._progress_initialized

        # Subsequent milestone calls should return False silently (no error)
        result = ctx.report_milestone("phase_entered", phase="builder")
        assert result is False

        # Still not initialized
        assert not ctx._progress_initialized

    def test_subsequent_events_work_when_initialized(self, tmp_path: Path) -> None:
        """Non-started events work when progress is initialized."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")

        # Initialize with started event
        ctx.report_milestone("started", issue=42, mode="default")
        assert ctx._progress_initialized is True

        # Subsequent events should work
        result = ctx.report_milestone("phase_entered", phase="builder")
        assert result is True

    def test_started_failure_prevents_subsequent_calls(self, tmp_path: Path, caplog) -> None:
        """If 'started' fails, subsequent calls are skipped without error spam."""
        import logging

        ctx = _make_context(tmp_path, issue=42, task_id="invalid")  # Invalid task_id

        # Started will fail due to invalid task_id format
        with caplog.at_level(logging.WARNING):
            result = ctx.report_milestone("started", issue=42, mode="default")

        # Should fail
        assert result is False
        assert not ctx._progress_initialized

        # Clear the log to check that subsequent calls don't log errors
        caplog.clear()

        # Subsequent calls should return False silently
        with caplog.at_level(logging.WARNING):
            result = ctx.report_milestone("phase_entered", phase="builder")

        assert result is False
        # No error logged for the skipped call (we skip silently)
        assert "No progress file found" not in caplog.text

    def test_started_failure_logs_warning(self, tmp_path: Path, caplog) -> None:
        """If 'started' fails, a warning is logged."""
        import logging

        ctx = _make_context(tmp_path, issue=42, task_id="invalid")  # Invalid task_id

        with caplog.at_level(logging.WARNING):
            ctx.report_milestone("started", issue=42, mode="default")

        # Should log a warning about the failure
        assert "Failed to initialize progress file" in caplog.text
        assert "invalid" in caplog.text


class TestValidateIssueBlockedLabel:
    """Tests for loom:blocked label handling in validate_issue()."""

    def test_blocked_label_raises_error_in_default_mode(self, tmp_path: Path) -> None:
        """Issue with loom:blocked label raises IssueBlockedError in default mode."""
        config = ShepherdConfig(issue=42, task_id="abc1234", mode=ExecutionMode.DEFAULT)
        (tmp_path / ".loom" / "progress").mkdir(parents=True, exist_ok=True)
        ctx = ShepherdContext(config=config, repo_root=tmp_path)

        # Mock gh issue view to return issue with blocked label
        gh_result = MagicMock()
        gh_result.returncode = 0
        gh_result.stdout = json.dumps({
            "url": "https://github.com/test/repo/issues/42",
            "state": "OPEN",
            "title": "Test Issue",
            "labels": [{"name": "loom:blocked"}],
        })

        with patch("subprocess.run", return_value=gh_result):
            with pytest.raises(IssueBlockedError):
                ctx.validate_issue()

    def test_blocked_label_removed_in_merge_mode(self, tmp_path: Path, caplog) -> None:
        """Issue with loom:blocked label is cleared in merge mode (--merge)."""
        config = ShepherdConfig(
            issue=42, task_id="abc1234", mode=ExecutionMode.FORCE_MERGE
        )
        (tmp_path / ".loom" / "progress").mkdir(parents=True, exist_ok=True)
        ctx = ShepherdContext(config=config, repo_root=tmp_path)

        # Mock gh issue view to return issue with blocked label
        gh_result = MagicMock()
        gh_result.returncode = 0
        gh_result.stdout = json.dumps({
            "url": "https://github.com/test/repo/issues/42",
            "state": "OPEN",
            "title": "Test Issue",
            "labels": [{"name": "loom:blocked"}, {"name": "loom:issue"}],
        })

        remove_label_calls = []

        def mock_run(cmd, **kwargs):
            if "issue" in cmd and "view" in cmd:
                return gh_result
            if "issue" in cmd and "edit" in cmd and "--remove-label" in cmd:
                # Capture the label being removed
                remove_idx = cmd.index("--remove-label")
                remove_label_calls.append(cmd[remove_idx + 1])
                result = MagicMock()
                result.returncode = 0
                return result
            # Default mock for other commands (like git ls-remote)
            result = MagicMock()
            result.returncode = 1
            result.stdout = ""
            return result

        with patch("subprocess.run", side_effect=mock_run):
            with caplog.at_level(logging.WARNING):
                meta = ctx.validate_issue()

        # Should succeed, not raise
        assert meta["title"] == "Test Issue"

        # Should have removed the blocked label
        assert "loom:blocked" in remove_label_calls

        # Should have logged a warning
        assert "loom:blocked" in caplog.text
        assert "merge mode override" in caplog.text

    def test_blocked_label_updates_cache_in_merge_mode(self, tmp_path: Path) -> None:
        """Removing blocked label in merge mode updates the label cache."""
        config = ShepherdConfig(
            issue=42, task_id="abc1234", mode=ExecutionMode.FORCE_MERGE
        )
        (tmp_path / ".loom" / "progress").mkdir(parents=True, exist_ok=True)
        ctx = ShepherdContext(config=config, repo_root=tmp_path)

        # Mock gh issue view to return issue with blocked label
        gh_result = MagicMock()
        gh_result.returncode = 0
        gh_result.stdout = json.dumps({
            "url": "https://github.com/test/repo/issues/42",
            "state": "OPEN",
            "title": "Test Issue",
            "labels": [{"name": "loom:blocked"}, {"name": "loom:issue"}],
        })

        def mock_run(cmd, **kwargs):
            if "issue" in cmd and "view" in cmd:
                return gh_result
            result = MagicMock()
            result.returncode = 0 if "edit" in cmd else 1
            result.stdout = ""
            return result

        with patch("subprocess.run", side_effect=mock_run):
            ctx.validate_issue()

        # After validate_issue, the label cache should NOT contain loom:blocked
        cached_labels = ctx.label_cache.get_issue_labels(42)
        assert "loom:blocked" not in cached_labels
        assert "loom:issue" in cached_labels

    def test_no_blocked_label_no_changes(self, tmp_path: Path) -> None:
        """Issue without loom:blocked label proceeds normally in merge mode."""
        config = ShepherdConfig(
            issue=42, task_id="abc1234", mode=ExecutionMode.FORCE_MERGE
        )
        (tmp_path / ".loom" / "progress").mkdir(parents=True, exist_ok=True)
        ctx = ShepherdContext(config=config, repo_root=tmp_path)

        # Mock gh issue view to return issue WITHOUT blocked label
        gh_result = MagicMock()
        gh_result.returncode = 0
        gh_result.stdout = json.dumps({
            "url": "https://github.com/test/repo/issues/42",
            "state": "OPEN",
            "title": "Test Issue",
            "labels": [{"name": "loom:issue"}],
        })

        edit_calls = []

        def mock_run(cmd, **kwargs):
            if "issue" in cmd and "view" in cmd:
                return gh_result
            if "issue" in cmd and "edit" in cmd:
                edit_calls.append(cmd)
            result = MagicMock()
            result.returncode = 1
            result.stdout = ""
            return result

        with patch("subprocess.run", side_effect=mock_run):
            meta = ctx.validate_issue()

        # Should succeed
        assert meta["title"] == "Test Issue"

        # Should NOT have called edit (no label to remove)
        # Only the git ls-remote call might have been made
        label_edit_calls = [c for c in edit_calls if "--remove-label" in c]
        assert len(label_edit_calls) == 0


class TestRunScriptMissingScript:
    """Tests for run_script() when scripts are missing (issues #2147, #2289)."""

    def test_missing_script_falls_back_to_main_branch(self, tmp_path: Path) -> None:
        """run_script() extracts script from main when missing on current branch."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")
        # scripts_dir exists but the script file does not
        (tmp_path / ".loom" / "scripts").mkdir(parents=True, exist_ok=True)

        # Mock subprocess.run: first call is git show (success), second is script execution
        git_show_result = MagicMock()
        git_show_result.returncode = 0
        git_show_result.stdout = "#!/bin/sh\necho ok\n"

        exec_result = MagicMock()
        exec_result.returncode = 0
        exec_result.stdout = "ok\n"

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                # git show main:.loom/scripts/nonexistent.sh
                assert cmd[0] == "git"
                assert cmd[1] == "show"
                assert "main:.loom/scripts/test-script.sh" in cmd[2]
                return git_show_result
            # Script execution from temp file
            return exec_result

        with patch("loom_tools.shepherd.context.subprocess.run", side_effect=mock_run):
            result = ctx.run_script("test-script.sh", [], check=False)
            assert result.returncode == 0

    def test_missing_script_raises_when_not_on_main_either(self, tmp_path: Path) -> None:
        """run_script() raises FileNotFoundError when script missing on both branches."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")
        (tmp_path / ".loom" / "scripts").mkdir(parents=True, exist_ok=True)

        # Mock git show to fail (script not on main either)
        git_show_result = MagicMock()
        git_show_result.returncode = 1
        git_show_result.stdout = ""

        with patch("loom_tools.shepherd.context.subprocess.run", return_value=git_show_result):
            with pytest.raises(FileNotFoundError, match="could not extract from main"):
                ctx.run_script("nonexistent.sh", [])

    def test_existing_script_runs_normally(self, tmp_path: Path) -> None:
        """run_script() works normally when the script file exists."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")
        scripts_dir = tmp_path / ".loom" / "scripts"
        scripts_dir.mkdir(parents=True, exist_ok=True)
        script = scripts_dir / "test-script.sh"
        script.write_text("#!/bin/sh\necho ok\n")
        script.chmod(0o755)

        # Mock subprocess.run to avoid deadlock with pytest-asyncio's event loop
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = "ok\n"
        with patch("loom_tools.shepherd.context.subprocess.run", return_value=mock_result) as mock_run:
            result = ctx.run_script("test-script.sh", [], check=False)
            assert result.returncode == 0
            assert "ok" in result.stdout
            # Verify subprocess.run was called with correct args
            mock_run.assert_called_once()
            call_kwargs = mock_run.call_args
            assert str(call_kwargs[0][0][0]).endswith("test-script.sh")
            assert call_kwargs[1]["stdin"] is not None  # stdin=DEVNULL

    def test_fallback_passes_args_to_extracted_script(self, tmp_path: Path) -> None:
        """run_script() passes arguments correctly when using main branch fallback."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")
        (tmp_path / ".loom" / "scripts").mkdir(parents=True, exist_ok=True)

        git_show_result = MagicMock()
        git_show_result.returncode = 0
        git_show_result.stdout = "#!/bin/sh\necho $1\n"

        exec_result = MagicMock()
        exec_result.returncode = 0
        exec_result.stdout = "42\n"

        calls = []

        def mock_run(cmd, **kwargs):
            calls.append(cmd)
            if cmd[0] == "git":
                return git_show_result
            return exec_result

        with patch("loom_tools.shepherd.context.subprocess.run", side_effect=mock_run):
            ctx.run_script("merge-pr.sh", ["42", "--auto"], check=False)

        # Second call should be the temp script with args
        assert len(calls) == 2
        exec_cmd = calls[1]
        assert "42" in exec_cmd
        assert "--auto" in exec_cmd

    def test_fallback_cleans_up_temp_file(self, tmp_path: Path) -> None:
        """run_script() removes temp file after execution."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")
        (tmp_path / ".loom" / "scripts").mkdir(parents=True, exist_ok=True)

        git_show_result = MagicMock()
        git_show_result.returncode = 0
        git_show_result.stdout = "#!/bin/sh\necho ok\n"

        exec_result = MagicMock()
        exec_result.returncode = 0

        temp_paths: list[str] = []

        def mock_run(cmd, **kwargs):
            if cmd[0] == "git":
                return git_show_result
            # Capture the temp file path
            temp_paths.append(cmd[0])
            return exec_result

        with patch("loom_tools.shepherd.context.subprocess.run", side_effect=mock_run):
            ctx.run_script("test-script.sh", [], check=False)

        # Temp file should have been cleaned up
        assert len(temp_paths) == 1
        assert not Path(temp_paths[0]).exists()


def _mock_gh_issue(labels: list[str], title: str = "Test Issue", issue: int = 42) -> MagicMock:
    """Create a mock subprocess result for gh issue view."""
    result = MagicMock()
    result.returncode = 0
    result.stdout = json.dumps({
        "url": f"https://github.com/test/repo/issues/{issue}",
        "state": "OPEN",
        "title": title,
        "labels": [{"name": name} for name in labels],
    })
    return result


def _mock_run_for_issue(gh_result: MagicMock):
    """Create a side_effect function that returns gh_result for issue view."""
    def mock_run(cmd, **kwargs):
        if "issue" in cmd and "view" in cmd:
            return gh_result
        # Default: fail silently (e.g. git ls-remote)
        result = MagicMock()
        result.returncode = 1
        result.stdout = ""
        return result
    return mock_run


class TestValidateIssueEpicRejection:
    """Tests for epic/tracking issue detection in validate_issue()."""

    def test_loom_epic_label_raises_error(self, tmp_path: Path) -> None:
        """Issue with loom:epic label raises IssueIsEpicError."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:epic", "loom:issue"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)):
            with pytest.raises(IssueIsEpicError, match="epic"):
                ctx.validate_issue()

    def test_epic_label_raises_error(self, tmp_path: Path) -> None:
        """Issue with plain 'epic' label raises IssueIsEpicError."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["epic"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)):
            with pytest.raises(IssueIsEpicError, match="epic"):
                ctx.validate_issue()

    def test_epic_phase_label_is_allowed(self, tmp_path: Path) -> None:
        """Issue with loom:epic-phase label is NOT rejected (concrete work item)."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:epic-phase", "loom:issue"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)):
            meta = ctx.validate_issue()

        assert meta["title"] == "Test Issue"

    def test_epic_with_epic_phase_is_allowed(self, tmp_path: Path) -> None:
        """Issue with both loom:epic and loom:epic-phase passes (phase takes precedence)."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:epic", "loom:epic-phase", "loom:issue"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)):
            meta = ctx.validate_issue()

        assert meta["title"] == "Test Issue"

    def test_epic_with_loom_issue_rejected(self, tmp_path: Path) -> None:
        """Epic rejection takes priority even when loom:issue is also present."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:epic", "loom:issue"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)):
            with pytest.raises(IssueIsEpicError):
                ctx.validate_issue()

    def test_epic_rejected_even_in_force_mode(self, tmp_path: Path) -> None:
        """Epic rejection applies even with --force (epics are not implementable)."""
        config = ShepherdConfig(
            issue=42, task_id="abc1234", mode=ExecutionMode.FORCE_MERGE
        )
        (tmp_path / ".loom" / "progress").mkdir(parents=True, exist_ok=True)
        ctx = ShepherdContext(config=config, repo_root=tmp_path)
        gh_result = _mock_gh_issue(["loom:epic"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)):
            with pytest.raises(IssueIsEpicError):
                ctx.validate_issue()


class TestValidateIssueMergedPR:
    """Tests for merged-PR detection in validate_issue()."""

    def test_merged_pr_raises_issue_closed_error(self, tmp_path: Path) -> None:
        """Issue with a merged PR raises IssueClosedError."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:issue"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)), \
             patch("loom_tools.shepherd.context.gh_list", return_value=[{"number": 100}]):
            with pytest.raises(IssueClosedError, match="RESOLVED by merged PR #100"):
                ctx.validate_issue()

    def test_no_merged_pr_proceeds_normally(self, tmp_path: Path) -> None:
        """Issue without a merged PR proceeds through validation."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:issue"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)), \
             patch("loom_tools.shepherd.context.gh_list", return_value=[]):
            meta = ctx.validate_issue()

        assert meta["title"] == "Test Issue"

    def test_gh_list_failure_does_not_block(self, tmp_path: Path) -> None:
        """If gh_list raises an exception, validation proceeds normally."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:issue"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)), \
             patch("loom_tools.shepherd.context.gh_list", side_effect=OSError("network error")):
            meta = ctx.validate_issue()

        assert meta["title"] == "Test Issue"

    def test_merged_pr_checked_before_stale_branch(self, tmp_path: Path) -> None:
        """Merged PR check runs before stale branch check (early exit)."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:issue"])

        stale_branch_called = False

        original_check_stale = ctx._check_stale_branch

        def tracking_check_stale(issue: int) -> None:
            nonlocal stale_branch_called
            stale_branch_called = True
            original_check_stale(issue)

        ctx._check_stale_branch = tracking_check_stale

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)), \
             patch("loom_tools.shepherd.context.gh_list", return_value=[{"number": 200}]):
            with pytest.raises(IssueClosedError):
                ctx.validate_issue()

        # Stale branch check should not have been reached
        assert not stale_branch_called

    def test_merged_pr_error_state_contains_pr_number(self, tmp_path: Path) -> None:
        """IssueClosedError.state includes the PR number for clear messaging."""
        ctx = _make_context(tmp_path, issue=42)
        gh_result = _mock_gh_issue(["loom:building"])

        with patch("subprocess.run", side_effect=_mock_run_for_issue(gh_result)), \
             patch("loom_tools.shepherd.context.gh_list", return_value=[{"number": 333}]):
            with pytest.raises(IssueClosedError) as exc_info:
                ctx.validate_issue()

        assert exc_info.value.state == "RESOLVED by merged PR #333"
        assert exc_info.value.issue == 42
