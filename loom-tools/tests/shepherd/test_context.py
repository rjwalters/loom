"""Tests for ShepherdContext, focusing on _cleanup_stale_progress_for_issue."""

from __future__ import annotations

import json
from pathlib import Path

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext


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
