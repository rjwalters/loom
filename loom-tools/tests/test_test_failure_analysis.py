"""Tests for test failure analysis module."""

from __future__ import annotations

import json
from unittest.mock import patch

import pytest

from loom_tools.models.progress import Milestone, ShepherdProgress
from loom_tools.test_failure_analysis import (
    CATEGORY_BUILDER_BUG,
    CATEGORY_ENVIRONMENT,
    CATEGORY_PRE_EXISTING,
    CATEGORY_UNKNOWN,
    AnalysisSummary,
    FailureCategorization,
    analyze_progress_files,
    categorize_failure,
    format_categorize_text,
    format_doctor_text,
    format_json,
    format_summary_text,
    main,
)


# ---------------------------------------------------------------------------
# Test data helpers
# ---------------------------------------------------------------------------


def _milestone(event: str, **data: object) -> Milestone:
    return Milestone(event=event, timestamp="2026-02-01T10:00:00Z", data=data)


def _blocked_progress(
    task_id: str = "abc1234",
    issue: int = 42,
    milestones: list[Milestone] | None = None,
) -> ShepherdProgress:
    return ShepherdProgress(
        task_id=task_id,
        issue=issue,
        mode="force-merge",
        started_at="2026-02-01T10:00:00Z",
        current_phase="builder",
        last_heartbeat="2026-02-01T10:10:00Z",
        status="blocked",
        milestones=milestones or [],
    )


def _completed_progress(
    task_id: str = "def5678",
    issue: int = 43,
) -> ShepherdProgress:
    return ShepherdProgress(
        task_id=task_id,
        issue=issue,
        mode="force-merge",
        started_at="2026-02-01T10:00:00Z",
        current_phase=None,
        last_heartbeat="2026-02-01T10:10:00Z",
        status="completed",
        milestones=[
            _milestone("started", issue=issue, mode="force-merge"),
            _milestone("phase_completed", phase="builder", duration_seconds=300, status="success"),
            _milestone("completed", pr_merged=True),
        ],
    )


# ---------------------------------------------------------------------------
# Categorization tests
# ---------------------------------------------------------------------------


class TestCategorizeFailure:
    """Tests for individual failure categorization."""

    def test_pre_existing_via_heartbeat(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("started", issue=42, mode="force-merge"),
            _milestone("phase_entered", phase="builder"),
            _milestone("heartbeat", action="tests have pre-existing failures (82s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 1)"),
            _milestone("phase_completed", phase="builder", duration_seconds=300, status="test_failure"),
        ])
        cat = categorize_failure(progress)
        assert cat.category == CATEGORY_PRE_EXISTING
        assert cat.has_pre_existing_signal is True
        assert cat.exit_code == 1
        assert cat.test_command == "pnpm check:ci:lite"

    def test_pre_existing_via_doctor_skip(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("started", issue=42, mode="force-merge"),
            _milestone("phase_entered", phase="builder"),
            _milestone("heartbeat", action="test verification failed (57s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 101)"),
            _milestone("phase_completed", phase="builder", duration_seconds=300, status="test_failure"),
            _milestone("phase_completed", phase="doctor_testfix", duration_seconds=0, status="skipped_unrelated"),
        ])
        cat = categorize_failure(progress)
        assert cat.category == CATEGORY_PRE_EXISTING
        assert cat.has_doctor_skip is True
        assert cat.doctor_outcome == "skipped_unrelated"

    def test_environment_issue_fast_failure(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("started", issue=42, mode="force-merge"),
            _milestone("phase_entered", phase="builder"),
            _milestone("heartbeat", action="test verification failed (1s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 1)"),
            _milestone("phase_completed", phase="builder", duration_seconds=200, status="test_failure"),
        ])
        cat = categorize_failure(progress)
        assert cat.category == CATEGORY_ENVIRONMENT
        assert cat.test_duration_seconds == 1

    def test_builder_bug_doctor_attempted(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("started", issue=42, mode="force-merge"),
            _milestone("phase_entered", phase="builder"),
            _milestone("heartbeat", action="verifying tests: pnpm check:ci:lite"),
            _milestone("heartbeat", action="test verification failed (44s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 101)"),
            _milestone("phase_completed", phase="builder", duration_seconds=300, status="test_failure"),
            _milestone("heartbeat", action="doctor running (1m elapsed)"),
            _milestone("heartbeat", action="doctor running (2m elapsed)"),
            _milestone("heartbeat", action="verifying tests: pnpm check:ci:lite"),
            _milestone("heartbeat", action="test verification failed (44s)"),
        ])
        cat = categorize_failure(progress)
        assert cat.category == CATEGORY_BUILDER_BUG
        assert cat.has_doctor_attempted is True
        assert cat.doctor_outcome == "attempted"

    def test_builder_bug_via_post_blocked_retry(self) -> None:
        """Doctor retry detected via second test verification after blocked event."""
        progress = _blocked_progress(milestones=[
            _milestone("started", issue=42, mode="force-merge"),
            _milestone("phase_entered", phase="builder"),
            _milestone("heartbeat", action="verifying tests: pnpm check:ci:lite"),
            _milestone("heartbeat", action="test verification failed (45s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 1)"),
            _milestone("phase_completed", phase="builder", duration_seconds=284, status="test_failure"),
            # Post-blocked: Doctor retried without explicit doctor heartbeats
            _milestone("heartbeat", action="verifying tests: pnpm check:ci:lite"),
            _milestone("heartbeat", action="test verification failed (45s)"),
        ])
        cat = categorize_failure(progress)
        assert cat.category == CATEGORY_BUILDER_BUG
        assert cat.has_doctor_attempted is True

    def test_unknown_no_signals(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("started", issue=42, mode="force-merge"),
            _milestone("phase_entered", phase="builder"),
            _milestone("heartbeat", action="verifying tests: pnpm check:ci:lite"),
            _milestone("heartbeat", action="test verification failed (50s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 101)"),
            _milestone("phase_completed", phase="builder", duration_seconds=149, status="test_failure"),
        ])
        cat = categorize_failure(progress)
        assert cat.category == CATEGORY_UNKNOWN

    def test_exit_code_parsing(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 101)"),
        ])
        cat = categorize_failure(progress)
        assert cat.exit_code == 101
        assert cat.test_command == "pnpm check:ci:lite"

    def test_test_duration_parsing(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("heartbeat", action="test verification failed (57s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 101)"),
        ])
        cat = categorize_failure(progress)
        assert cat.test_duration_seconds == 57

    def test_builder_duration_extraction(self) -> None:
        progress = _blocked_progress(milestones=[
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 1)"),
            _milestone("phase_completed", phase="builder", duration_seconds=334, status="test_failure"),
        ])
        cat = categorize_failure(progress)
        assert cat.builder_duration_seconds == 334


# ---------------------------------------------------------------------------
# Analysis summary tests
# ---------------------------------------------------------------------------


class TestAnalyzeProgressFiles:
    """Tests for overall analysis."""

    def test_empty_input(self) -> None:
        summary = analyze_progress_files([])
        assert summary.total_runs == 0
        assert summary.blocked_runs == 0
        assert summary.block_rate_percent == 0.0

    def test_all_completed(self) -> None:
        files = [_completed_progress(task_id=f"abc{i:04d}", issue=i) for i in range(10)]
        summary = analyze_progress_files(files)
        assert summary.total_runs == 10
        assert summary.completed_runs == 10
        assert summary.blocked_runs == 0
        assert summary.block_rate_percent == 0.0

    def test_mixed_runs(self) -> None:
        completed = [_completed_progress(task_id=f"abc{i:04d}", issue=i) for i in range(3)]
        blocked = _blocked_progress(task_id="blk0001", issue=100, milestones=[
            _milestone("heartbeat", action="tests have pre-existing failures"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 1)"),
            _milestone("phase_completed", phase="builder", duration_seconds=300, status="test_failure"),
        ])
        summary = analyze_progress_files(completed + [blocked])
        assert summary.total_runs == 4
        assert summary.completed_runs == 3
        assert summary.blocked_runs == 1
        assert summary.block_rate_percent == 25.0
        assert CATEGORY_PRE_EXISTING in summary.categories

    def test_blocked_non_test_failure_not_counted(self) -> None:
        """Blocked runs without test_failure reason are not counted."""
        progress = _blocked_progress(milestones=[
            _milestone("blocked", reason="merge_conflict", details="conflict on main"),
        ])
        summary = analyze_progress_files([progress])
        assert summary.blocked_runs == 0

    def test_doctor_metrics(self) -> None:
        blocked_with_skip = _blocked_progress(task_id="skip001", issue=100, milestones=[
            _milestone("heartbeat", action="test verification failed (50s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 101)"),
            _milestone("phase_completed", phase="builder", duration_seconds=300, status="test_failure"),
            _milestone("phase_completed", phase="doctor_testfix", duration_seconds=0, status="skipped_unrelated"),
        ])
        blocked_with_attempt = _blocked_progress(task_id="att0001", issue=101, milestones=[
            _milestone("heartbeat", action="test verification failed (30s)"),
            _milestone("blocked", reason="test_failure",
                       details="test verification failed (pnpm check:ci:lite, exit code 1)"),
            _milestone("phase_completed", phase="builder", duration_seconds=200, status="test_failure"),
            _milestone("heartbeat", action="doctor running (1m elapsed)"),
        ])
        summary = analyze_progress_files([blocked_with_skip, blocked_with_attempt])
        assert summary.doctor.total_invocations == 2
        assert summary.doctor.skipped_unrelated == 1
        assert summary.doctor.attempted == 1


# ---------------------------------------------------------------------------
# Formatting tests
# ---------------------------------------------------------------------------


class TestFormatting:
    """Tests for output formatting functions."""

    def test_format_summary_text(self) -> None:
        summary = AnalysisSummary(
            total_runs=10,
            completed_runs=8,
            blocked_runs=2,
            block_rate_percent=20.0,
            categories={CATEGORY_PRE_EXISTING: 1, CATEGORY_BUILDER_BUG: 1},
        )
        text = format_summary_text(summary)
        assert "Total shepherd runs:  10" in text
        assert "Blocked (test fail):  2" in text
        assert "20.0%" in text

    def test_format_categorize_text(self) -> None:
        failures = [
            FailureCategorization(
                task_id="abc1234",
                issue=42,
                category=CATEGORY_PRE_EXISTING,
                doctor_outcome="skipped_unrelated",
                test_command="pnpm check:ci:lite",
                exit_code=101,
                details="Doctor skipped",
            )
        ]
        text = format_categorize_text(failures)
        assert "#42" in text
        assert "abc1234" in text
        assert "pnpm check:ci:lite" in text

    def test_format_doctor_text_no_invocations(self) -> None:
        summary = AnalysisSummary()
        text = format_doctor_text(summary)
        assert "No Doctor invocations" in text

    def test_format_doctor_text_with_data(self) -> None:
        summary = AnalysisSummary()
        summary.doctor.total_invocations = 5
        summary.doctor.skipped_unrelated = 2
        summary.doctor.attempted = 3
        summary.doctor.failed = 3
        text = format_doctor_text(summary)
        assert "Total invocations:    5" in text
        assert "Skipped (unrelated):  2" in text

    def test_format_json(self) -> None:
        summary = AnalysisSummary(
            total_runs=5,
            completed_runs=4,
            blocked_runs=1,
            block_rate_percent=20.0,
        )
        output = format_json(summary)
        data = json.loads(output)
        assert data["total_runs"] == 5
        assert data["blocked_runs"] == 1
        assert data["block_rate_percent"] == 20.0
        assert "failures" in data
        assert "doctor" in data


# ---------------------------------------------------------------------------
# CLI tests
# ---------------------------------------------------------------------------


class TestCLI:
    """Tests for CLI entry point."""

    @patch("loom_tools.test_failure_analysis.find_repo_root")
    @patch("loom_tools.test_failure_analysis.read_progress_files")
    def test_summary_command(self, mock_read: object, mock_root: object) -> None:
        mock_root.return_value = "/fake/repo"
        mock_read.return_value = [_completed_progress()]
        assert main(["summary"]) == 0

    @patch("loom_tools.test_failure_analysis.find_repo_root")
    @patch("loom_tools.test_failure_analysis.read_progress_files")
    def test_json_format(self, mock_read: object, mock_root: object, capsys: pytest.CaptureFixture) -> None:
        mock_root.return_value = "/fake/repo"
        mock_read.return_value = [_completed_progress()]
        assert main(["--format", "json"]) == 0
        output = capsys.readouterr().out
        data = json.loads(output)
        assert "total_runs" in data

    @patch("loom_tools.test_failure_analysis.find_repo_root")
    @patch("loom_tools.test_failure_analysis.read_progress_files")
    def test_no_progress_files(self, mock_read: object, mock_root: object) -> None:
        mock_root.return_value = "/fake/repo"
        mock_read.return_value = []
        assert main([]) == 0

    @patch("loom_tools.test_failure_analysis.find_repo_root")
    def test_no_repo_root(self, mock_root: object) -> None:
        mock_root.return_value = None
        assert main([]) == 1

    @patch("loom_tools.test_failure_analysis.find_repo_root")
    @patch("loom_tools.test_failure_analysis.read_progress_files")
    def test_categorize_command(self, mock_read: object, mock_root: object) -> None:
        mock_root.return_value = "/fake/repo"
        mock_read.return_value = [
            _blocked_progress(milestones=[
                _milestone("heartbeat", action="test verification failed (57s)"),
                _milestone("blocked", reason="test_failure",
                           details="test verification failed (pnpm check:ci:lite, exit code 101)"),
                _milestone("phase_completed", phase="builder", duration_seconds=300, status="test_failure"),
                _milestone("phase_completed", phase="doctor_testfix", duration_seconds=0, status="skipped_unrelated"),
            ])
        ]
        assert main(["categorize"]) == 0

    @patch("loom_tools.test_failure_analysis.find_repo_root")
    @patch("loom_tools.test_failure_analysis.read_progress_files")
    def test_doctor_command(self, mock_read: object, mock_root: object) -> None:
        mock_root.return_value = "/fake/repo"
        mock_read.return_value = [_completed_progress()]
        assert main(["doctor"]) == 0
