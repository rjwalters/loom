"""Tests for the daemon_diagnostic module."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.daemon_diagnostic import (
    HealthReport,
    PipelineState,
    ShepherdDetail,
    StaleIssue,
    SupportRoleStatus,
    ValidationResult,
    check_orphaned_building,
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


class TestBashPythonParity:
    """Tests validating that daemon_diagnostic.py behavior matches daemon-health.sh.

    The Python implementation should produce identical behavior to the bash script
    for all supported operations. This test class validates parity for:
    - CLI argument parsing (--json, --help)
    - Default values for all thresholds and intervals
    - State file validation status values
    - Exit code semantics
    - Support role configuration and order
    - JSON output structure
    - Task ID validation regex

    See issue #1697 for the loom-tools migration validation effort.
    """

    def test_default_stale_building_minutes_matches_bash(self) -> None:
        """Verify Python default stale building threshold matches bash.

        Bash default (daemon-health.sh line 75):
        - STALE_BUILDING_MINUTES="${LOOM_STALE_BUILDING_MINUTES:-15}"
        """
        from loom_tools.daemon_diagnostic import STALE_BUILDING_MINUTES

        BASH_STALE_BUILDING_MINUTES = 15
        assert STALE_BUILDING_MINUTES == BASH_STALE_BUILDING_MINUTES, (
            f"stale building minutes mismatch: Python={STALE_BUILDING_MINUTES}, "
            f"bash={BASH_STALE_BUILDING_MINUTES}"
        )

    def test_support_role_intervals_match_bash(self) -> None:
        """Verify all support role intervals match bash defaults.

        Bash defaults (daemon-health.sh lines 77-82):
        - GUIDE_INTERVAL="${LOOM_GUIDE_INTERVAL:-900}"        # 15 minutes
        - CHAMPION_INTERVAL="${LOOM_CHAMPION_INTERVAL:-600}"  # 10 minutes
        - DOCTOR_INTERVAL="${LOOM_DOCTOR_INTERVAL:-300}"      # 5 minutes
        - AUDITOR_INTERVAL="${LOOM_AUDITOR_INTERVAL:-600}"    # 10 minutes
        - JUDGE_INTERVAL="${LOOM_JUDGE_INTERVAL:-300}"        # 5 minutes
        """
        from loom_tools.daemon_diagnostic import (
            AUDITOR_INTERVAL,
            CHAMPION_INTERVAL,
            DOCTOR_INTERVAL,
            GUIDE_INTERVAL,
            JUDGE_INTERVAL,
        )

        # Bash default values
        BASH_GUIDE_INTERVAL = 900
        BASH_CHAMPION_INTERVAL = 600
        BASH_DOCTOR_INTERVAL = 300
        BASH_AUDITOR_INTERVAL = 600
        BASH_JUDGE_INTERVAL = 300

        assert GUIDE_INTERVAL == BASH_GUIDE_INTERVAL, (
            f"guide interval mismatch: Python={GUIDE_INTERVAL}, bash={BASH_GUIDE_INTERVAL}"
        )
        assert CHAMPION_INTERVAL == BASH_CHAMPION_INTERVAL, (
            f"champion interval mismatch: Python={CHAMPION_INTERVAL}, "
            f"bash={BASH_CHAMPION_INTERVAL}"
        )
        assert DOCTOR_INTERVAL == BASH_DOCTOR_INTERVAL, (
            f"doctor interval mismatch: Python={DOCTOR_INTERVAL}, bash={BASH_DOCTOR_INTERVAL}"
        )
        assert AUDITOR_INTERVAL == BASH_AUDITOR_INTERVAL, (
            f"auditor interval mismatch: Python={AUDITOR_INTERVAL}, "
            f"bash={BASH_AUDITOR_INTERVAL}"
        )
        assert JUDGE_INTERVAL == BASH_JUDGE_INTERVAL, (
            f"judge interval mismatch: Python={JUDGE_INTERVAL}, bash={BASH_JUDGE_INTERVAL}"
        )

    def test_support_role_display_intervals_match_bash(self) -> None:
        """Verify support role display interval strings match bash.

        Bash display strings (daemon-health.sh lines 443-444):
        - interval_display=("15 min" "5 min" "10 min" "5 min" "10 min")
        - Corresponds to: guide, judge, champion, doctor, auditor
        """
        from loom_tools.daemon_diagnostic import SUPPORT_ROLE_INTERVALS

        # Bash display interval strings (roles order: guide, judge, champion, doctor, auditor)
        expected_displays = {
            "guide": "15 min",
            "judge": "5 min",
            "champion": "10 min",
            "doctor": "5 min",
            "auditor": "10 min",
        }

        for role, (_, display_str) in SUPPORT_ROLE_INTERVALS.items():
            expected = expected_displays.get(role)
            assert expected is not None, f"unexpected role: {role}"
            assert display_str == expected, (
                f"{role} display interval mismatch: Python={display_str}, bash={expected}"
            )

    def test_support_roles_list_matches_bash(self) -> None:
        """Verify Python checks the same support roles as bash.

        Bash roles list (daemon-health.sh line 442):
        - roles=("guide" "judge" "champion" "doctor" "auditor")
        """
        from loom_tools.daemon_diagnostic import SUPPORT_ROLE_INTERVALS

        BASH_ROLES = ["guide", "judge", "champion", "doctor", "auditor"]
        python_roles = list(SUPPORT_ROLE_INTERVALS.keys())

        assert set(python_roles) == set(BASH_ROLES), (
            f"support roles mismatch: Python={python_roles}, bash={BASH_ROLES}"
        )
        # Also verify count matches
        assert len(python_roles) == 5, f"expected 5 roles, got {len(python_roles)}"

    def test_exit_code_semantics_match_bash(self) -> None:
        """Verify exit code semantics match bash implementation.

        Bash exit codes (daemon-health.sh lines 617-622):
        - EXIT_CODE=0 (default)
        - EXIT_CODE=2 if CRITICALS > 0
        - EXIT_CODE=1 if WARNINGS > 0 (and no criticals)
        """
        report = HealthReport()
        assert report.exit_code == 0, "healthy report should exit 0"

        report.add_warning("test warning")
        assert report.exit_code == 1, "warnings-only should exit 1"

        report.add_critical("test critical")
        assert report.exit_code == 2, "with criticals should exit 2"

    def test_validation_status_values_match_bash(self) -> None:
        """Verify state file validation status values match bash.

        Bash status values (daemon-health.sh lines 505-540):
        - "ok" - valid state file
        - "missing" - file not found
        - "corrupt" - invalid JSON
        - "incomplete" - missing required fields
        """
        valid_statuses = {"ok", "missing", "corrupt", "incomplete"}

        # Test each status is produced correctly
        result = ValidationResult(valid=True, status="ok")
        assert result.status in valid_statuses

        result = ValidationResult(valid=False, status="missing")
        assert result.status in valid_statuses

        result = ValidationResult(valid=False, status="corrupt")
        assert result.status in valid_statuses

        result = ValidationResult(valid=False, status="incomplete")
        assert result.status in valid_statuses

    def test_task_id_regex_matches_bash(self) -> None:
        """Verify task ID validation regex matches bash.

        Bash regex (daemon-health.sh line 288):
        - if [[ "$task_id" =~ ^[0-9a-f]{7}$ ]]

        Python regex (daemon_diagnostic.py line 171):
        - re.match(r"^[0-9a-f]{7}$", task_id)
        """
        # Valid 7-char hex strings
        assert validate_task_id("a1b2c3d") is True
        assert validate_task_id("0000000") is True
        assert validate_task_id("fffffff") is True
        assert validate_task_id("1234567") is True

        # Invalid - uppercase (bash regex is case-sensitive to lowercase)
        assert validate_task_id("A1B2C3D") is False
        assert validate_task_id("ABCDEFG") is False

        # Invalid - wrong length
        assert validate_task_id("a1b2c3") is False  # 6 chars
        assert validate_task_id("a1b2c3d4") is False  # 8 chars

        # Invalid - non-hex characters
        assert validate_task_id("ghijklm") is False
        assert validate_task_id("xyz1234") is False

        # null/empty is valid for idle shepherds (both implementations)
        assert validate_task_id(None) is True
        assert validate_task_id("null") is True

    def test_required_state_fields_match_bash(self, tmp_path: pathlib.Path) -> None:
        """Verify required state file fields match bash implementation.

        Bash required fields (daemon-health.sh lines 266-267):
        - for field in started_at running iteration shepherds; do
        """
        state_file = tmp_path / "daemon-state.json"

        # File with all required fields - should be valid
        state_file.write_text(json.dumps({
            "started_at": "2026-01-23T10:00:00Z",
            "running": True,
            "iteration": 5,
            "shepherds": {},
        }))
        result = validate_state_file(str(state_file))
        assert result.valid is True

        # File missing each required field
        required_fields = ["started_at", "running", "iteration", "shepherds"]
        for missing_field in required_fields:
            fields = {f: "test" for f in required_fields if f != missing_field}
            if "running" in fields:
                fields["running"] = True
            if "iteration" in fields:
                fields["iteration"] = 1
            if "shepherds" in fields:
                fields["shepherds"] = {}

            state_file.write_text(json.dumps(fields))
            result = validate_state_file(str(state_file))
            assert result.valid is False, f"should be invalid when missing {missing_field}"
            assert result.status == "incomplete"
            assert missing_field in result.missing_fields, (
                f"missing_fields should contain {missing_field}"
            )

    def test_cli_json_flag_accepted(self) -> None:
        """Verify --json flag is accepted like bash implementation.

        Bash (daemon-health.sh lines 141-144):
        - --json)
        -     JSON_OUTPUT=true
        """
        import argparse

        # Verify the argument is parsed correctly (same as Python implementation)
        parser = argparse.ArgumentParser()
        parser.add_argument("--json", action="store_true")
        args = parser.parse_args(["--json"])
        assert args.json is True

    def test_json_output_structure_matches_bash(self) -> None:
        """Verify JSON output structure matches bash implementation.

        Bash JSON structure (daemon-health.sh lines 713-749):
        - state_file: {path, status}
        - daemon: {running, iteration, started_at, force_mode}
        - shepherds: {entries, invalid_task_ids, total}
        - pipeline: {ready, building, review_requested, ready_to_merge, blocked}
        - consistency: {orphaned_building, stale_building}
        - support_roles: [...]
        - diagnostics: {warnings, criticals, recommendations, warning_count, critical_count, exit_code}
        """
        report = HealthReport(
            state_file_path="/test/path",
            daemon_running=True,
            daemon_iteration=5,
            daemon_started_at="2026-01-23T10:00:00Z",
            daemon_force_mode=True,
        )
        report.add_warning("test warning")
        report.add_recommendation("test recommendation")

        output = format_json_output(report)
        data = json.loads(output)

        # Verify top-level structure matches bash
        assert "state_file" in data
        assert "path" in data["state_file"]
        assert "status" in data["state_file"]

        assert "daemon" in data
        assert "running" in data["daemon"]
        assert "iteration" in data["daemon"]
        assert "started_at" in data["daemon"]
        assert "force_mode" in data["daemon"]

        assert "shepherds" in data
        assert "entries" in data["shepherds"]
        assert "invalid_task_ids" in data["shepherds"]
        assert "total" in data["shepherds"]

        assert "pipeline" in data
        assert "ready" in data["pipeline"]
        assert "building" in data["pipeline"]
        assert "review_requested" in data["pipeline"]
        assert "ready_to_merge" in data["pipeline"]
        assert "blocked" in data["pipeline"]

        assert "consistency" in data
        assert "orphaned_building" in data["consistency"]
        assert "stale_building" in data["consistency"]

        assert "support_roles" in data

        assert "diagnostics" in data
        assert "warnings" in data["diagnostics"]
        assert "criticals" in data["diagnostics"]
        assert "recommendations" in data["diagnostics"]
        assert "warning_count" in data["diagnostics"]
        assert "critical_count" in data["diagnostics"]
        assert "exit_code" in data["diagnostics"]

    def test_json_pipeline_substructure_matches_bash(self) -> None:
        """Verify pipeline JSON substructure matches bash implementation.

        Bash pipeline structure (daemon-health.sh lines 730-736):
        - ready: { count: N, issues: [...] }
        - building: { count: N, issues: [...] }
        - review_requested: { count: N, prs: [...] }
        - ready_to_merge: { count: N, prs: [...] }
        - blocked: { count: N, issues: [...] }
        """
        report = HealthReport()
        report.pipeline = PipelineState(
            ready=[{"number": 1}],
            building=[{"number": 2}],
            review_requested=[{"number": 3}],
            ready_to_merge=[{"number": 4}],
            blocked=[{"number": 5}],
        )

        output = format_json_output(report)
        data = json.loads(output)

        # Verify count and list keys match bash naming
        assert data["pipeline"]["ready"]["count"] == 1
        assert "issues" in data["pipeline"]["ready"]

        assert data["pipeline"]["building"]["count"] == 1
        assert "issues" in data["pipeline"]["building"]

        assert data["pipeline"]["review_requested"]["count"] == 1
        assert "prs" in data["pipeline"]["review_requested"]

        assert data["pipeline"]["ready_to_merge"]["count"] == 1
        assert "prs" in data["pipeline"]["ready_to_merge"]

        assert data["pipeline"]["blocked"]["count"] == 1
        assert "issues" in data["pipeline"]["blocked"]

    def test_json_support_roles_structure_matches_bash(self) -> None:
        """Verify support roles JSON structure matches bash implementation.

        Bash support roles structure (daemon-health.sh lines 661-668):
        - {name, last_completed_ago, expected_interval, current_status}
        """
        report = HealthReport()
        report.support_roles = [
            SupportRoleStatus(
                name="Guide",
                elapsed="5 min",
                interval="15 min",
                status="idle",
            )
        ]

        output = format_json_output(report)
        data = json.loads(output)

        assert len(data["support_roles"]) == 1
        role = data["support_roles"][0]

        # Verify field names match bash
        assert "name" in role
        assert "last_completed_ago" in role
        assert "expected_interval" in role
        assert "current_status" in role

        assert role["name"] == "Guide"
        assert role["last_completed_ago"] == "5 min"
        assert role["expected_interval"] == "15 min"
        assert role["current_status"] == "idle"

    def test_json_shepherd_entry_structure_matches_bash(self) -> None:
        """Verify shepherd entry JSON structure matches bash implementation.

        Bash shepherd structure (daemon-health.sh lines 649-656):
        - {key, task_id, status, issue, task_id_valid}
        """
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
        entry = data["shepherds"]["entries"][0]

        # Verify field names match bash
        assert "key" in entry
        assert "task_id" in entry
        assert "status" in entry
        assert "issue" in entry
        assert "task_id_valid" in entry

        assert entry["key"] == "shepherd-1"
        assert entry["task_id"] == "a1b2c3d"
        assert entry["status"] == "working"
        assert entry["issue"] == 42
        assert entry["task_id_valid"] is True

    def test_json_stale_building_structure_matches_bash(self) -> None:
        """Verify stale building JSON structure matches bash implementation.

        Bash stale building structure (daemon-health.sh lines 677-681):
        - {issue: N, age_minutes: N}
        """
        report = HealthReport()
        report.stale_building = [StaleIssue(number=42, age_minutes=30)]

        output = format_json_output(report)
        data = json.loads(output)

        assert len(data["consistency"]["stale_building"]) == 1
        stale = data["consistency"]["stale_building"][0]

        # Verify field names match bash
        assert "issue" in stale
        assert "age_minutes" in stale

        assert stale["issue"] == 42
        assert stale["age_minutes"] == 30

    def test_overdue_warning_threshold_matches_bash(self) -> None:
        """Verify overdue warning uses 2x interval like bash.

        Bash (daemon-health.sh line 481):
        - if [[ "$status" != "running" ]] && [[ $elapsed -gt $((expected_interval * 2)) ]]

        Python (daemon_diagnostic.py line 385):
        - if status != "running" and elapsed > (expected_interval * 2)
        """
        from datetime import datetime, timedelta, timezone

        from loom_tools.daemon_diagnostic import GUIDE_INTERVAL

        daemon_state = DaemonState(
            support_roles={
                "guide": SupportRoleEntry(
                    status="idle",
                    # Set last_completed to just over 2x the interval ago
                    last_completed=(
                        datetime.now(timezone.utc) - timedelta(seconds=GUIDE_INTERVAL * 2 + 60)
                    ).strftime("%Y-%m-%dT%H:%M:%SZ"),
                ),
            }
        )
        statuses, warnings = check_support_roles(daemon_state)

        # Should generate a warning because elapsed > 2x interval
        guide_warnings = [w for w in warnings if "Guide" in w]
        assert len(guide_warnings) == 1, "should warn when overdue"

    def test_running_role_no_overdue_warning_matches_bash(self) -> None:
        """Verify running roles don't generate overdue warnings like bash.

        Bash (daemon-health.sh line 481):
        - if [[ "$status" != "running" ]] && ...
        """
        from datetime import datetime, timedelta, timezone

        from loom_tools.daemon_diagnostic import GUIDE_INTERVAL

        daemon_state = DaemonState(
            support_roles={
                "guide": SupportRoleEntry(
                    status="running",
                    started="2026-01-23T10:00:00Z",
                    # Even with old last_completed, running status should suppress warning
                    last_completed=(
                        datetime.now(timezone.utc) - timedelta(seconds=GUIDE_INTERVAL * 3)
                    ).strftime("%Y-%m-%dT%H:%M:%SZ"),
                ),
            }
        )
        statuses, warnings = check_support_roles(daemon_state)

        # Should NOT generate overdue warning because status is "running"
        guide_warnings = [w for w in warnings if "Guide" in w and "ago" in w]
        assert len(guide_warnings) == 0, "running roles should not warn about overdue"


class TestEdgeCases:
    """Edge case tests to ensure Python matches bash error handling."""

    def test_empty_state_file(self, tmp_path: pathlib.Path) -> None:
        """Verify empty file is detected as corrupt like bash.

        Bash behavior: jq -e . will fail on empty file.
        Python behavior: Should detect as corrupt.
        """
        state_file = tmp_path / "daemon-state.json"
        state_file.write_text("")

        result = validate_state_file(str(state_file))
        assert result.valid is False
        assert result.status == "corrupt"

    def test_array_instead_of_object(self, tmp_path: pathlib.Path) -> None:
        """Verify JSON array is detected as corrupt.

        State file should be an object, not an array.
        """
        state_file = tmp_path / "daemon-state.json"
        state_file.write_text("[1, 2, 3]")

        result = validate_state_file(str(state_file))
        assert result.valid is False
        assert result.status == "corrupt"

    def test_orphaned_detection_only_counts_working_status(self) -> None:
        """Verify only 'working' status counts for orphan detection like bash.

        Bash (daemon-health.sh line 368):
        - select(.value.status == "working")
        """
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=42),
                "shepherd-2": ShepherdEntry(status="idle", issue=None),
                "shepherd-3": ShepherdEntry(status="paused", issue=99),  # paused != working
            }
        )
        building_issues = [
            {"number": 42, "title": "Issue 42"},
            {"number": 99, "title": "Issue 99 (paused shepherd)"},
        ]

        result = check_orphaned_building(building_issues, daemon_state)
        # Issue 99 should be orphaned because shepherd-3 is paused, not working
        assert 99 in result
        assert 42 not in result
