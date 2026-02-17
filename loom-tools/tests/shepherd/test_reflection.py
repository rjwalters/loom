"""Tests for the reflection phase."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import PhaseStatus
from loom_tools.shepherd.phases.reflection import (
    HIGH_RETRY_THRESHOLD,
    HIGH_TEST_FIX_THRESHOLD,
    REFLECTION_ISSUE_LABEL,
    TITLE_PREFIX,
    UPSTREAM_REPO,
    Finding,
    ReflectionPhase,
    RunSummary,
    _extract_error_context,
    _is_recent_duplicate,
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

    def test_no_slow_phase_finding(self, clean_summary: RunSummary) -> None:
        """slow_phase category has been removed — slow phases should not produce findings."""
        clean_summary.phase_durations["Builder"] = 600
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        slow_findings = [f for f in findings if f.category == "slow_phase"]
        assert len(slow_findings) == 0

    def test_no_missing_baseline_finding(self, clean_summary: RunSummary) -> None:
        """missing_baseline category has been removed."""
        clean_summary.warnings = ["No baseline health cache found"]
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        baseline_findings = [f for f in findings if f.category == "missing_baseline"]
        assert len(baseline_findings) == 0

    def test_no_stale_artifacts_finding(self, clean_summary: RunSummary) -> None:
        """stale_artifacts category has been removed."""
        clean_summary.warnings = ["Stale branch feature/issue-42 exists on remote"]
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        stale_findings = [f for f in findings if f.category == "stale_artifacts"]
        assert len(stale_findings) == 0

    def test_detects_excessive_judge_retries(self, clean_summary: RunSummary) -> None:
        clean_summary.judge_retries = HIGH_RETRY_THRESHOLD
        clean_summary.exit_code = 1  # retries only flagged on failed runs
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 1
        assert "Judge" in retry_findings[0].title

    def test_detects_excessive_doctor_attempts(
        self, clean_summary: RunSummary
    ) -> None:
        clean_summary.doctor_attempts = HIGH_RETRY_THRESHOLD
        clean_summary.exit_code = 1  # retries only flagged on failed runs
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 1
        assert "Doctor" in retry_findings[0].title

    def test_detects_excessive_test_fix_attempts(
        self, clean_summary: RunSummary
    ) -> None:
        clean_summary.test_fix_attempts = HIGH_TEST_FIX_THRESHOLD
        clean_summary.exit_code = 1  # retries only flagged on failed runs
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 1
        assert "test-fix" in retry_findings[0].title.lower()

    def test_retries_skipped_when_run_succeeded(
        self, clean_summary: RunSummary
    ) -> None:
        """Retries that self-heal (exit_code == 0) are not actionable."""
        clean_summary.judge_retries = HIGH_RETRY_THRESHOLD + 1
        clean_summary.doctor_attempts = HIGH_RETRY_THRESHOLD + 1
        clean_summary.test_fix_attempts = HIGH_TEST_FIX_THRESHOLD + 1
        clean_summary.exit_code = 0  # run succeeded
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 0

    def test_test_fix_at_old_threshold_no_finding(
        self, clean_summary: RunSummary
    ) -> None:
        """Test-fix loop of 2 is routine and should not produce a finding."""
        clean_summary.test_fix_attempts = 2
        clean_summary.exit_code = 1
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 0

    def test_retry_finding_includes_error_context(
        self, clean_summary: RunSummary
    ) -> None:
        """Retry findings include error context extracted from logs."""
        clean_summary.judge_retries = HIGH_RETRY_THRESHOLD
        clean_summary.exit_code = 1
        clean_summary.log_content = "Error: review submission timed out\n"
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        retry_findings = [f for f in findings if f.category == "excessive_retries"]
        assert len(retry_findings) == 1
        assert "Error: review submission timed out" in retry_findings[0].details

    def test_builder_failure_with_diagnostics(self, clean_summary: RunSummary) -> None:
        """builder_failure finding includes extracted error context."""
        clean_summary.exit_code = 1
        clean_summary.log_content = (
            "some output\n"
            "Traceback (most recent call last):\n"
            '  File "foo.py", line 10, in main\n'
            "ImportError: No module named 'missing_module'\n"
            "more output\n"
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        failure_findings = [f for f in findings if f.category == "builder_failure"]
        assert len(failure_findings) == 1
        assert failure_findings[0].severity == "bug"
        assert "ImportError" in failure_findings[0].details

    def test_builder_failure_skipped_without_log_content(
        self, clean_summary: RunSummary
    ) -> None:
        """builder_failure is NOT filed when no error can be extracted."""
        clean_summary.exit_code = 1
        clean_summary.log_content = ""
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        failure_findings = [f for f in findings if f.category == "builder_failure"]
        assert len(failure_findings) == 0

    def test_builder_failure_skipped_with_unparseable_log(
        self, clean_summary: RunSummary
    ) -> None:
        """builder_failure is NOT filed when log has no actionable errors."""
        clean_summary.exit_code = 1
        clean_summary.log_content = "INFO: Starting build\nINFO: Build complete\n"
        phase = ReflectionPhase(run_summary=clean_summary)
        findings = phase._analyze_run(clean_summary)
        failure_findings = [f for f in findings if f.category == "builder_failure"]
        assert len(failure_findings) == 0


class TestErrorExtraction:
    """Test _extract_error_context helper."""

    def test_empty_log(self) -> None:
        assert _extract_error_context("") == ""

    def test_python_traceback(self) -> None:
        log = (
            "some preamble\n"
            "Traceback (most recent call last):\n"
            '  File "foo.py", line 1\n'
            "ImportError: no module\n"
            "trailing text\n"
        )
        result = _extract_error_context(log)
        assert "Traceback" in result
        assert "ImportError" in result

    def test_rust_error(self) -> None:
        log = "error[E0277]: the trait bound is not satisfied\n  --> src/main.rs:5\n\n"
        result = _extract_error_context(log)
        assert "error[E0277]" in result

    def test_typescript_error(self) -> None:
        log = "src/app.ts(5,3): error TS2304: Cannot find name 'foo'.\n"
        result = _extract_error_context(log)
        assert "error TS2304" in result

    def test_git_fatal(self) -> None:
        log = "fatal: not a git repository (or any parent up to mount point /)\n"
        result = _extract_error_context(log)
        assert "fatal:" in result

    def test_generic_error(self) -> None:
        log = "Error: something went wrong\n"
        result = _extract_error_context(log)
        assert "Error:" in result

    def test_no_error_pattern(self) -> None:
        log = "INFO: all good\nDEBUG: done\n"
        assert _extract_error_context(log) == ""

    def test_truncation(self) -> None:
        log = "Traceback (most recent call last):\n" + "x" * 2000 + "\nValueError: big\n"
        result = _extract_error_context(log)
        assert "truncated" in result


class TestRecursiveReflectionGuard:
    """Test that reflection doesn't file issues about reflection issues."""

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_skips_when_source_is_reflection_issue(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        clean_summary.issue_title = "[shepherd-reflection] Builder failed to create PR"
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
            severity="bug",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is False
        # subprocess.run should never be called (skipped before search)
        mock_run.assert_not_called()

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_does_not_skip_normal_issue(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        clean_summary.issue_title = "Normal issue title"
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([]),
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is True


class TestDuplicateDetection:
    """Test duplicate issue detection with stable titles."""

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_skips_when_open_duplicate_exists(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([{
                "number": 100,
                "title": "[shepherd-reflection] Builder failed to create PR",
                "closedAt": None,
            }]),
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is False

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_files_when_recently_closed_duplicate_exists(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        """Closed issues are never duplicates — recurrence should be tracked."""
        recently_closed = (
            datetime.now(timezone.utc) - timedelta(days=1)
        ).isoformat()
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([{
                "number": 100,
                "title": "[shepherd-reflection] Builder failed to create PR",
                "closedAt": recently_closed,
            }]),
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is True

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_files_when_old_closed_duplicate(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        """Closed issues are never duplicates — OK to refile."""
        old_closed = (
            datetime.now(timezone.utc) - timedelta(days=30)
        ).isoformat()
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([{
                "number": 100,
                "title": "[shepherd-reflection] Builder failed to create PR",
                "closedAt": old_closed,
            }]),
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is True

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
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        assert phase._should_file_issue(finding, mock_context) is True

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_searches_all_states(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        """Verify the gh issue list call uses --state all."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([]),
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        phase._should_file_issue(finding, mock_context)

        call_args = mock_run.call_args[0][0]
        state_idx = call_args.index("--state")
        assert call_args[state_idx + 1] == "all"

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_searches_by_stable_title(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        """Verify the search query uses the stable title, not category."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([]),
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        phase._should_file_issue(finding, mock_context)

        call_args = mock_run.call_args[0][0]
        search_idx = call_args.index("--search")
        search_query = call_args[search_idx + 1]
        assert search_query == f"{TITLE_PREFIX} Builder failed to create PR"


class TestIsRecentDuplicate:
    """Test _is_recent_duplicate helper directly."""

    def test_open_issue_is_duplicate(self) -> None:
        issue = {
            "title": "[shepherd-reflection] Builder failed to create PR",
            "closedAt": None,
        }
        assert _is_recent_duplicate(issue, "[shepherd-reflection] Builder failed to create PR") is True

    def test_recently_closed_is_not_duplicate(self) -> None:
        """Closed issues are never duplicates — recurrence should be tracked."""
        closed = (datetime.now(timezone.utc) - timedelta(hours=1)).isoformat()
        issue = {
            "title": "[shepherd-reflection] Builder failed to create PR",
            "closedAt": closed,
        }
        assert _is_recent_duplicate(issue, "[shepherd-reflection] Builder failed to create PR") is False

    def test_old_closed_is_not_duplicate(self) -> None:
        closed = (datetime.now(timezone.utc) - timedelta(days=30)).isoformat()
        issue = {
            "title": "[shepherd-reflection] Builder failed to create PR",
            "closedAt": closed,
        }
        assert _is_recent_duplicate(issue, "[shepherd-reflection] Builder failed to create PR") is False

    def test_unrelated_title_is_not_duplicate(self) -> None:
        issue = {
            "title": "Completely unrelated issue",
            "closedAt": None,
        }
        assert _is_recent_duplicate(issue, "[shepherd-reflection] Builder failed to create PR") is False


class TestStableTitles:
    """Test that filed issues use stable titles without variable data."""

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_title_has_no_variable_suffix(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        """Filed title should be stable (e.g., no '(358s)' suffix)."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="https://github.com/rjwalters/loom/issues/999\n",
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
            severity="bug",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        phase._file_upstream_issue(finding, clean_summary, mock_context)

        call_args = mock_run.call_args[0][0]
        title_idx = call_args.index("--title")
        title = call_args[title_idx + 1]
        assert title == "[shepherd-reflection] Builder failed to create PR"
        # No variable data like durations
        assert "(" not in title
        assert "s)" not in title


class TestReflectionPhaseFileUpstream:
    """Test _file_upstream_issue uses the correct label."""

    @patch("loom_tools.shepherd.phases.reflection.subprocess.run")
    def test_uses_loom_triage_label(
        self,
        mock_run: MagicMock,
        mock_context: MagicMock,
        clean_summary: RunSummary,
    ) -> None:
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="https://github.com/rjwalters/loom/issues/999\n",
        )
        finding = Finding(
            category="builder_failure",
            title="Builder failed to create PR",
            details="details",
            severity="bug",
        )
        phase = ReflectionPhase(run_summary=clean_summary)
        result = phase._file_upstream_issue(finding, clean_summary, mock_context)
        assert result is True
        # Verify the gh issue create call used loom:triage, not the severity
        call_args = mock_run.call_args[0][0]
        label_idx = call_args.index("--label")
        assert call_args[label_idx + 1] == REFLECTION_ISSUE_LABEL


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
        # Builder failure with error context triggers a finding
        clean_summary.exit_code = 1
        clean_summary.log_content = "Error: something broke\n"
        phase = ReflectionPhase(run_summary=clean_summary)
        result = phase.run(mock_context)
        assert result.status == PhaseStatus.SUCCESS
        assert result.data["findings_count"] >= 1
        assert result.data["filed_count"] >= 1

    def test_validate_always_true(self, mock_context: MagicMock) -> None:
        phase = ReflectionPhase()
        assert phase.validate(mock_context) is True
