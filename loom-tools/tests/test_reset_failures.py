"""Tests for the reset_failures function in loom_tools.common.issue_failures."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.common.issue_failures import (
    load_failure_log,
    record_failure,
    reset_failures,
)


@pytest.fixture
def repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo with .loom directory and daemon-state.json."""
    (tmp_path / ".git").mkdir()
    loom_dir = tmp_path / ".loom"
    loom_dir.mkdir()
    return tmp_path


def _write_daemon_state(repo: pathlib.Path, data: dict) -> None:
    (repo / ".loom" / "daemon-state.json").write_text(json.dumps(data))


def _read_daemon_state(repo: pathlib.Path) -> dict:
    return json.loads((repo / ".loom" / "daemon-state.json").read_text())


def _read_failures(repo: pathlib.Path) -> dict:
    return json.loads((repo / ".loom" / "issue-failures.json").read_text())


# ── reset_failures for a single issue ─────────────────────────


class TestResetSingleIssue:
    def test_clears_persistent_failure_log_entry(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck", phase="builder")
        record_failure(repo, 99, error_class="judge_stuck", phase="judge")

        cleared = reset_failures(repo, issue=42)

        assert cleared == 1
        log = load_failure_log(repo)
        assert "42" not in log.entries
        assert "99" in log.entries

    def test_clears_blocked_issue_retries(self, repo: pathlib.Path) -> None:
        _write_daemon_state(repo, {
            "blocked_issue_retries": {
                "42": {"retry_count": 3, "retry_exhausted": True, "error_class": "builder_stuck"},
                "99": {"retry_count": 1, "retry_exhausted": False, "error_class": "judge_stuck"},
            },
            "recent_failures": [],
        })

        reset_failures(repo, issue=42)

        state = _read_daemon_state(repo)
        assert "42" not in state["blocked_issue_retries"]
        assert "99" in state["blocked_issue_retries"]

    def test_clears_recent_failures(self, repo: pathlib.Path) -> None:
        _write_daemon_state(repo, {
            "blocked_issue_retries": {},
            "recent_failures": [
                {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "2026-01-01T00:00:00Z"},
                {"issue": 99, "error_class": "judge_stuck", "phase": "judge", "timestamp": "2026-01-02T00:00:00Z"},
                {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "2026-01-03T00:00:00Z"},
            ],
        })

        reset_failures(repo, issue=42)

        state = _read_daemon_state(repo)
        assert len(state["recent_failures"]) == 1
        assert state["recent_failures"][0]["issue"] == 99

    def test_clears_needs_human_input(self, repo: pathlib.Path) -> None:
        _write_daemon_state(repo, {
            "blocked_issue_retries": {},
            "recent_failures": [],
            "needs_human_input": [
                {"type": "exhausted_retry", "issue": 42, "error_class": "builder_stuck"},
                {"type": "exhausted_retry", "issue": 99, "error_class": "judge_stuck"},
                {"type": "other", "reason": "manual request"},
            ],
        })

        reset_failures(repo, issue=42)

        state = _read_daemon_state(repo)
        assert len(state["needs_human_input"]) == 2
        issues = [h.get("issue") for h in state["needs_human_input"]]
        assert 42 not in issues

    def test_returns_zero_for_unknown_issue(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        cleared = reset_failures(repo, issue=999)
        assert cleared == 0

    def test_works_without_daemon_state(self, repo: pathlib.Path) -> None:
        """Should work even when daemon-state.json doesn't exist."""
        record_failure(repo, 42, error_class="builder_stuck")

        cleared = reset_failures(repo, issue=42)

        assert cleared == 1
        log = load_failure_log(repo)
        assert "42" not in log.entries


# ── reset_failures for all issues ─────────────────────────────


class TestResetAllIssues:
    def test_clears_all_persistent_entries(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        record_failure(repo, 99, error_class="judge_stuck")
        record_failure(repo, 100, error_class="doctor_exhausted")

        cleared = reset_failures(repo, issue=None)

        assert cleared == 3
        log = load_failure_log(repo)
        assert log.entries == {}

    def test_clears_all_daemon_state_fields(self, repo: pathlib.Path) -> None:
        _write_daemon_state(repo, {
            "blocked_issue_retries": {
                "42": {"retry_count": 3, "retry_exhausted": True},
                "99": {"retry_count": 1, "retry_exhausted": False},
            },
            "recent_failures": [
                {"issue": 42, "error_class": "builder_stuck"},
                {"issue": 99, "error_class": "judge_stuck"},
            ],
            "systematic_failure": {
                "active": True,
                "pattern": "builder_stuck",
                "count": 3,
            },
            "needs_human_input": [
                {"type": "exhausted_retry", "issue": 42},
                {"type": "other", "reason": "manual"},
            ],
            "running": True,
            "iteration": 10,
        })

        reset_failures(repo, issue=None)

        state = _read_daemon_state(repo)
        assert state["blocked_issue_retries"] == {}
        assert state["recent_failures"] == []
        assert state["systematic_failure"] == {}
        # Non-exhausted_retry entries should be preserved
        assert len(state["needs_human_input"]) == 1
        assert state["needs_human_input"][0]["type"] == "other"
        # Other state fields should be untouched
        assert state["running"] is True
        assert state["iteration"] == 10

    def test_returns_zero_when_no_entries(self, repo: pathlib.Path) -> None:
        cleared = reset_failures(repo, issue=None)
        assert cleared == 0

    def test_works_without_daemon_state(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")

        cleared = reset_failures(repo, issue=None)

        assert cleared == 1
        log = load_failure_log(repo)
        assert log.entries == {}


# ── Integration: reset then retry ─────────────────────────────


class TestResetThenRetry:
    def test_issue_passes_backoff_filter_after_reset(self, repo: pathlib.Path) -> None:
        """After reset, the issue should pass the backoff filter immediately."""
        from loom_tools.snapshot import filter_issues_by_failure_backoff

        # Record enough failures to trigger backoff
        for _ in range(4):
            record_failure(repo, 42, error_class="builder_stuck")

        # Verify issue is in backoff
        log = load_failure_log(repo)
        issues = [{"number": 42}]
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 0  # Should be blocked/in backoff

        # Reset failures
        reset_failures(repo, issue=42)

        # Verify issue now passes filter
        log = load_failure_log(repo)
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 1
        assert result[0]["number"] == 42

    def test_full_lifecycle_reset_all(self, repo: pathlib.Path) -> None:
        """Reset all -> verify clean state -> record new failure -> works."""
        record_failure(repo, 42, error_class="builder_stuck")
        record_failure(repo, 99, error_class="judge_stuck")

        _write_daemon_state(repo, {
            "blocked_issue_retries": {
                "42": {"retry_count": 5, "retry_exhausted": True},
            },
            "recent_failures": [
                {"issue": 42, "error_class": "builder_stuck"},
            ],
            "systematic_failure": {"active": True, "pattern": "builder_stuck"},
        })

        # Reset all
        reset_failures(repo, issue=None)

        # Verify clean state
        log = load_failure_log(repo)
        assert log.entries == {}
        state = _read_daemon_state(repo)
        assert state["blocked_issue_retries"] == {}
        assert state["recent_failures"] == []
        assert state["systematic_failure"] == {}

        # Record new failure - should work as fresh
        entry = record_failure(repo, 42, error_class="new_error")
        assert entry.total_failures == 1
