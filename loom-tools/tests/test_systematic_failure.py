"""Tests for loom_tools.common.systematic_failure."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.common.systematic_failure import (
    clear_failures_for_issue,
    clear_systematic_failure,
    detect_systematic_failure,
    probe_started,
    record_blocked_reason,
)


@pytest.fixture
def repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo with .loom directory and empty daemon state."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    _write_state(tmp_path, {"running": True})
    return tmp_path


def _write_state(repo: pathlib.Path, data: dict) -> None:
    (repo / ".loom" / "daemon-state.json").write_text(json.dumps(data))


def _read_state(repo: pathlib.Path) -> dict:
    return json.loads((repo / ".loom" / "daemon-state.json").read_text())


# ── record_blocked_reason ────────────────────────────────────────


class TestRecordBlockedReason:
    def test_records_retry_metadata(self, repo: pathlib.Path) -> None:
        record_blocked_reason(
            repo, 42, error_class="builder_stuck", phase="builder", details="timed out"
        )
        state = _read_state(repo)

        retries = state["blocked_issue_retries"]
        assert "42" in retries
        entry = retries["42"]
        assert entry["error_class"] == "builder_stuck"
        assert entry["last_blocked_phase"] == "builder"
        assert entry["last_blocked_details"] == "timed out"
        assert entry["retry_count"] == 0
        assert entry["retry_exhausted"] is False

    def test_appends_to_recent_failures(self, repo: pathlib.Path) -> None:
        record_blocked_reason(repo, 42, error_class="builder_stuck", phase="builder")
        state = _read_state(repo)

        failures = state["recent_failures"]
        assert len(failures) == 1
        assert failures[0]["issue"] == 42
        assert failures[0]["error_class"] == "builder_stuck"
        assert failures[0]["phase"] == "builder"
        assert "timestamp" in failures[0]

    def test_preserves_existing_retry_count(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "blocked_issue_retries": {
                    "42": {"retry_count": 2, "last_retry_at": "2026-01-01T00:00:00Z"}
                }
            },
        )
        record_blocked_reason(repo, 42, error_class="judge_stuck", phase="judge")
        state = _read_state(repo)

        entry = state["blocked_issue_retries"]["42"]
        assert entry["retry_count"] == 2
        assert entry["error_class"] == "judge_stuck"

    def test_sliding_window_caps_at_20(self, repo: pathlib.Path) -> None:
        # Pre-populate with 20 failures
        existing = [
            {"issue": i, "error_class": "old", "phase": "builder", "timestamp": "t"}
            for i in range(20)
        ]
        _write_state(repo, {"recent_failures": existing})

        record_blocked_reason(repo, 99, error_class="new_failure", phase="merge")
        state = _read_state(repo)

        assert len(state["recent_failures"]) == 20
        assert state["recent_failures"][-1]["error_class"] == "new_failure"
        # First entry should be issue 1, not issue 0
        assert state["recent_failures"][0]["issue"] == 1

    def test_no_state_file_is_noop(self, tmp_path: pathlib.Path) -> None:
        (tmp_path / ".git").mkdir()
        (tmp_path / ".loom").mkdir()
        # No daemon-state.json exists
        record_blocked_reason(tmp_path, 42, error_class="builder_stuck")
        # Should not raise

    def test_multiple_issues(self, repo: pathlib.Path) -> None:
        record_blocked_reason(repo, 10, error_class="builder_stuck", phase="builder")
        record_blocked_reason(repo, 20, error_class="judge_stuck", phase="judge")
        state = _read_state(repo)

        assert "10" in state["blocked_issue_retries"]
        assert "20" in state["blocked_issue_retries"]
        assert len(state["recent_failures"]) == 2


# ── detect_systematic_failure ────────────────────────────────────


class TestDetectSystematicFailure:
    def test_no_failures_returns_none(self, repo: pathlib.Path) -> None:
        result = detect_systematic_failure(repo)
        assert result is None

    def test_below_threshold_returns_none(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "builder_stuck"},
                    {"error_class": "builder_stuck"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is None

    def test_detects_same_error_class(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "builder_stuck"},
                    {"error_class": "builder_stuck"},
                    {"error_class": "builder_stuck"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is not None
        assert result.active is True
        assert result.pattern == "builder_stuck"
        assert result.count == 3
        assert result.probe_count == 0

    def test_mixed_classes_returns_none(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "builder_stuck"},
                    {"error_class": "judge_stuck"},
                    {"error_class": "builder_stuck"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is None

    def test_only_checks_last_n(self, repo: pathlib.Path) -> None:
        """Earlier mixed failures shouldn't prevent detection."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "judge_stuck"},
                    {"error_class": "builder_stuck"},
                    {"error_class": "builder_stuck"},
                    {"error_class": "builder_stuck"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is not None
        assert result.pattern == "builder_stuck"

    def test_updates_state_file(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "merge_failed"},
                    {"error_class": "merge_failed"},
                    {"error_class": "merge_failed"},
                ]
            },
        )
        detect_systematic_failure(repo, update=True)
        state = _read_state(repo)

        sf = state["systematic_failure"]
        assert sf["active"] is True
        assert sf["pattern"] == "merge_failed"
        assert sf["count"] == 3
        assert sf["probe_count"] == 0
        assert "detected_at" in sf
        assert "cooldown_until" in sf

    def test_clears_state_when_no_pattern(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "systematic_failure": {"active": True, "pattern": "old"},
                "recent_failures": [
                    {"error_class": "a"},
                    {"error_class": "b"},
                    {"error_class": "c"},
                ],
            },
        )
        result = detect_systematic_failure(repo, update=True)
        assert result is None

        state = _read_state(repo)
        assert state["systematic_failure"] == {}

    def test_no_state_file_returns_none(self, tmp_path: pathlib.Path) -> None:
        (tmp_path / ".git").mkdir()
        (tmp_path / ".loom").mkdir()
        result = detect_systematic_failure(tmp_path)
        assert result is None

    def test_no_update_mode(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "builder_stuck"},
                    {"error_class": "builder_stuck"},
                    {"error_class": "builder_stuck"},
                ]
            },
        )
        result = detect_systematic_failure(repo, update=False)
        assert result is not None

        # State file should not be modified
        state = _read_state(repo)
        assert "systematic_failure" not in state

    def test_infra_failures_not_counted(self, repo: pathlib.Path) -> None:
        """3 consecutive infrastructure failures should NOT trigger detection."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "mcp_infrastructure_failure"},
                    {"error_class": "mcp_infrastructure_failure"},
                    {"error_class": "mcp_infrastructure_failure"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is None

    def test_worktree_conflict_not_counted(self, repo: pathlib.Path) -> None:
        """Consecutive worktree_conflict failures should NOT trigger systematic failure
        detection, because they are infrastructure failures (git state), not issue
        defects.  See issue #2918."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "worktree_conflict"},
                    {"error_class": "worktree_conflict"},
                    {"error_class": "worktree_conflict"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is None

    def test_worktree_conflict_mixed_with_other_failures(self, repo: pathlib.Path) -> None:
        """worktree_conflict failures are filtered out and don't inflate the count
        for non-infrastructure failures.  See issue #2918."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "builder_stuck"},
                    {"error_class": "worktree_conflict"},
                    {"error_class": "builder_stuck"},
                    {"error_class": "worktree_conflict"},
                    # Only 2 non-infra failures — below threshold of 3
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is None

    def test_infra_mixed_with_issue_failures(self, repo: pathlib.Path) -> None:
        """Infra failures interspersed with issue failures don't inflate the count."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "builder_stuck"},
                    {"error_class": "mcp_infrastructure_failure"},
                    {"error_class": "builder_stuck"},
                    {"error_class": "auth_infrastructure_failure"},
                    {"error_class": "builder_stuck"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is not None
        assert result.pattern == "builder_stuck"
        assert result.count == 3

    def test_only_non_infra_exceeds_threshold(self, repo: pathlib.Path) -> None:
        """Only non-infra failures that exceed the threshold trigger detection."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "mcp_infrastructure_failure"},
                    {"error_class": "mcp_infrastructure_failure"},
                    {"error_class": "builder_stuck"},
                    {"error_class": "judge_stuck"},
                    {"error_class": "builder_stuck"},
                ]
            },
        )
        # Only 3 non-infra: [builder_stuck, judge_stuck, builder_stuck] — mixed classes
        result = detect_systematic_failure(repo)
        assert result is None

    def test_custom_threshold(
        self, repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("LOOM_SYSTEMATIC_FAILURE_THRESHOLD", "2")
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"error_class": "doctor_exhausted"},
                    {"error_class": "doctor_exhausted"},
                ]
            },
        )
        result = detect_systematic_failure(repo)
        assert result is not None
        assert result.count == 2


# ── clear_systematic_failure ─────────────────────────────────────


class TestClearSystematicFailure:
    def test_clears_failure_and_recent(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "systematic_failure": {"active": True, "pattern": "builder_stuck"},
                "recent_failures": [{"error_class": "builder_stuck"}],
                "other_field": "preserved",
            },
        )
        clear_systematic_failure(repo)
        state = _read_state(repo)

        assert state["systematic_failure"] == {}
        assert state["recent_failures"] == []
        assert state["other_field"] == "preserved"

    def test_no_state_file_is_noop(self, tmp_path: pathlib.Path) -> None:
        (tmp_path / ".git").mkdir()
        (tmp_path / ".loom").mkdir()
        clear_systematic_failure(tmp_path)


# ── probe_started ────────────────────────────────────────────────


class TestProbeStarted:
    def test_increments_probe_count(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "systematic_failure": {
                    "active": True,
                    "probe_count": 0,
                }
            },
        )
        count = probe_started(repo)
        assert count == 1

        state = _read_state(repo)
        assert state["systematic_failure"]["probe_count"] == 1
        assert "cooldown_until" in state["systematic_failure"]

    def test_exponential_backoff(self, repo: pathlib.Path) -> None:
        _write_state(
            repo,
            {
                "systematic_failure": {
                    "active": True,
                    "probe_count": 2,
                }
            },
        )
        count = probe_started(repo)
        assert count == 3

    def test_no_state_file_returns_zero(self, tmp_path: pathlib.Path) -> None:
        (tmp_path / ".git").mkdir()
        (tmp_path / ".loom").mkdir()
        count = probe_started(tmp_path)
        assert count == 0


# ── clear_failures_for_issue ─────────────────────────────────────


class TestClearFailuresForIssue:
    def test_removes_only_target_issue(self, repo: pathlib.Path) -> None:
        """Only entries for the specified issue are removed."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"issue": 10, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t1"},
                    {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t2"},
                    {"issue": 10, "error_class": "judge_stuck", "phase": "judge", "timestamp": "t3"},
                    {"issue": 42, "error_class": "merge_failed", "phase": "merge", "timestamp": "t4"},
                ],
            },
        )
        cleared = clear_failures_for_issue(repo, 42)
        assert cleared == 2

        state = _read_state(repo)
        assert len(state["recent_failures"]) == 2
        assert all(f["issue"] == 10 for f in state["recent_failures"])

    def test_resets_blocked_issue_retries(self, repo: pathlib.Path) -> None:
        """blocked_issue_retries entry for the issue is reset."""
        _write_state(
            repo,
            {
                "blocked_issue_retries": {
                    "42": {
                        "retry_count": 2,
                        "retry_exhausted": True,
                        "last_retry_at": "2026-01-01T00:00:00Z",
                        "error_class": "builder_stuck",
                        "last_blocked_at": "2026-01-02T00:00:00Z",
                    },
                    "10": {
                        "retry_count": 1,
                        "retry_exhausted": False,
                    },
                },
                "recent_failures": [
                    {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t"},
                ],
            },
        )
        clear_failures_for_issue(repo, 42)
        state = _read_state(repo)

        # Issue 42 is reset
        entry_42 = state["blocked_issue_retries"]["42"]
        assert entry_42["retry_count"] == 0
        assert entry_42["retry_exhausted"] is False

        # Issue 10 is untouched
        entry_10 = state["blocked_issue_retries"]["10"]
        assert entry_10["retry_count"] == 1

    def test_leaves_other_issues_intact(self, repo: pathlib.Path) -> None:
        """Failures for other issues are preserved."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"issue": 10, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t1"},
                    {"issue": 20, "error_class": "judge_stuck", "phase": "judge", "timestamp": "t2"},
                    {"issue": 30, "error_class": "merge_failed", "phase": "merge", "timestamp": "t3"},
                ],
            },
        )
        cleared = clear_failures_for_issue(repo, 99)
        assert cleared == 0

        state = _read_state(repo)
        assert len(state["recent_failures"]) == 3

    def test_clears_systematic_failure_when_cause_removed(self, repo: pathlib.Path) -> None:
        """Systematic failure is cleared if the removed entries were the cause."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t1"},
                    {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t2"},
                    {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t3"},
                ],
                "systematic_failure": {
                    "active": True,
                    "pattern": "builder_stuck",
                    "count": 3,
                },
            },
        )
        clear_failures_for_issue(repo, 42)
        state = _read_state(repo)

        assert state["systematic_failure"] == {}
        assert state["recent_failures"] == []

    def test_preserves_systematic_failure_from_other_issues(self, repo: pathlib.Path) -> None:
        """Systematic failure stays if remaining failures still trigger it."""
        _write_state(
            repo,
            {
                "recent_failures": [
                    {"issue": 42, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t0"},
                    {"issue": 10, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t1"},
                    {"issue": 20, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t2"},
                    {"issue": 30, "error_class": "builder_stuck", "phase": "builder", "timestamp": "t3"},
                ],
                "systematic_failure": {
                    "active": True,
                    "pattern": "builder_stuck",
                    "count": 3,
                },
            },
        )
        clear_failures_for_issue(repo, 42)
        state = _read_state(repo)

        # 3 failures remain for other issues, all same class => systematic failure preserved
        assert len(state["recent_failures"]) == 3
        sf = state["systematic_failure"]
        assert sf.get("active") is True

    def test_no_prior_failures_is_noop(self, repo: pathlib.Path) -> None:
        """No failures for the issue means no changes, no errors."""
        _write_state(repo, {"recent_failures": [], "blocked_issue_retries": {}})
        cleared = clear_failures_for_issue(repo, 42)
        assert cleared == 0

    def test_no_state_file_returns_zero(self, tmp_path: pathlib.Path) -> None:
        """Missing daemon-state.json returns 0, no errors."""
        (tmp_path / ".git").mkdir()
        (tmp_path / ".loom").mkdir()
        cleared = clear_failures_for_issue(tmp_path, 42)
        assert cleared == 0


# ── Integration: record + detect ─────────────────────────────────


class TestIntegration:
    def test_record_then_detect(self, repo: pathlib.Path) -> None:
        """Recording 3 failures with the same class triggers detection."""
        for _ in range(3):
            record_blocked_reason(
                repo, 42, error_class="builder_stuck", phase="builder"
            )

        result = detect_systematic_failure(repo)
        assert result is not None
        assert result.pattern == "builder_stuck"

    def test_mixed_records_no_detection(self, repo: pathlib.Path) -> None:
        """Mixed error classes don't trigger detection."""
        record_blocked_reason(repo, 42, error_class="builder_stuck", phase="builder")
        record_blocked_reason(repo, 43, error_class="judge_stuck", phase="judge")
        record_blocked_reason(repo, 44, error_class="merge_failed", phase="merge")

        result = detect_systematic_failure(repo)
        assert result is None

    def test_full_lifecycle(self, repo: pathlib.Path) -> None:
        """Record failures -> detect -> clear -> verify clean."""
        for i in range(3):
            record_blocked_reason(
                repo, i, error_class="worktree_failed", phase="builder"
            )

        sf = detect_systematic_failure(repo)
        assert sf is not None
        assert sf.active is True

        clear_systematic_failure(repo)
        state = _read_state(repo)
        assert state["systematic_failure"] == {}
        assert state["recent_failures"] == []
