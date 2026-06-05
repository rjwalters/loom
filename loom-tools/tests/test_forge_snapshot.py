"""Tests for forge_snapshot.py — the kept forge-query half of the former snapshot.py.

Phase 3.2 (#3399): snapshot.py was split; the daemon-brain half was deleted.
This file covers the functions retained in forge_snapshot.py.
"""

from __future__ import annotations

import pathlib
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.forge_snapshot import (
    _has_label,
    collect_pipeline_data,
    detect_contradictory_labels,
    detect_orphaned_prs,
    detect_spinning_prs,
    filter_issues_by_failure_backoff,
    sort_issues_by_strategy,
)


# ---------------------------------------------------------------------------
# _has_label
# ---------------------------------------------------------------------------

class TestHasLabel:
    def test_present(self) -> None:
        item = {"labels": [{"name": "loom:issue"}, {"name": "loom:urgent"}]}
        assert _has_label(item, "loom:urgent") is True

    def test_absent(self) -> None:
        item = {"labels": [{"name": "loom:issue"}]}
        assert _has_label(item, "loom:building") is False

    def test_empty_labels(self) -> None:
        assert _has_label({"labels": []}, "loom:issue") is False

    def test_missing_labels_key(self) -> None:
        assert _has_label({}, "loom:issue") is False


# ---------------------------------------------------------------------------
# sort_issues_by_strategy
# ---------------------------------------------------------------------------

class TestSortIssuesByStrategy:
    def _make_issue(self, number: int, created_at: str, urgent: bool = False) -> dict:
        labels = [{"name": "loom:urgent"}] if urgent else []
        return {"number": number, "createdAt": created_at, "labels": labels}

    def test_fifo(self) -> None:
        issues = [
            self._make_issue(2, "2024-01-02"),
            self._make_issue(1, "2024-01-01"),
        ]
        result = sort_issues_by_strategy(issues, "fifo")
        assert [i["number"] for i in result] == [1, 2]

    def test_lifo(self) -> None:
        issues = [
            self._make_issue(1, "2024-01-01"),
            self._make_issue(2, "2024-01-02"),
        ]
        result = sort_issues_by_strategy(issues, "lifo")
        assert [i["number"] for i in result] == [2, 1]

    def test_urgent_first(self) -> None:
        issues = [
            self._make_issue(1, "2024-01-01"),
            self._make_issue(2, "2024-01-02", urgent=True),
            self._make_issue(3, "2024-01-03"),
        ]
        result = sort_issues_by_strategy(issues, "fifo")
        # Urgent issue should come first
        assert result[0]["number"] == 2

    def test_unknown_strategy_falls_back_to_fifo(self) -> None:
        issues = [
            self._make_issue(2, "2024-01-02"),
            self._make_issue(1, "2024-01-01"),
        ]
        result = sort_issues_by_strategy(issues, "unknown_strategy")
        assert [i["number"] for i in result] == [1, 2]

    def test_empty_list(self) -> None:
        result = sort_issues_by_strategy([], "fifo")
        assert result == []


# ---------------------------------------------------------------------------
# filter_issues_by_failure_backoff
# ---------------------------------------------------------------------------

class TestFilterIssuesByFailureBackoff:
    def _make_issue(self, number: int) -> dict:
        return {"number": number, "labels": []}

    def test_no_failures_returns_all(self) -> None:
        from loom_tools.common.issue_failures import IssueFailureLog
        log = IssueFailureLog(entries={})
        issues = [self._make_issue(1), self._make_issue(2)]
        result = filter_issues_by_failure_backoff(issues, log, 0)
        assert len(result) == 2

    def test_auto_block_filters_out(self) -> None:
        from loom_tools.common.issue_failures import IssueFailureEntry, IssueFailureLog

        entry = MagicMock()
        entry.should_auto_block = True
        entry.backoff_iterations.return_value = 0

        log = IssueFailureLog(entries={"1": entry})
        issues = [self._make_issue(1), self._make_issue(2)]
        result = filter_issues_by_failure_backoff(issues, log, 0)
        # Issue 1 is auto-blocked, only issue 2 passes
        assert len(result) == 1
        assert result[0]["number"] == 2

    def test_issue_without_number_passes_through(self) -> None:
        from loom_tools.common.issue_failures import IssueFailureLog
        log = IssueFailureLog(entries={})
        issues = [{"labels": []}]  # no "number" key
        result = filter_issues_by_failure_backoff(issues, log, 0)
        assert len(result) == 1


# ---------------------------------------------------------------------------
# detect_orphaned_prs
# ---------------------------------------------------------------------------

class TestDetectOrphanedPRs:
    def test_no_tracked_prs(self) -> None:
        review_requested = [{"number": 10}, {"number": 11}]
        changes_requested = [{"number": 12}]
        orphaned = detect_orphaned_prs(review_requested, changes_requested)
        assert len(orphaned) == 3

    def test_tracked_prs_excluded(self) -> None:
        review_requested = [{"number": 10}, {"number": 11}]
        changes_requested = [{"number": 12}]
        orphaned = detect_orphaned_prs(
            review_requested, changes_requested, tracked_pr_numbers={10, 12}
        )
        # Only #11 is orphaned
        assert len(orphaned) == 1
        assert orphaned[0].pr_number == 11
        assert orphaned[0].needed_role == "judge"

    def test_sorted_by_pr_number(self) -> None:
        review_requested = [{"number": 20}, {"number": 5}]
        orphaned = detect_orphaned_prs(review_requested, [])
        assert [o.pr_number for o in orphaned] == [5, 20]

    def test_merge_conflicted_needs_doctor(self) -> None:
        merge_conflicted = [{"number": 99}]
        orphaned = detect_orphaned_prs([], [], merge_conflicted=merge_conflicted)
        assert len(orphaned) == 1
        assert orphaned[0].needed_role == "doctor"

    def test_empty_inputs(self) -> None:
        orphaned = detect_orphaned_prs([], [])
        assert orphaned == []


# ---------------------------------------------------------------------------
# detect_contradictory_labels
# ---------------------------------------------------------------------------

class TestDetectContradictoryLabels:
    def test_no_conflicts(self) -> None:
        pipeline = {
            "review_requested": [{"number": 1}],
            "changes_requested": [{"number": 2}],
            "ready_to_merge": [{"number": 3}],
            "ready_issues": [{"number": 10}],
            "building_issues": [{"number": 11}],
            "blocked_issues": [{"number": 12}],
        }
        conflicts = detect_contradictory_labels(pipeline)
        assert conflicts == []

    def test_pr_conflict(self) -> None:
        pipeline = {
            "review_requested": [{"number": 5}],
            "changes_requested": [{"number": 5}],  # same PR in both!
            "ready_to_merge": [],
            "ready_issues": [],
            "building_issues": [],
            "blocked_issues": [],
        }
        conflicts = detect_contradictory_labels(pipeline)
        pr_conflicts = [c for c in conflicts if c["entity_type"] == "pr"]
        assert len(pr_conflicts) == 1
        assert 5 == pr_conflicts[0]["number"]
        assert "loom:changes-requested" in pr_conflicts[0]["conflicting_labels"]
        assert "loom:review-requested" in pr_conflicts[0]["conflicting_labels"]

    def test_issue_conflict(self) -> None:
        pipeline = {
            "ready_issues": [{"number": 42}],
            "building_issues": [{"number": 42}],  # same issue in both!
            "blocked_issues": [],
            "review_requested": [],
            "changes_requested": [],
            "ready_to_merge": [],
        }
        conflicts = detect_contradictory_labels(pipeline)
        issue_conflicts = [c for c in conflicts if c["entity_type"] == "issue"]
        assert len(issue_conflicts) == 1
        assert 42 == issue_conflicts[0]["number"]

    def test_empty_pipeline(self) -> None:
        conflicts = detect_contradictory_labels({})
        assert conflicts == []


# ---------------------------------------------------------------------------
# collect_pipeline_data (mock test)
# ---------------------------------------------------------------------------

class TestCollectPipelineData:
    """Smoke test that collect_pipeline_data calls gh_parallel_queries."""

    def test_returns_expected_keys(self, tmp_path: pathlib.Path) -> None:
        """Mocks gh queries and checks the returned dict has the right keys."""
        # Build a fake results list: 10 queries, each returning []
        fake_results = [[] for _ in range(10)]

        with (
            patch("loom_tools.forge_snapshot.gh_parallel_queries", return_value=fake_results),
            patch("loom_tools.forge_snapshot._collect_usage", return_value={"error": "no data"}),
        ):
            # ci_health_check_enabled=False avoids needing a forge/gh mock
            result = collect_pipeline_data(tmp_path, ci_health_check_enabled=False)

        expected_keys = {
            "ready_issues", "building_issues", "architect_proposals",
            "hermit_proposals", "curated_issues", "blocked_issues",
            "review_requested", "changes_requested", "ready_to_merge",
            "uncurated_issues", "usage", "ci_status",
        }
        assert set(result.keys()) == expected_keys

    def test_curated_filtered(self, tmp_path: pathlib.Path) -> None:
        """Curated issues also labeled loom:building are excluded."""
        fake_results = [
            [],  # 0: ready issues
            [],  # 1: building issues
            [],  # 2: architect
            [],  # 3: hermit
            # 4: curated — one has loom:building, one doesn't
            [
                {"number": 1, "labels": [{"name": "loom:curated"}, {"name": "loom:building"}]},
                {"number": 2, "labels": [{"name": "loom:curated"}]},
            ],
            [],  # 5: blocked
            [],  # 6: review-requested
            [],  # 7: changes-requested
            [],  # 8: ready-to-merge
            [],  # 9: all open issues
        ]
        with (
            patch("loom_tools.forge_snapshot.gh_parallel_queries", return_value=fake_results),
            patch("loom_tools.forge_snapshot._collect_usage", return_value={}),
        ):
            result = collect_pipeline_data(tmp_path, ci_health_check_enabled=False)

        # Only issue #2 should be in curated_issues (not #1 which has loom:building)
        curated = result["curated_issues"]
        assert len(curated) == 1
        assert curated[0]["number"] == 2


# ---------------------------------------------------------------------------
# detect_spinning_prs (light unit tests — actual gh call is mocked)
# ---------------------------------------------------------------------------

class TestDetectSpinningPRs:
    def test_empty_returns_empty(self) -> None:
        result = detect_spinning_prs([])
        assert result == []

    def test_no_numbers_returns_empty(self) -> None:
        result = detect_spinning_prs([{"title": "no number"}])
        assert result == []

    def test_below_threshold_not_spinning(self) -> None:
        with patch("loom_tools.forge_snapshot._count_review_rounds", return_value=2):
            result = detect_spinning_prs([{"number": 1}], threshold=3)
        assert result == []

    def test_at_threshold_is_spinning(self) -> None:
        with (
            patch("loom_tools.forge_snapshot._count_review_rounds", return_value=3),
            patch("loom_tools.forge_snapshot._extract_linked_issue", return_value=None),
        ):
            result = detect_spinning_prs([{"number": 42}], threshold=3)
        assert len(result) == 1
        assert result[0].pr_number == 42
        assert result[0].review_cycles == 3
