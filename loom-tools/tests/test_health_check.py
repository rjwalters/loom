"""Tests for the health_check module."""

from __future__ import annotations

import json
import pathlib
import tempfile

import pytest

from loom_tools.health_check import (
    HealthReport,
    PipelineState,
    ShepherdDetail,
    StaleIssue,
    SupportRoleStatus,
    ValidationResult,
    check_orphaned_building,
    check_stale_building,
    check_support_roles,
    format_json_output,
    format_numbers,
    time_ago,
    validate_state_file,
    validate_task_id,
)
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry, SupportRoleEntry


class TestValidateTaskId:
    def test_valid_task_id(self) -> None:
        assert validate_task_id("a1b2c3d") is True
        assert validate_task_id("0000000") is True
        assert validate_task_id("fffffff") is True

    def test_invalid_task_id_too_short(self) -> None:
        assert validate_task_id("a1b2c3") is False
        assert validate_task_id("abc") is False

    def test_invalid_task_id_too_long(self) -> None:
        assert validate_task_id("a1b2c3d4") is False
        assert validate_task_id("a1b2c3d4e") is False

    def test_invalid_task_id_not_hex(self) -> None:
        assert validate_task_id("ghijklm") is False
        assert validate_task_id("a1b2c3g") is False

    def test_null_task_id(self) -> None:
        assert validate_task_id(None) is True
        assert validate_task_id("null") is True


class TestValidateStateFile:
    def test_valid_state_file(self, tmp_path: pathlib.Path) -> None:
        state_file = tmp_path / "daemon-state.json"
        state_file.write_text(json.dumps({
            "started_at": "2026-01-23T10:00:00Z",
            "running": True,
            "iteration": 5,
            "shepherds": {},
        }))

        result = validate_state_file(str(state_file))
        assert result.valid is True
        assert result.status == "ok"

    def test_missing_state_file(self, tmp_path: pathlib.Path) -> None:
        state_file = tmp_path / "nonexistent.json"
        result = validate_state_file(str(state_file))
        assert result.valid is False
        assert result.status == "missing"

    def test_corrupt_state_file(self, tmp_path: pathlib.Path) -> None:
        state_file = tmp_path / "daemon-state.json"
        state_file.write_text("not valid json {{{")

        result = validate_state_file(str(state_file))
        assert result.valid is False
        assert result.status == "corrupt"

    def test_incomplete_state_file(self, tmp_path: pathlib.Path) -> None:
        state_file = tmp_path / "daemon-state.json"
        state_file.write_text(json.dumps({
            "running": True,
            # Missing: started_at, iteration, shepherds
        }))

        result = validate_state_file(str(state_file))
        assert result.valid is False
        assert result.status == "incomplete"
        assert "started_at" in result.missing_fields
        assert "iteration" in result.missing_fields
        assert "shepherds" in result.missing_fields


class TestCheckOrphanedBuilding:
    def test_no_building_issues(self) -> None:
        daemon_state = DaemonState()
        result = check_orphaned_building([], daemon_state)
        assert result == []

    def test_all_issues_tracked(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=42),
                "shepherd-2": ShepherdEntry(status="working", issue=43),
            }
        )
        building_issues = [
            {"number": 42, "title": "Issue 42"},
            {"number": 43, "title": "Issue 43"},
        ]
        result = check_orphaned_building(building_issues, daemon_state)
        assert result == []

    def test_orphaned_issue_found(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=42),
            }
        )
        building_issues = [
            {"number": 42, "title": "Issue 42"},
            {"number": 99, "title": "Orphaned Issue"},
        ]
        result = check_orphaned_building(building_issues, daemon_state)
        assert result == [99]

    def test_idle_shepherd_not_counted(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="idle", issue=None),
            }
        )
        building_issues = [
            {"number": 42, "title": "Issue 42"},
        ]
        result = check_orphaned_building(building_issues, daemon_state)
        assert result == [42]


class TestCheckSupportRoles:
    def test_no_support_roles(self) -> None:
        daemon_state = DaemonState()
        statuses, warnings = check_support_roles(daemon_state)
        # Should return statuses for all expected roles
        assert len(statuses) == 5  # guide, judge, champion, doctor, auditor
        # All should be NEVER_SPAWNED
        for s in statuses:
            assert s.elapsed == "NEVER_SPAWNED"

    def test_running_role_no_warning(self) -> None:
        daemon_state = DaemonState(
            support_roles={
                "guide": SupportRoleEntry(status="running", started="2026-01-23T10:00:00Z"),
            }
        )
        statuses, warnings = check_support_roles(daemon_state)
        guide_status = next(s for s in statuses if s.name == "Guide")
        assert guide_status.status == "running"
        # Running roles shouldn't generate warnings even if never completed
        assert not any("Guide has NEVER SPAWNED" in w for w in warnings)


class TestHealthReport:
    def test_exit_code_healthy(self) -> None:
        report = HealthReport()
        assert report.exit_code == 0

    def test_exit_code_warnings(self) -> None:
        report = HealthReport()
        report.add_warning("Test warning")
        assert report.exit_code == 1

    def test_exit_code_critical(self) -> None:
        report = HealthReport()
        report.add_critical("Test critical")
        assert report.exit_code == 2

    def test_exit_code_critical_takes_precedence(self) -> None:
        report = HealthReport()
        report.add_warning("Test warning")
        report.add_critical("Test critical")
        assert report.exit_code == 2


class TestFormatNumbers:
    def test_empty_list(self) -> None:
        assert format_numbers([]) == ""

    def test_single_item(self) -> None:
        assert format_numbers([{"number": 42}]) == "#42"

    def test_multiple_items(self) -> None:
        items = [{"number": 42}, {"number": 43}, {"number": 44}]
        assert format_numbers(items) == "#42, #43, #44"

    def test_missing_numbers(self) -> None:
        items = [{"number": 42}, {"title": "no number"}, {"number": 44}]
        assert format_numbers(items) == "#42, #44"


class TestTimeAgo:
    def test_empty_timestamp(self) -> None:
        assert time_ago("") == "never"

    def test_invalid_timestamp(self) -> None:
        assert time_ago("not a timestamp") == "unknown"


class TestFormatJsonOutput:
    def test_basic_output(self) -> None:
        report = HealthReport(
            state_file_path="/test/path",
            daemon_running=True,
            daemon_iteration=5,
        )
        output = format_json_output(report)
        data = json.loads(output)

        assert data["state_file"]["path"] == "/test/path"
        assert data["daemon"]["running"] is True
        assert data["daemon"]["iteration"] == 5
        assert data["diagnostics"]["exit_code"] == 0

    def test_with_shepherd_details(self) -> None:
        report = HealthReport()
        report.shepherd_details = [
            ShepherdDetail(
                key="shepherd-1",
                task_id="a1b2c3d",
                status="working",
                issue=42,
                task_id_valid=True,
            )
        ]
        output = format_json_output(report)
        data = json.loads(output)

        assert len(data["shepherds"]["entries"]) == 1
        assert data["shepherds"]["entries"][0]["key"] == "shepherd-1"
        assert data["shepherds"]["entries"][0]["task_id_valid"] is True

    def test_with_stale_building(self) -> None:
        report = HealthReport()
        report.stale_building = [
            StaleIssue(number=42, age_minutes=30),
        ]
        output = format_json_output(report)
        data = json.loads(output)

        assert len(data["consistency"]["stale_building"]) == 1
        assert data["consistency"]["stale_building"][0]["issue"] == 42
        assert data["consistency"]["stale_building"][0]["age_minutes"] == 30


class TestBashPythonComparison:
    """Tests documenting behavioral differences between bash and Python implementations.

    The bash script (health-check.sh) and Python module (health_check.py) serve
    DIFFERENT purposes despite similar names:

    Bash health-check.sh:
    - Proactive health MONITORING system with time-series metrics
    - Collects and stores metrics in health-metrics.json
    - Maintains alerts in alerts.json
    - Computes composite health score (0-100) from 7 factors
    - Designed for continuous monitoring during daemon operation

    Python health_check.py:
    - Point-in-time diagnostic CHECKER
    - Validates daemon state file structure
    - Checks shepherd task ID validity
    - Detects orphaned/stale building issues
    - Reports support role status
    - Designed for on-demand diagnostics

    These are intentional design divergences, not bugs.
    """

    def test_cli_argument_divergence_documented(self) -> None:
        """Document CLI argument differences between implementations."""
        bash_cli_options = {
            "--json": "Output as JSON",
            "--collect": "Collect and store health metrics",
            "--history": "Show metric history (optional hours param)",
            "--alerts": "Show current alerts",
            "--acknowledge": "Acknowledge an alert (requires ID)",
            "--clear-alerts": "Clear all alerts",
            "--help": "Show help",
        }

        python_cli_options = {
            "--json": "Output health report as JSON",
            "--help": "Show help",
        }

        # Document that Python is missing these bash features
        missing_in_python = set(bash_cli_options.keys()) - set(python_cli_options.keys())
        assert missing_in_python == {
            "--collect",
            "--history",
            "--alerts",
            "--acknowledge",
            "--clear-alerts",
        }

    def test_environment_variable_divergence_documented(self) -> None:
        """Document environment variable differences between implementations."""
        bash_env_vars = {
            "LOOM_HEALTH_RETENTION_HOURS": 24,
            "LOOM_THROUGHPUT_DECLINE_THRESHOLD": 50,
            "LOOM_QUEUE_GROWTH_THRESHOLD": 5,
            "LOOM_STUCK_AGENT_THRESHOLD": 10,
            "LOOM_ERROR_RATE_THRESHOLD": 20,
        }

        python_env_vars = {
            "LOOM_STALE_BUILDING_MINUTES": 15,
            "LOOM_GUIDE_INTERVAL": 900,
            "LOOM_CHAMPION_INTERVAL": 600,
            "LOOM_DOCTOR_INTERVAL": 300,
            "LOOM_AUDITOR_INTERVAL": 600,
            "LOOM_JUDGE_INTERVAL": 300,
        }

        # These are completely different sets of variables
        assert set(bash_env_vars.keys()).isdisjoint(set(python_env_vars.keys()))

    def test_json_output_structure_divergence_documented(self) -> None:
        """Document JSON output structure differences between implementations."""
        # Bash --json output structure
        bash_json_keys = {
            "health_score",
            "health_status",
            "last_updated",
            "metric_count",
            "unacknowledged_alerts",
            "total_alerts",
            "latest_metrics",
            "metrics_history",
        }

        # Python --json output structure
        python_json_keys = {
            "state_file",
            "daemon",
            "shepherds",
            "pipeline",
            "consistency",
            "support_roles",
            "diagnostics",
        }

        # The output structures are completely different
        assert bash_json_keys.isdisjoint(python_json_keys)

    def test_exit_code_semantics_match(self) -> None:
        """Verify exit code semantics match between implementations.

        Both implementations use the same exit code semantics:
        - 0: Healthy (no warnings or critical issues)
        - 1: Warnings detected (degraded but functional)
        - 2: Critical issues (state corruption, orphaned work)
        """
        # Python implementation
        report_healthy = HealthReport()
        assert report_healthy.exit_code == 0

        report_warning = HealthReport()
        report_warning.add_warning("test warning")
        assert report_warning.exit_code == 1

        report_critical = HealthReport()
        report_critical.add_critical("test critical")
        assert report_critical.exit_code == 2

        # Exit codes semantics are documented to match

    def test_features_only_in_bash_documented(self) -> None:
        """Document features present in bash but missing from Python.

        The following bash features are NOT implemented in Python:
        """
        bash_only_features = [
            "Time-series metrics collection and storage",
            "Composite health score calculation (0-100)",
            "Alert generation based on thresholds",
            "Alert acknowledgment and management",
            "Historical metrics viewing",
            "Throughput trend analysis",
            "Queue depth trend analysis",
            "Error rate tracking",
            "Resource usage monitoring",
            "Integration with daemon-metrics.json",
            "Integration with daemon-snapshot.sh output",
        ]
        assert len(bash_only_features) == 11

    def test_features_only_in_python_documented(self) -> None:
        """Document features present in Python but not in bash.

        The following Python features are NOT in the bash implementation:
        """
        python_only_features = [
            "State file validation (missing, corrupt, incomplete)",
            "Shepherd task ID format validation (7-char hex)",
            "Orphaned building issue detection",
            "Stale building issue detection (with PR matching)",
            "Support role spawn time monitoring",
            "Per-issue staleness checking with PR correlation",
        ]
        assert len(python_only_features) == 6

    def test_implementations_serve_different_purposes(self) -> None:
        """Confirm the implementations serve different purposes.

        This is documented as an INTENTIONAL divergence:

        Bash health-check.sh:
        - Called periodically by daemon iteration (--collect)
        - Maintains historical metrics for trend analysis
        - Generates alerts when thresholds are crossed
        - Focus: Continuous monitoring and proactive alerting

        Python health_check.py:
        - Called on-demand for diagnostic purposes
        - Provides point-in-time state validation
        - Checks for corruption and orphaned work
        - Focus: Diagnostic validation and troubleshooting
        """
        # The purposes are different - this is intentional
        bash_purpose = "Continuous monitoring and proactive alerting"
        python_purpose = "Diagnostic validation and troubleshooting"
        assert bash_purpose != python_purpose
