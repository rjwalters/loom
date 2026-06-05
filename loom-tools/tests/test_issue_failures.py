"""Tests for loom_tools.common.issue_failures (persistent cross-session failure log)."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.common.issue_failures import (
    BACKOFF_BASE,
    INFRASTRUCTURE_ERROR_CLASSES,
    MAX_FAILURES_BEFORE_BLOCK,
    _DEFAULT_FAILURE_THRESHOLD,
    IssueFailureEntry,
    IssueFailureLog,
    _decay_on_main_advance,
    get_failure_entry,
    load_failure_log,
    merge_into_daemon_state,
    record_failure,
    record_success,
    save_failure_log,
)
from loom_tools.forge_snapshot import filter_issues_by_failure_backoff


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

    def test_infrastructure_error_never_auto_blocks(self) -> None:
        """Infrastructure failures should never trigger auto-block, regardless of count."""
        for error_class in INFRASTRUCTURE_ERROR_CLASSES:
            entry = IssueFailureEntry(
                total_failures=MAX_FAILURES_BEFORE_BLOCK + 10,
                error_class=error_class,
            )
            assert entry.should_auto_block is False, (
                f"{error_class} should not auto-block"
            )

    def test_dependency_blocked_never_auto_blocks(self) -> None:
        """dependency_blocked is an infrastructure error and should never auto-block."""
        assert "dependency_blocked" in INFRASTRUCTURE_ERROR_CLASSES
        entry = IssueFailureEntry(
            total_failures=MAX_FAILURES_BEFORE_BLOCK + 10,
            error_class="dependency_blocked",
        )
        assert entry.should_auto_block is False

    def test_non_infrastructure_error_still_auto_blocks(self) -> None:
        """Non-infrastructure failures still auto-block at threshold."""
        entry = IssueFailureEntry(
            total_failures=MAX_FAILURES_BEFORE_BLOCK,
            error_class="builder_stuck",
        )
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

    def test_merge_does_not_set_retry_exhausted_for_infra(self, repo: pathlib.Path) -> None:
        for _ in range(MAX_FAILURES_BEFORE_BLOCK):
            record_failure(repo, 42, error_class="mcp_infrastructure_failure")

        retries: dict = {}
        result = merge_into_daemon_state(repo, retries)
        assert result["42"]["retry_count"] == MAX_FAILURES_BEFORE_BLOCK
        assert "retry_exhausted" not in result["42"] or result["42"].get("retry_exhausted") is not True

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


# ── Configurable threshold ────────────────────────────────────


class TestConfigurableThreshold:
    def test_default_threshold_value(self) -> None:
        """Default threshold is 5 when env var is not set."""
        assert _DEFAULT_FAILURE_THRESHOLD == 5

    def test_env_var_overrides_threshold(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """LOOM_ISSUE_FAILURE_THRESHOLD env var overrides the default."""
        monkeypatch.setenv("LOOM_ISSUE_FAILURE_THRESHOLD", "10")
        # Re-import to pick up the env var change
        import importlib

        import loom_tools.common.issue_failures as mod

        importlib.reload(mod)
        try:
            assert mod.MAX_FAILURES_BEFORE_BLOCK == 10
        finally:
            # Restore original value
            monkeypatch.delenv("LOOM_ISSUE_FAILURE_THRESHOLD", raising=False)
            importlib.reload(mod)

    def test_entry_uses_module_level_threshold(self) -> None:
        """IssueFailureEntry.block_threshold falls back to MAX_FAILURES_BEFORE_BLOCK."""
        entry = IssueFailureEntry(total_failures=1, error_class="builder_stuck")
        assert entry.block_threshold == MAX_FAILURES_BEFORE_BLOCK


# ── Commit-based decay ────────────────────────────────────────


class TestDecayOnMainAdvance:
    """Tests for failure counter decay when main branch advances (issue #3112)."""

    def test_no_decay_when_sha_unchanged(self) -> None:
        """No decay when main SHA has not changed."""
        log = IssueFailureLog(
            entries={"42": IssueFailureEntry(issue=42, total_failures=4)},
            last_known_main_sha="aaa111",
        )
        modified = _decay_on_main_advance(log, "aaa111")
        assert modified is False
        assert log.entries["42"].total_failures == 4

    def test_no_decay_when_sha_first_seen(self) -> None:
        """First time seeing a SHA — record it, don't decay."""
        log = IssueFailureLog(
            entries={"42": IssueFailureEntry(issue=42, total_failures=4)},
            last_known_main_sha=None,
        )
        modified = _decay_on_main_advance(log, "aaa111")
        assert modified is False
        assert log.entries["42"].total_failures == 4
        assert log.last_known_main_sha == "aaa111"

    def test_decay_halves_failure_count(self) -> None:
        """When main advances, failure counts are halved."""
        log = IssueFailureLog(
            entries={"42": IssueFailureEntry(issue=42, total_failures=4)},
            last_known_main_sha="aaa111",
        )
        modified = _decay_on_main_advance(log, "bbb222")
        assert modified is True
        assert log.entries["42"].total_failures == 2
        assert log.last_known_main_sha == "bbb222"

    def test_decay_removes_zero_entries(self) -> None:
        """Entries that decay to zero are removed."""
        log = IssueFailureLog(
            entries={"42": IssueFailureEntry(issue=42, total_failures=1)},
            last_known_main_sha="aaa111",
        )
        modified = _decay_on_main_advance(log, "bbb222")
        assert modified is True
        assert "42" not in log.entries

    def test_decay_mixed_entries(self) -> None:
        """Some entries removed, others reduced."""
        log = IssueFailureLog(
            entries={
                "42": IssueFailureEntry(issue=42, total_failures=1),  # -> 0, removed
                "99": IssueFailureEntry(issue=99, total_failures=6),  # -> 3, kept
                "77": IssueFailureEntry(issue=77, total_failures=2),  # -> 1, kept
            },
            last_known_main_sha="aaa111",
        )
        modified = _decay_on_main_advance(log, "bbb222")
        assert modified is True
        assert "42" not in log.entries
        assert log.entries["99"].total_failures == 3
        assert log.entries["77"].total_failures == 1

    def test_decay_unblocks_previously_blocked_issue(self) -> None:
        """An issue at the block threshold is unblocked after decay."""
        entry = IssueFailureEntry(
            issue=42,
            total_failures=MAX_FAILURES_BEFORE_BLOCK,
            error_class="builder_stuck",
        )
        assert entry.should_auto_block is True

        log = IssueFailureLog(
            entries={"42": entry},
            last_known_main_sha="aaa111",
        )
        _decay_on_main_advance(log, "bbb222")

        # 5 // 2 = 2, which is below the block threshold
        assert log.entries["42"].total_failures == MAX_FAILURES_BEFORE_BLOCK // 2
        assert log.entries["42"].should_auto_block is False

    def test_no_decay_when_entries_empty(self) -> None:
        """No modification when there are no failure entries."""
        log = IssueFailureLog(
            entries={},
            last_known_main_sha="aaa111",
        )
        modified = _decay_on_main_advance(log, "bbb222")
        assert modified is False
        assert log.last_known_main_sha == "bbb222"

    def test_successive_decays(self) -> None:
        """Multiple main advances progressively decay failures."""
        log = IssueFailureLog(
            entries={"42": IssueFailureEntry(issue=42, total_failures=8)},
            last_known_main_sha="sha1",
        )

        # First advance: 8 -> 4
        _decay_on_main_advance(log, "sha2")
        assert log.entries["42"].total_failures == 4

        # Second advance: 4 -> 2
        _decay_on_main_advance(log, "sha3")
        assert log.entries["42"].total_failures == 2

        # Third advance: 2 -> 1
        _decay_on_main_advance(log, "sha4")
        assert log.entries["42"].total_failures == 1

        # Fourth advance: 1 -> 0, removed
        _decay_on_main_advance(log, "sha5")
        assert "42" not in log.entries


class TestLoadWithDecay:
    """Tests for load_failure_log with commit-based decay via _main_sha."""

    def test_load_triggers_decay_on_sha_change(self, repo: pathlib.Path) -> None:
        """Loading with a new SHA triggers decay and saves."""
        # Write initial state with known SHA
        _write_failures(repo, {
            "entries": {
                "42": {
                    "issue": 42,
                    "total_failures": 4,
                    "error_class": "builder_stuck",
                    "phase": "builder",
                }
            },
            "updated_at": "2026-01-01T00:00:00Z",
            "last_known_main_sha": "old_sha",
        })

        log = load_failure_log(repo, _main_sha="new_sha")
        assert log.entries["42"].total_failures == 2  # 4 // 2

        # Verify it was persisted
        data = _read_failures(repo)
        assert data["entries"]["42"]["total_failures"] == 2
        assert data["last_known_main_sha"] == "new_sha"

    def test_load_no_decay_when_sha_same(self, repo: pathlib.Path) -> None:
        """Loading with the same SHA does not trigger decay."""
        _write_failures(repo, {
            "entries": {
                "42": {
                    "issue": 42,
                    "total_failures": 4,
                    "error_class": "builder_stuck",
                    "phase": "builder",
                }
            },
            "updated_at": "2026-01-01T00:00:00Z",
            "last_known_main_sha": "same_sha",
        })

        log = load_failure_log(repo, _main_sha="same_sha")
        assert log.entries["42"].total_failures == 4

    def test_load_no_decay_when_sha_none(self, repo: pathlib.Path) -> None:
        """Loading with SHA=None (git unavailable) skips decay."""
        _write_failures(repo, {
            "entries": {
                "42": {
                    "issue": 42,
                    "total_failures": 4,
                    "error_class": "builder_stuck",
                    "phase": "builder",
                }
            },
            "updated_at": "2026-01-01T00:00:00Z",
            "last_known_main_sha": "old_sha",
        })

        log = load_failure_log(repo, _main_sha=None)
        assert log.entries["42"].total_failures == 4

    def test_load_decay_removes_entries_at_one(self, repo: pathlib.Path) -> None:
        """Entries with 1 failure are removed on decay (1 // 2 == 0)."""
        _write_failures(repo, {
            "entries": {
                "42": {
                    "issue": 42,
                    "total_failures": 1,
                    "error_class": "builder_stuck",
                    "phase": "builder",
                }
            },
            "updated_at": "2026-01-01T00:00:00Z",
            "last_known_main_sha": "old_sha",
        })

        log = load_failure_log(repo, _main_sha="new_sha")
        assert "42" not in log.entries

    def test_load_decay_then_filter_unblocks_issue(self, repo: pathlib.Path) -> None:
        """After decay, previously blocked issues pass the backoff filter."""
        _write_failures(repo, {
            "entries": {
                "42": {
                    "issue": 42,
                    "total_failures": MAX_FAILURES_BEFORE_BLOCK,
                    "error_class": "builder_stuck",
                    "phase": "builder",
                }
            },
            "updated_at": "2026-01-01T00:00:00Z",
            "last_known_main_sha": "old_sha",
        })

        # Before decay: issue is blocked
        log_before = load_failure_log(repo, _main_sha="old_sha")
        issues = [{"number": 42}]
        result = filter_issues_by_failure_backoff(issues, log_before, current_iteration=1)
        assert len(result) == 0  # Blocked

        # After main advances: issue is unblocked
        log_after = load_failure_log(repo, _main_sha="new_sha")
        result = filter_issues_by_failure_backoff(issues, log_after, current_iteration=3)
        assert len(result) == 1  # Unblocked (2 failures, backoff=2, iter 3 % 3 == 0)


class TestIssueFailureLogSerialization:
    """Tests for last_known_main_sha serialization."""

    def test_round_trip_with_sha(self) -> None:
        log = IssueFailureLog(
            entries={},
            updated_at="2026-01-01T00:00:00Z",
            last_known_main_sha="abc123def456",
        )
        data = log.to_dict()
        assert data["last_known_main_sha"] == "abc123def456"

        restored = IssueFailureLog.from_dict(data)
        assert restored.last_known_main_sha == "abc123def456"

    def test_round_trip_without_sha(self) -> None:
        log = IssueFailureLog(
            entries={},
            updated_at="2026-01-01T00:00:00Z",
        )
        data = log.to_dict()
        assert "last_known_main_sha" not in data

        restored = IssueFailureLog.from_dict(data)
        assert restored.last_known_main_sha is None

    def test_backward_compatible_load(self) -> None:
        """Old files without last_known_main_sha field load correctly."""
        data = {
            "entries": {
                "42": {
                    "issue": 42,
                    "total_failures": 3,
                    "error_class": "builder_stuck",
                }
            },
            "updated_at": "2026-01-01T00:00:00Z",
        }
        log = IssueFailureLog.from_dict(data)
        assert log.last_known_main_sha is None
        assert log.entries["42"].total_failures == 3
