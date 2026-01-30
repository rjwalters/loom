"""Tests for the daemon_diagnostic module."""

from __future__ import annotations

import json
import pathlib
import tempfile

import pytest

from loom_tools.daemon_diagnostic import (
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
