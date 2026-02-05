"""Tests for loom_tools.milestones."""

from __future__ import annotations

import json
import pathlib
import threading

import pytest

from loom_tools.common.repo import clear_repo_cache
from loom_tools.milestones import (
    _validate_task_id,
    main,
    report_milestone,
)


@pytest.fixture
def repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo with .git and .loom directories."""
    clear_repo_cache()
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    return tmp_path


def _read_progress(repo: pathlib.Path, task_id: str) -> dict:
    path = repo / ".loom" / "progress" / f"shepherd-{task_id}.json"
    return json.loads(path.read_text())


# ── Task ID validation ──────────────────────────────────────────


class TestValidateTaskId:
    def test_valid(self) -> None:
        _validate_task_id("a7dc1e0")

    def test_too_short(self) -> None:
        with pytest.raises(ValueError, match="Invalid task_id"):
            _validate_task_id("abc12")

    def test_too_long(self) -> None:
        with pytest.raises(ValueError, match="Invalid task_id"):
            _validate_task_id("a7dc1e0f")

    def test_uppercase(self) -> None:
        with pytest.raises(ValueError, match="Invalid task_id"):
            _validate_task_id("A7DC1E0")

    def test_non_hex(self) -> None:
        with pytest.raises(ValueError, match="Invalid task_id"):
            _validate_task_id("xyz1234")

    def test_empty(self) -> None:
        with pytest.raises(ValueError, match="Invalid task_id"):
            _validate_task_id("")


# ── init_progress_file (via started event) ──────────────────────


class TestStartedEvent:
    def test_creates_valid_json(self, repo: pathlib.Path) -> None:
        ok = report_milestone(repo, "abc1234", "started", issue=42)
        assert ok

        data = _read_progress(repo, "abc1234")
        assert data["task_id"] == "abc1234"
        assert data["issue"] == 42
        assert data["mode"] == ""
        assert data["current_phase"] == "started"
        assert data["status"] == "working"
        assert data["last_heartbeat"] == data["started_at"]
        assert len(data["milestones"]) == 1
        assert data["milestones"][0]["event"] == "started"
        assert data["milestones"][0]["data"]["issue"] == 42

    def test_with_mode(self, repo: pathlib.Path) -> None:
        ok = report_milestone(repo, "abc1234", "started", issue=42, mode="default")
        assert ok
        data = _read_progress(repo, "abc1234")
        assert data["mode"] == "default"
        assert data["milestones"][0]["data"]["mode"] == "default"

    def test_creates_progress_dir(self, repo: pathlib.Path) -> None:
        progress_dir = repo / ".loom" / "progress"
        assert not progress_dir.exists()
        report_milestone(repo, "abc1234", "started", issue=42)
        assert progress_dir.is_dir()

    def test_missing_issue_returns_false(self, repo: pathlib.Path) -> None:
        ok = report_milestone(repo, "abc1234", "started")
        assert not ok


# ── add_milestone (appending to existing file) ──────────────────


class TestAddMilestone:
    def test_appends_to_milestones(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "heartbeat", action="running tests")

        data = _read_progress(repo, "abc1234")
        assert len(data["milestones"]) == 2
        assert data["milestones"][1]["event"] == "heartbeat"
        assert data["milestones"][1]["data"]["action"] == "running tests"

    def test_updates_last_heartbeat(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        initial = _read_progress(repo, "abc1234")["last_heartbeat"]

        report_milestone(repo, "abc1234", "heartbeat", action="test")
        updated = _read_progress(repo, "abc1234")["last_heartbeat"]
        assert updated >= initial


# ── phase_entered ────────────────────────────────────────────────


class TestPhaseEntered:
    def test_updates_current_phase(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "phase_entered", phase="builder")

        data = _read_progress(repo, "abc1234")
        assert data["current_phase"] == "builder"

    def test_missing_phase_returns_false(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "phase_entered")
        assert not ok


# ── phase_completed ─────────────────────────────────────────────


class TestPhaseCompleted:
    def test_records_phase_completion(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(
            repo,
            "abc1234",
            "phase_completed",
            phase="builder",
            duration_seconds=120,
            status="success",
        )

        data = _read_progress(repo, "abc1234")
        milestone = data["milestones"][-1]
        assert milestone["event"] == "phase_completed"
        assert milestone["data"]["phase"] == "builder"
        assert milestone["data"]["duration_seconds"] == 120
        assert milestone["data"]["status"] == "success"

    def test_optional_fields(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "phase_completed", phase="curator")
        assert ok

        data = _read_progress(repo, "abc1234")
        milestone = data["milestones"][-1]
        assert milestone["data"]["phase"] == "curator"
        assert "duration_seconds" not in milestone["data"]
        assert "status" not in milestone["data"]

    def test_missing_phase_returns_false(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "phase_completed")
        assert not ok


# ── completed ────────────────────────────────────────────────────


class TestCompleted:
    def test_sets_status_completed(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "completed")

        data = _read_progress(repo, "abc1234")
        assert data["status"] == "completed"
        assert data["current_phase"] is None

    def test_with_pr_merged(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "completed", pr_merged=True)

        data = _read_progress(repo, "abc1234")
        assert data["milestones"][-1]["data"]["pr_merged"] is True


# ── error ────────────────────────────────────────────────────────


class TestErrorEvent:
    def test_sets_status_errored(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "error", error="build failed")

        data = _read_progress(repo, "abc1234")
        assert data["status"] == "errored"

    def test_will_retry_sets_retrying(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(
            repo, "abc1234", "error", error="build failed", will_retry=True
        )

        data = _read_progress(repo, "abc1234")
        assert data["status"] == "retrying"

    def test_missing_error_returns_false(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "error")
        assert not ok


# ── blocked ──────────────────────────────────────────────────────


class TestBlockedEvent:
    def test_sets_status_blocked(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "blocked", reason="dependency")

        data = _read_progress(repo, "abc1234")
        assert data["status"] == "blocked"

    def test_with_details(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(
            repo, "abc1234", "blocked", reason="dependency", details="needs #99"
        )

        data = _read_progress(repo, "abc1234")
        assert data["milestones"][-1]["data"]["details"] == "needs #99"

    def test_missing_reason_returns_false(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "blocked")
        assert not ok


# ── Other events ─────────────────────────────────────────────────


class TestOtherEvents:
    def test_worktree_created(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(
            repo, "abc1234", "worktree_created", path=".loom/worktrees/issue-42"
        )
        assert ok
        data = _read_progress(repo, "abc1234")
        assert data["milestones"][-1]["data"]["path"] == ".loom/worktrees/issue-42"

    def test_first_commit(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "first_commit", sha="deadbeef")
        assert ok
        data = _read_progress(repo, "abc1234")
        assert data["milestones"][-1]["data"]["sha"] == "deadbeef"

    def test_pr_created(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "pr_created", pr_number=99)
        assert ok
        data = _read_progress(repo, "abc1234")
        assert data["milestones"][-1]["data"]["pr_number"] == 99


# ── Missing progress file ───────────────────────────────────────


class TestMissingProgressFile:
    def test_non_started_event_without_file(self, repo: pathlib.Path) -> None:
        ok = report_milestone(repo, "abc1234", "heartbeat", action="test")
        assert not ok

    def test_started_creates_file(self, repo: pathlib.Path) -> None:
        ok = report_milestone(repo, "abc1234", "started", issue=42)
        assert ok
        path = repo / ".loom" / "progress" / "shepherd-abc1234.json"
        assert path.is_file()


# ── Concurrent write safety ─────────────────────────────────────


class TestConcurrentWrites:
    def test_concurrent_heartbeats_dont_corrupt(self, repo: pathlib.Path) -> None:
        report_milestone(repo, "abc1234", "started", issue=42)

        errors: list[Exception] = []

        def send_heartbeat(n: int) -> None:
            try:
                report_milestone(
                    repo, "abc1234", "heartbeat", quiet=True, action=f"beat-{n}"
                )
            except Exception as e:
                errors.append(e)

        threads = [
            threading.Thread(target=send_heartbeat, args=(i,)) for i in range(10)
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert not errors
        # File should still be valid JSON
        data = _read_progress(repo, "abc1234")
        assert data["task_id"] == "abc1234"
        assert len(data["milestones"]) >= 2  # at least started + some heartbeats


# ── CLI main() ───────────────────────────────────────────────────


class TestCLI:
    def test_help_returns_zero(self) -> None:
        assert main([]) == 0

    def test_missing_task_id_returns_one(self) -> None:
        assert main(["started"]) == 1

    def test_invalid_task_id_returns_one(self) -> None:
        assert main(["started", "--task-id", "INVALID"]) == 1

    def test_started_via_cli(
        self, repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.chdir(repo)
        clear_repo_cache()
        rc = main(["started", "--task-id", "abc1234", "--issue", "42", "--quiet"])
        assert rc == 0
        data = _read_progress(repo, "abc1234")
        assert data["issue"] == 42

    def test_phase_entered_via_cli(
        self, repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.chdir(repo)
        clear_repo_cache()
        main(["started", "--task-id", "abc1234", "--issue", "42", "--quiet"])
        rc = main(
            ["phase_entered", "--task-id", "abc1234", "--phase", "builder", "--quiet"]
        )
        assert rc == 0
        data = _read_progress(repo, "abc1234")
        assert data["current_phase"] == "builder"

    def test_phase_completed_via_cli(
        self, repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.chdir(repo)
        clear_repo_cache()
        main(["started", "--task-id", "abc1234", "--issue", "42", "--quiet"])
        rc = main(
            [
                "phase_completed",
                "--task-id",
                "abc1234",
                "--phase",
                "builder",
                "--duration-seconds",
                "120",
                "--status",
                "success",
                "--quiet",
            ]
        )
        assert rc == 0
        data = _read_progress(repo, "abc1234")
        milestone = data["milestones"][-1]
        assert milestone["event"] == "phase_completed"
        assert milestone["data"]["phase"] == "builder"
        assert milestone["data"]["duration_seconds"] == 120
        assert milestone["data"]["status"] == "success"


# ── JSON output compatibility ────────────────────────────────────


class TestOutputFormat:
    def test_field_order_matches_schema(self, repo: pathlib.Path) -> None:
        """Verify that the JSON output contains all expected fields."""
        report_milestone(repo, "abc1234", "started", issue=42, mode="default")
        data = _read_progress(repo, "abc1234")

        expected_keys = {
            "task_id",
            "issue",
            "mode",
            "started_at",
            "current_phase",
            "last_heartbeat",
            "status",
            "milestones",
        }
        assert set(data.keys()) == expected_keys

    def test_last_heartbeat_always_present(self, repo: pathlib.Path) -> None:
        """last_heartbeat must always be in the output (not conditionally omitted)."""
        report_milestone(repo, "abc1234", "started", issue=42)
        data = _read_progress(repo, "abc1234")
        assert "last_heartbeat" in data
        assert data["last_heartbeat"] is not None

    def test_completed_sets_current_phase_null(self, repo: pathlib.Path) -> None:
        """completed event should set current_phase to JSON null."""
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "completed")
        data = _read_progress(repo, "abc1234")
        assert data["current_phase"] is None

    def test_issue_is_integer(self, repo: pathlib.Path) -> None:
        """Issue number should be an integer, not a string."""
        report_milestone(repo, "abc1234", "started", issue=42)
        data = _read_progress(repo, "abc1234")
        assert isinstance(data["issue"], int)
        assert isinstance(data["milestones"][0]["data"]["issue"], int)

    def test_pr_number_is_integer(self, repo: pathlib.Path) -> None:
        """PR number should be an integer, not a string."""
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "pr_created", pr_number=99)
        data = _read_progress(repo, "abc1234")
        assert isinstance(data["milestones"][-1]["data"]["pr_number"], int)

    def test_pr_merged_is_boolean(self, repo: pathlib.Path) -> None:
        """pr_merged should be a boolean."""
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "completed", pr_merged=True)
        data = _read_progress(repo, "abc1234")
        assert data["milestones"][-1]["data"]["pr_merged"] is True

    def test_will_retry_is_boolean(self, repo: pathlib.Path) -> None:
        """will_retry should be a boolean."""
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "error", error="fail", will_retry=True)
        data = _read_progress(repo, "abc1234")
        assert data["milestones"][-1]["data"]["will_retry"] is True


# ── judge_retry ─────────────────────────────────────────────────


class TestJudgeRetryEvent:
    def test_records_judge_retry(self, repo: pathlib.Path) -> None:
        """judge_retry event should record attempt and optional fields."""
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(
            repo,
            "abc1234",
            "judge_retry",
            attempt=1,
            max_retries=3,
            reason="no review submitted",
        )
        assert ok

        data = _read_progress(repo, "abc1234")
        milestone = data["milestones"][-1]
        assert milestone["event"] == "judge_retry"
        assert milestone["data"]["attempt"] == 1
        assert milestone["data"]["max_retries"] == 3
        assert milestone["data"]["reason"] == "no review submitted"

    def test_attempt_only(self, repo: pathlib.Path) -> None:
        """judge_retry with only required attempt field."""
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "judge_retry", attempt=2)
        assert ok

        data = _read_progress(repo, "abc1234")
        milestone = data["milestones"][-1]
        assert milestone["event"] == "judge_retry"
        assert milestone["data"]["attempt"] == 2
        assert "max_retries" not in milestone["data"]
        assert "reason" not in milestone["data"]

    def test_missing_attempt_returns_false(self, repo: pathlib.Path) -> None:
        """judge_retry without attempt should fail."""
        report_milestone(repo, "abc1234", "started", issue=42)
        ok = report_milestone(repo, "abc1234", "judge_retry")
        assert not ok

    def test_attempt_is_integer(self, repo: pathlib.Path) -> None:
        """attempt should be an integer."""
        report_milestone(repo, "abc1234", "started", issue=42)
        report_milestone(repo, "abc1234", "judge_retry", attempt=1, max_retries=3)
        data = _read_progress(repo, "abc1234")
        assert isinstance(data["milestones"][-1]["data"]["attempt"], int)
        assert isinstance(data["milestones"][-1]["data"]["max_retries"], int)

    def test_judge_retry_via_cli(
        self, repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """judge_retry should work via CLI."""
        monkeypatch.chdir(repo)
        clear_repo_cache()
        main(["started", "--task-id", "abc1234", "--issue", "42", "--quiet"])
        rc = main(
            [
                "judge_retry",
                "--task-id",
                "abc1234",
                "--attempt",
                "1",
                "--max-retries",
                "3",
                "--reason",
                "no review submitted",
                "--quiet",
            ]
        )
        assert rc == 0
        data = _read_progress(repo, "abc1234")
        milestone = data["milestones"][-1]
        assert milestone["event"] == "judge_retry"
        assert milestone["data"]["attempt"] == 1
        assert milestone["data"]["max_retries"] == 3
        assert milestone["data"]["reason"] == "no review submitted"
