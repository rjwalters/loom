"""Tests for loom_tools.common.issue_failures (persistent cross-session failure log)."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.common.issue_failures import (
    BACKOFF_BASE,
    MAX_FAILURES_BEFORE_BLOCK,
    IssueFailureEntry,
    IssueFailureLog,
    get_failure_entry,
    load_failure_log,
    merge_into_daemon_state,
    record_failure,
    record_success,
    save_failure_log,
)
from loom_tools.snapshot import filter_issues_by_failure_backoff


@pytest.fixture
def repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo with .loom directory."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    return tmp_path


def _write_failures(repo: pathlib.Path, data: dict) -> None:
    (repo / ".loom" / "issue-failures.json").write_text(json.dumps(data))


def _read_failures(repo: pathlib.Path) -> dict:
    return json.loads((repo / ".loom" / "issue-failures.json").read_text())


# ── IssueFailureEntry ──────────────────────────────────────────


class TestIssueFailureEntry:
    def test_backoff_first_failure(self) -> None:
        entry = IssueFailureEntry(total_failures=1)
        assert entry.backoff_iterations() == 0

    def test_backoff_second_failure(self) -> None:
        entry = IssueFailureEntry(total_failures=2)
        assert entry.backoff_iterations() == BACKOFF_BASE

    def test_backoff_third_failure(self) -> None:
        entry = IssueFailureEntry(total_failures=3)
        assert entry.backoff_iterations() == BACKOFF_BASE ** 2

    def test_backoff_fourth_failure(self) -> None:
        entry = IssueFailureEntry(total_failures=4)
        assert entry.backoff_iterations() == BACKOFF_BASE ** 3

    def test_backoff_at_max(self) -> None:
        entry = IssueFailureEntry(total_failures=MAX_FAILURES_BEFORE_BLOCK)
        assert entry.backoff_iterations() == -1

    def test_should_auto_block_false(self) -> None:
        entry = IssueFailureEntry(total_failures=4)
        assert entry.should_auto_block is False

    def test_should_auto_block_true(self) -> None:
        entry = IssueFailureEntry(total_failures=MAX_FAILURES_BEFORE_BLOCK)
        assert entry.should_auto_block is True

    def test_round_trip(self) -> None:
        entry = IssueFailureEntry(
            issue=42,
            total_failures=3,
            error_class="builder_stuck",
            phase="builder",
            details="timed out",
            first_failure_at="2026-01-01T00:00:00Z",
            last_failure_at="2026-01-03T00:00:00Z",
        )
        restored = IssueFailureEntry.from_dict(entry.to_dict())
        assert restored.issue == 42
        assert restored.total_failures == 3
        assert restored.error_class == "builder_stuck"
        assert restored.phase == "builder"
        assert restored.details == "timed out"
        assert restored.first_failure_at == "2026-01-01T00:00:00Z"
        assert restored.last_failure_at == "2026-01-03T00:00:00Z"


# ── Load / Save ────────────────────────────────────────────────


class TestLoadSave:
    def test_load_missing_file(self, repo: pathlib.Path) -> None:
        log = load_failure_log(repo)
        assert log.entries == {}

    def test_save_and_load(self, repo: pathlib.Path) -> None:
        log = IssueFailureLog(
            entries={
                "42": IssueFailureEntry(
                    issue=42,
                    total_failures=2,
                    error_class="builder_stuck",
                    phase="builder",
                ),
            }
        )
        save_failure_log(repo, log)

        loaded = load_failure_log(repo)
        assert "42" in loaded.entries
        assert loaded.entries["42"].total_failures == 2
        assert loaded.entries["42"].error_class == "builder_stuck"
        assert loaded.updated_at is not None

    def test_load_corrupt_file(self, repo: pathlib.Path) -> None:
        (repo / ".loom" / "issue-failures.json").write_text("not json{{{")
        log = load_failure_log(repo)
        assert log.entries == {}

    def test_load_empty_file(self, repo: pathlib.Path) -> None:
        (repo / ".loom" / "issue-failures.json").write_text("")
        log = load_failure_log(repo)
        assert log.entries == {}


# ── record_failure ─────────────────────────────────────────────


class TestRecordFailure:
    def test_records_first_failure(self, repo: pathlib.Path) -> None:
        entry = record_failure(
            repo, 42, error_class="builder_stuck", phase="builder", details="timed out"
        )
        assert entry.total_failures == 1
        assert entry.error_class == "builder_stuck"
        assert entry.phase == "builder"
        assert entry.details == "timed out"
        assert entry.first_failure_at is not None
        assert entry.last_failure_at is not None

    def test_increments_total_failures(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        entry = record_failure(repo, 42, error_class="judge_stuck", phase="judge")
        assert entry.total_failures == 2
        assert entry.error_class == "judge_stuck"

    def test_preserves_first_failure_at(self, repo: pathlib.Path) -> None:
        e1 = record_failure(repo, 42, error_class="builder_stuck")
        first = e1.first_failure_at
        e2 = record_failure(repo, 42, error_class="judge_stuck")
        assert e2.first_failure_at == first

    def test_multiple_issues(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        record_failure(repo, 99, error_class="judge_stuck")

        log = load_failure_log(repo)
        assert "42" in log.entries
        assert "99" in log.entries

    def test_persists_to_disk(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        data = _read_failures(repo)
        assert "42" in data["entries"]
        assert data["entries"]["42"]["total_failures"] == 1


# ── record_success ─────────────────────────────────────────────


class TestRecordSuccess:
    def test_clears_entry_on_success(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        record_success(repo, 42)

        log = load_failure_log(repo)
        assert "42" not in log.entries

    def test_noop_for_untracked_issue(self, repo: pathlib.Path) -> None:
        record_success(repo, 999)
        # No file created if no failures were recorded
        log = load_failure_log(repo)
        assert log.entries == {}

    def test_preserves_other_entries(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        record_failure(repo, 99, error_class="judge_stuck")
        record_success(repo, 42)

        log = load_failure_log(repo)
        assert "42" not in log.entries
        assert "99" in log.entries


# ── get_failure_entry ──────────────────────────────────────────


class TestGetFailureEntry:
    def test_returns_entry(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        entry = get_failure_entry(repo, 42)
        assert entry is not None
        assert entry.total_failures == 1

    def test_returns_none_when_absent(self, repo: pathlib.Path) -> None:
        entry = get_failure_entry(repo, 999)
        assert entry is None


# ── merge_into_daemon_state ────────────────────────────────────


class TestMergeIntoDaemonState:
    def test_merge_into_empty_state(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck", phase="builder")
        record_failure(repo, 42, error_class="builder_stuck", phase="builder")

        retries: dict = {}
        result = merge_into_daemon_state(repo, retries)
        assert "42" in result
        assert result["42"]["retry_count"] == 2
        assert result["42"]["error_class"] == "builder_stuck"

    def test_merge_preserves_higher_count(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")

        retries = {"42": {"retry_count": 5, "error_class": "old"}}
        result = merge_into_daemon_state(repo, retries)
        # Existing count is higher, so should be preserved
        assert result["42"]["retry_count"] == 5
        assert result["42"]["error_class"] == "old"

    def test_merge_updates_when_persistent_higher(self, repo: pathlib.Path) -> None:
        for _ in range(3):
            record_failure(repo, 42, error_class="builder_stuck", phase="builder")

        retries = {"42": {"retry_count": 1, "error_class": "old"}}
        result = merge_into_daemon_state(repo, retries)
        assert result["42"]["retry_count"] == 3
        assert result["42"]["error_class"] == "builder_stuck"

    def test_merge_sets_retry_exhausted(self, repo: pathlib.Path) -> None:
        for _ in range(MAX_FAILURES_BEFORE_BLOCK):
            record_failure(repo, 42, error_class="builder_stuck")

        retries: dict = {}
        result = merge_into_daemon_state(repo, retries)
        assert result["42"]["retry_exhausted"] is True

    def test_merge_no_failures(self, repo: pathlib.Path) -> None:
        retries = {"42": {"retry_count": 1}}
        result = merge_into_daemon_state(repo, retries)
        # No persistent failures, so original should be unchanged
        assert result["42"]["retry_count"] == 1


# ── filter_issues_by_failure_backoff ───────────────────────────


class TestFilterIssuesByFailureBackoff:
    def test_no_failures_passes_all(self) -> None:
        log = IssueFailureLog()
        issues = [{"number": 1}, {"number": 2}, {"number": 3}]
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 3

    def test_first_failure_passes(self) -> None:
        log = IssueFailureLog(
            entries={"1": IssueFailureEntry(issue=1, total_failures=1)}
        )
        issues = [{"number": 1}]
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 1

    def test_second_failure_backoff(self) -> None:
        log = IssueFailureLog(
            entries={"1": IssueFailureEntry(issue=1, total_failures=2)}
        )
        issues = [{"number": 1}]

        # backoff for 2nd failure = BACKOFF_BASE = 2
        # iteration 1: 1 % (2+1) = 1, skip
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 0

        # iteration 3: 3 % (2+1) = 0, pass
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=3)
        assert len(result) == 1

    def test_auto_block_always_skipped(self) -> None:
        log = IssueFailureLog(
            entries={
                "1": IssueFailureEntry(
                    issue=1, total_failures=MAX_FAILURES_BEFORE_BLOCK
                )
            }
        )
        issues = [{"number": 1}]
        # Should be skipped regardless of iteration
        for iteration in range(100):
            result = filter_issues_by_failure_backoff(issues, log, current_iteration=iteration)
            assert len(result) == 0

    def test_mixed_issues(self) -> None:
        log = IssueFailureLog(
            entries={
                "1": IssueFailureEntry(issue=1, total_failures=2),  # backoff
                "3": IssueFailureEntry(issue=3, total_failures=MAX_FAILURES_BEFORE_BLOCK),  # blocked
            }
        )
        issues = [{"number": 1}, {"number": 2}, {"number": 3}]

        # At iteration 1: issue 1 in backoff, issue 2 has no failures, issue 3 auto-blocked
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 1
        assert result[0]["number"] == 2

    def test_issue_without_number_passes(self) -> None:
        log = IssueFailureLog(
            entries={"1": IssueFailureEntry(issue=1, total_failures=MAX_FAILURES_BEFORE_BLOCK)}
        )
        issues = [{"title": "no number"}]
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 1


# ── Integration ────────────────────────────────────────────────


class TestIntegration:
    def test_full_lifecycle(self, repo: pathlib.Path) -> None:
        """Record failures -> check backoff -> succeed -> verify clean."""
        # Record 3 failures
        for _ in range(3):
            record_failure(repo, 42, error_class="builder_stuck", phase="builder")

        entry = get_failure_entry(repo, 42)
        assert entry is not None
        assert entry.total_failures == 3

        # Verify backoff is active
        log = load_failure_log(repo)
        issues = [{"number": 42}]
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 0  # Should be in backoff

        # Record success
        record_success(repo, 42)
        entry = get_failure_entry(repo, 42)
        assert entry is None

        # Verify issue passes filter now
        log = load_failure_log(repo)
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        assert len(result) == 1

    def test_auto_block_at_threshold(self, repo: pathlib.Path) -> None:
        """Issue auto-blocked after MAX_FAILURES_BEFORE_BLOCK failures."""
        for _ in range(MAX_FAILURES_BEFORE_BLOCK):
            entry = record_failure(repo, 42, error_class="builder_stuck")

        assert entry.should_auto_block is True
        assert entry.total_failures == MAX_FAILURES_BEFORE_BLOCK

    def test_merge_then_filter(self, repo: pathlib.Path) -> None:
        """Merge on startup -> filter during iteration."""
        # Simulate previous session failures
        for _ in range(3):
            record_failure(repo, 42, error_class="builder_stuck", phase="builder")

        # Simulate daemon startup merge
        retries: dict = {}
        retries = merge_into_daemon_state(repo, retries)
        assert retries["42"]["retry_count"] == 3

        # Simulate iteration filtering
        log = load_failure_log(repo)
        issues = [{"number": 42}, {"number": 99}]
        result = filter_issues_by_failure_backoff(issues, log, current_iteration=1)
        # Issue 42 should be in backoff, 99 should pass
        assert len(result) == 1
        assert result[0]["number"] == 99
