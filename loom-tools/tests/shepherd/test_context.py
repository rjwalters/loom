"""Tests for ShepherdContext, focusing on _cleanup_stale_progress_for_issue."""

from __future__ import annotations

import json
import logging
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ExecutionMode, ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.errors import IssueBlockedError


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
    """Tests for run_script() when scripts are missing (issue #2147)."""

    def test_missing_script_raises_file_not_found(self, tmp_path: Path) -> None:
        """run_script() raises FileNotFoundError for missing scripts."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")
        # scripts_dir exists but the script file does not
        (tmp_path / ".loom" / "scripts").mkdir(parents=True, exist_ok=True)

        with pytest.raises(FileNotFoundError, match="predate Loom installation"):
            ctx.run_script("nonexistent.sh", [])

    def test_existing_script_runs_normally(self, tmp_path: Path) -> None:
        """run_script() works normally when the script file exists."""
        ctx = _make_context(tmp_path, issue=42, task_id="abc1234")
        scripts_dir = tmp_path / ".loom" / "scripts"
        scripts_dir.mkdir(parents=True, exist_ok=True)
        script = scripts_dir / "test-script.sh"
        script.write_text("#!/bin/sh\necho ok\n")
        script.chmod(0o755)

        result = ctx.run_script("test-script.sh", [], check=False)
        assert result.returncode == 0
        assert "ok" in result.stdout
