"""Tests for the reflection phase."""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import PhaseStatus
from loom_tools.shepherd.phases.reflection import (
    HIGH_RETRY_THRESHOLD,
    SLOW_PHASE_THRESHOLD_SECONDS,
    TITLE_PREFIX,
    UPSTREAM_REPO,
    ReflectionPhase,
    RunSummary,
)


@pytest.fixture
def mock_context() -> MagicMock:
    """Create a mock ShepherdContext."""
    ctx = MagicMock(spec=ShepherdContext)
    ctx.config = ShepherdConfig(issue=42)
    ctx.repo_root = Path("/fake/repo")
    ctx.issue_title = "Test issue"
    return ctx


@pytest.fixture
def clean_summary() -> RunSummary:
    """Create a clean run summary with no anomalies."""
    return RunSummary(
        issue=42,
        issue_title="Test issue",
        mode="force-merge",
        task_id="abc1234",
        duration=120,
        exit_code=0,
        phase_durations={"Curator": 30, "Builder": 60, "Judge": 20, "Merge": 10},
        completed_phases=["Curator", "Builder", "Judge", "Merge"],
        judge_retries=0,
        doctor_attempts=0,
        test_fix_attempts=0,
        warnings=[],
    )


class TestReflectionPhaseShouldSkip:
    """Test should_skip logic."""

    def test_skips_when_no_reflect_set(self, mock_context: MagicMock) -> None:
        mock_context.config.no_reflect = True
        phase = ReflectionPhase()
        skip, reason = phase.should_skip(mock_context)
        assert skip is True
        assert "no-reflect" in reason

    def test_does_not_skip_by_default(self, mock_context: MagicMock) -> None:
        phase = ReflectionPhase()
        skip, _ = phase.should_skip(mock_context)
        assert skip is False


class TestReflectionPhaseAnalysis:
    """Test finding detection logic."""

    def test_clean_run_produces_no_findings(self, clean_summary: RunSummary) -> None:
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        assert len(findings) == 0

    def test_detects_slow_phase(self, clean_summary: RunSummary) -> None:
        clean_summary.phase_durations["Builder"] = SLOW_PHASE_THRESHOLD_SECONDS + 100
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        slow_findings = [f for f in findings if f.category == "slow_phase"]
        assert len(slow_findings) == 1
        assert "Builder" in slow_findings[0].title

    def test_detects_excessive_judge_retries(self, clean_summary: RunSummary) -> None:
        clean_summary.judge_retries = HIGH_RETRY_THRESHOLD
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 1
        assert "Judge" in retry_findings[0].title

    def test_detects_excessive_doctor_attempts(
        self, clean_summary: RunSummary
    ) -> None:
        clean_summary.doctor_attempts = HIGH_RETRY_THRESHOLD
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 1
        assert "Doctor" in retry_findings[0].title

    def test_detects_excessive_test_fix_attempts(
        self, clean_summary: RunSummary
    ) -> None:
        clean_summary.test_fix_attempts = HIGH_RETRY_THRESHOLD
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 1
        assert "test-fix" in retry_findings[0].title.lower()

    def test_detects_builder_failure(self, clean_summary: RunSummary) -> None:
        clean_summary.exit_code = 1  # BUILDER_FAILED
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        failure_findings = [f for f in findings if f.category == "builder_failure"]
        assert len(failure_findings) == 1
        assert failure_findings[0].severity == "bug"

    def test_detects_stale_artifacts(self, clean_summary: RunSummary) -> None:
        clean_summary.warnings = ["Stale branch feature/issue-42 exists on remote"]
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        stale_findings = [f for f in findings if f.category == "stale_artifacts"]
        assert len(stale_findings) == 1

    def test_detects_missing_baseline(self, clean_summary: RunSummary) -> None:
        clean_summary.warnings = ["No baseline health cache found"]
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        baseline_findings = [
            f for f in findings if f.category == "missing_baseline"
        ]
        assert len(baseline_findings) == 1

    def test_multiple_findings(self, clean_summary: RunSummary) -> None:
        clean_summary.phase_durations["Judge"] = 600
        clean_summary.judge_retries = 3
        clean_summary.warnings = ["Stale branch exists"]
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        assert len(findings) >= 3


class TestReflectionPhaseDuplicateCheck:
    """Test duplicate issue detection."""

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_skips_when_duplicate_exists(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([{"number": 100, "title": "existing issue"}]),
        )
        from loom_tools.shepherd.phases.reflection import Finding

        finding = Finding(
            category="slow_phase",
            title="Slow Builder phase",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is False

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_files_when_no_duplicate(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([]),
        )
        from loom_tools.shepherd.phases.reflection import Finding

        finding = Finding(
            category="slow_phase",
            title="Slow Builder phase",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is True


class TestReflectionPhaseRun:
    """Test the full run method."""

    @patch.object(ReflectionPhase, "_file_upstream_issue", return_value=False)
    @patch.object(ReflectionPhase, "_should_file_issue", return_value=False)
    def test_clean_run_returns_success(
        self,
        mock_should_file: MagicMock,
        mock_file: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        phase = ReflectionPhase(run_summary=clean_summary)
        result = phase.run(mock_context)
        assert result.status == PhaseStatus.SUCCESS
        assert result.data["findings_count"] == 0

    @patch.object(ReflectionPhase, "_file_upstream_issue", return_value=True)
    @patch.object(ReflectionPhase, "_should_file_issue", return_value=True)
    def test_anomalous_run_files_issues(
        self,
        mock_should_file: MagicMock,
        mock_file: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        clean_summary.exit_code = 1  # Builder failure triggers a finding
        phase = ReflectionPhase(run_summary=clean_summary)
        result = phase.run(mock_context)
        assert result.status == PhaseStatus.SUCCESS
        assert result.data["findings_count"] >= 1
        assert result.data["filed_count"] >= 1

    def test_validate_always_true(self, mock_context: MagicMock) -> None:
        phase = ReflectionPhase()
        assert phase.validate(mock_context) is True
