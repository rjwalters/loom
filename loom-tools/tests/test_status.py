"""Tests for loom_tools.status."""

from __future__ import annotations

import json
import pathlib
from datetime import datetime, timezone

import pytest

from loom_tools.models.daemon_state import (
    DaemonState,
    PipelineState,
    ShepherdEntry,
    SupportRoleEntry,
    Warning,
)
from loom_tools.status import (
    _Colors,
    format_seconds,
    format_uptime,
    main,
    output_formatted,
    output_json,
    render_daemon_status,
    render_layer3_actions,
    render_pipeline_status,
    render_session_stats,
    render_shepherds,
    render_stuck_detection,
    render_support_roles,
    render_system_state,
    render_warnings,
    time_ago,
)

# Fixed "now" for deterministic tests
NOW = datetime(2026, 1, 30, 18, 0, 0, tzinfo=timezone.utc)

# No-color palette for testing
NC = _Colors(use_color=False)
CC = _Colors(use_color=True)


# ---------------------------------------------------------------------------
# time_ago
# ---------------------------------------------------------------------------


class TestTimeAgo:
    def test_none_returns_never(self):
        assert time_ago(None) == "never"

    def test_empty_returns_never(self):
        assert time_ago("") == "never"

    def test_null_string_returns_never(self):
        assert time_ago("null") == "never"

    def test_invalid_returns_unknown(self):
        assert time_ago("not-a-timestamp") == "unknown"

    def test_seconds_ago(self):
        ts = "2026-01-30T17:59:30Z"
        result = time_ago(ts, _now=NOW)
        assert result == "30s ago"

    def test_minutes_ago(self):
        ts = "2026-01-30T17:55:00Z"
        result = time_ago(ts, _now=NOW)
        assert result == "5m ago"

    def test_hours_ago(self):
        ts = "2026-01-30T15:30:00Z"
        result = time_ago(ts, _now=NOW)
        assert result == "2h 30m ago"

    def test_days_ago(self):
        ts = "2026-01-28T12:00:00Z"
        result = time_ago(ts, _now=NOW)
        assert result == "2d 6h ago"

    def test_future_returns_just_now(self):
        ts = "2026-01-30T19:00:00Z"
        result = time_ago(ts, _now=NOW)
        assert result == "just now"


# ---------------------------------------------------------------------------
# format_uptime
# ---------------------------------------------------------------------------


class TestFormatUptime:
    def test_none_returns_unknown(self):
        assert format_uptime(None) == "unknown"

    def test_empty_returns_unknown(self):
        assert format_uptime("") == "unknown"

    def test_null_string_returns_unknown(self):
        assert format_uptime("null") == "unknown"

    def test_invalid_returns_unknown(self):
        assert format_uptime("not-a-timestamp") == "unknown"

    def test_seconds(self):
        ts = "2026-01-30T17:59:30Z"
        assert format_uptime(ts, _now=NOW) == "30s"

    def test_minutes(self):
        ts = "2026-01-30T17:55:00Z"
        assert format_uptime(ts, _now=NOW) == "5m"

    def test_hours_minutes(self):
        ts = "2026-01-30T15:30:00Z"
        assert format_uptime(ts, _now=NOW) == "2h 30m"

    def test_days_hours(self):
        ts = "2026-01-28T12:00:00Z"
        assert format_uptime(ts, _now=NOW) == "2d 6h"


# ---------------------------------------------------------------------------
# format_seconds
# ---------------------------------------------------------------------------


class TestFormatSeconds:
    def test_negative(self):
        assert format_seconds(-1) == "unknown"

    def test_zero(self):
        assert format_seconds(0) == "0s"

    def test_seconds(self):
        assert format_seconds(45) == "45s"

    def test_minutes(self):
        assert format_seconds(120) == "2m"

    def test_minutes_seconds(self):
        assert format_seconds(150) == "2m 30s"

    def test_hours_minutes(self):
        assert format_seconds(3661) == "1h 1m"

    def test_days_hours(self):
        assert format_seconds(90000) == "1d 1h"


# ---------------------------------------------------------------------------
# render_daemon_status
# ---------------------------------------------------------------------------


class TestRenderDaemonStatus:
    def test_stopped(self, tmp_path):
        ds = DaemonState(running=False)
        lines = render_daemon_status(ds, tmp_path, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "Stopped" in combined
        assert "n/a" in combined

    def test_running(self, tmp_path):
        ds = DaemonState(
            running=True,
            started_at="2026-01-30T16:00:00Z",
            last_poll="2026-01-30T17:55:00Z",
        )
        lines = render_daemon_status(ds, tmp_path, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "Running" in combined
        assert "2h 0m" in combined
        assert "5m ago" in combined

    def test_stopping(self, tmp_path):
        stop_file = tmp_path / ".loom" / "stop-daemon"
        stop_file.parent.mkdir(parents=True)
        stop_file.touch()
        ds = DaemonState(running=True)
        lines = render_daemon_status(ds, tmp_path, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "Stopping" in combined


# ---------------------------------------------------------------------------
# render_system_state
# ---------------------------------------------------------------------------


class TestRenderSystemState:
    def test_basic(self):
        snapshot = {
            "computed": {
                "total_ready": 5,
                "total_building": 2,
                "prs_awaiting_review": 1,
                "prs_ready_to_merge": 0,
            },
            "proposals": {
                "architect": [{"number": 1}],
                "hermit": [{"number": 2}],
                "curated": [{"number": 3}, {"number": 4}],
            },
        }
        lines = render_system_state(snapshot, NC)
        combined = "\n".join(lines)
        assert "5" in combined
        assert "2" in combined
        assert "Proposals pending" in combined


# ---------------------------------------------------------------------------
# render_shepherds
# ---------------------------------------------------------------------------


class TestRenderShepherds:
    def test_no_daemon_state(self):
        ds = DaemonState()
        lines = render_shepherds(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "No daemon state available" in combined

    def test_active_shepherd(self):
        ds = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working",
                    issue=42,
                    started="2026-01-30T17:00:00Z",
                    last_phase="builder",
                    pr_number=100,
                ),
            }
        )
        lines = render_shepherds(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "1/1 active" in combined
        assert "Issue #42" in combined
        assert "[phase: builder]" in combined
        assert "[PR #100]" in combined

    def test_idle_shepherd(self):
        ds = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="idle",
                    idle_since="2026-01-30T17:30:00Z",
                    idle_reason="no_ready_issues",
                ),
            }
        )
        lines = render_shepherds(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "0/1 active" in combined
        assert "idle" in combined
        assert "no ready issues" in combined

    def test_errored_shepherd(self):
        ds = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="errored"),
            }
        )
        lines = render_shepherds(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "errored" in combined


# ---------------------------------------------------------------------------
# render_support_roles
# ---------------------------------------------------------------------------


class TestRenderSupportRoles:
    def test_no_data(self):
        ds = DaemonState()
        lines = render_support_roles(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "No daemon state available" in combined

    def test_running_role(self):
        ds = DaemonState(
            support_roles={
                "architect": SupportRoleEntry(status="running", task_id="abc1234"),
                "hermit": SupportRoleEntry(status="idle"),
                "guide": SupportRoleEntry(status="idle"),
                "champion": SupportRoleEntry(status="idle"),
            }
        )
        lines = render_support_roles(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "Architect:" in combined
        assert "running" in combined

    def test_idle_role_with_last_completed(self):
        ds = DaemonState(
            support_roles={
                "architect": SupportRoleEntry(
                    status="idle",
                    last_completed="2026-01-30T17:50:00Z",
                ),
                "hermit": SupportRoleEntry(status="idle"),
                "guide": SupportRoleEntry(status="idle"),
                "champion": SupportRoleEntry(status="idle"),
            }
        )
        lines = render_support_roles(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "idle (last: 10m ago)" in combined


# ---------------------------------------------------------------------------
# render_session_stats
# ---------------------------------------------------------------------------


class TestRenderSessionStats:
    def test_no_data(self):
        ds = DaemonState()
        lines = render_session_stats(ds, NC)
        combined = "\n".join(lines)
        assert "No session data available" in combined

    def test_with_data(self):
        ds = DaemonState(
            started_at="2026-01-30T10:00:00Z",
            iteration=42,
            completed_issues=[100, 101, 102],
            total_prs_merged=3,
        )
        lines = render_session_stats(ds, NC)
        combined = "\n".join(lines)
        assert "42" in combined
        assert "3" in combined


# ---------------------------------------------------------------------------
# render_pipeline_status
# ---------------------------------------------------------------------------


class TestRenderPipelineStatus:
    def test_no_blocked(self):
        ds = DaemonState()
        lines = render_pipeline_status(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "No blocked items" in combined

    def test_blocked_items(self):
        ds = DaemonState(
            pipeline_state=PipelineState(
                blocked=[
                    {"type": "pr", "number": 100, "reason": "merge_conflicts"},
                ],
                last_updated="2026-01-30T17:50:00Z",
            )
        )
        lines = render_pipeline_status(ds, NC, _now=NOW)
        combined = "\n".join(lines)
        assert "Blocked Items: 1" in combined
        assert "pr #100: merge_conflicts" in combined
        assert "Last sync:" in combined


# ---------------------------------------------------------------------------
# render_warnings
# ---------------------------------------------------------------------------


class TestRenderWarnings:
    def test_no_warnings(self):
        ds = DaemonState()
        lines = render_warnings(ds, NC)
        combined = "\n".join(lines)
        assert "No warnings" in combined

    def test_all_acknowledged(self):
        ds = DaemonState(
            warnings=[
                Warning(
                    time="2026-01-30T17:00:00Z",
                    type="test",
                    severity="warning",
                    message="test warning",
                    acknowledged=True,
                ),
            ]
        )
        lines = render_warnings(ds, NC)
        combined = "\n".join(lines)
        assert "All warnings acknowledged" in combined
        assert "1 total" in combined

    def test_unacknowledged_warnings(self):
        ds = DaemonState(
            warnings=[
                Warning(
                    time="2026-01-30T17:00:00Z",
                    type="blocked_pr",
                    severity="warning",
                    message="PR #100 has merge conflicts",
                    acknowledged=False,
                ),
            ]
        )
        lines = render_warnings(ds, NC)
        combined = "\n".join(lines)
        assert "1 unacknowledged" in combined
        assert "PR #100 has merge conflicts" in combined

    def test_warning_severity_colors(self):
        ds = DaemonState(
            warnings=[
                Warning(time="2026-01-30T17:00:00Z", severity="error", message="err"),
                Warning(time="2026-01-30T17:00:00Z", severity="warning", message="warn"),
                Warning(time="2026-01-30T17:00:00Z", severity="info", message="info"),
            ]
        )
        # Test with colors to verify severity-based coloring
        lines = render_warnings(ds, CC)
        combined = "\n".join(lines)
        assert "\033[0;31m" in combined  # red for error
        assert "\033[1;33m" in combined  # yellow for warning


# ---------------------------------------------------------------------------
# render_stuck_detection
# ---------------------------------------------------------------------------


class TestRenderStuckDetection:
    def test_no_interventions(self, tmp_path):
        lines = render_stuck_detection(tmp_path, NC)
        combined = "\n".join(lines)
        assert "All agents healthy" in combined

    def test_with_interventions(self, tmp_path):
        interventions_dir = tmp_path / ".loom" / "interventions"
        interventions_dir.mkdir(parents=True)
        (interventions_dir / "agent-1.json").write_text(json.dumps({
            "agent_id": "shepherd-1",
            "severity": "high",
            "intervention_type": "restart",
        }))
        lines = render_stuck_detection(tmp_path, NC)
        combined = "\n".join(lines)
        assert "1 active intervention(s)" in combined
        assert "shepherd-1" in combined
        assert "restart" in combined

    def test_with_config(self, tmp_path):
        stuck_config = tmp_path / ".loom" / "stuck-config.json"
        stuck_config.parent.mkdir(parents=True, exist_ok=True)
        stuck_config.write_text(json.dumps({
            "idle_threshold": 600,
            "working_threshold": 1800,
            "intervention_mode": "escalate",
        }))
        lines = render_stuck_detection(tmp_path, NC)
        combined = "\n".join(lines)
        assert "idle=10m" in combined
        assert "working=30m" in combined
        assert "mode=escalate" in combined

    def test_default_config(self, tmp_path):
        lines = render_stuck_detection(tmp_path, NC)
        combined = "\n".join(lines)
        assert "Using defaults" in combined


# ---------------------------------------------------------------------------
# render_layer3_actions
# ---------------------------------------------------------------------------


class TestRenderLayer3Actions:
    def test_with_proposals(self, tmp_path):
        snapshot = {
            "proposals": {
                "architect": [{"number": 1}],
                "hermit": [],
                "curated": [{"number": 2}],
            },
        }
        ds = DaemonState()
        lines = render_layer3_actions(snapshot, ds, tmp_path, NC)
        combined = "\n".join(lines)
        assert "Pending Approvals:" in combined
        assert "architect proposals" in combined
        assert "Curated Issues Awaiting Approval:" in combined

    def test_daemon_control_stop(self, tmp_path):
        stop_file = tmp_path / ".loom" / "stop-daemon"
        stop_file.parent.mkdir(parents=True)
        stop_file.touch()
        snapshot = {"proposals": {"architect": [], "hermit": [], "curated": []}}
        ds = DaemonState()
        lines = render_layer3_actions(snapshot, ds, tmp_path, NC)
        combined = "\n".join(lines)
        assert "Cancel shutdown" in combined

    def test_daemon_control_running(self, tmp_path):
        snapshot = {"proposals": {"architect": [], "hermit": [], "curated": []}}
        ds = DaemonState()
        lines = render_layer3_actions(snapshot, ds, tmp_path, NC)
        combined = "\n".join(lines)
        assert "Stop daemon" in combined


# ---------------------------------------------------------------------------
# output_formatted
# ---------------------------------------------------------------------------


class TestOutputFormatted:
    def test_no_color(self, tmp_path):
        ds = DaemonState()
        snapshot = {
            "computed": {
                "total_ready": 0,
                "total_building": 0,
                "prs_awaiting_review": 0,
                "prs_ready_to_merge": 0,
            },
            "proposals": {"architect": [], "hermit": [], "curated": []},
        }
        result = output_formatted(snapshot, ds, tmp_path, use_color=False, _now=NOW)
        assert "LOOM SYSTEM STATUS" in result
        # No ANSI codes
        assert "\033[" not in result

    def test_with_color(self, tmp_path):
        ds = DaemonState()
        snapshot = {
            "computed": {
                "total_ready": 0,
                "total_building": 0,
                "prs_awaiting_review": 0,
                "prs_ready_to_merge": 0,
            },
            "proposals": {"architect": [], "hermit": [], "curated": []},
        }
        result = output_formatted(snapshot, ds, tmp_path, use_color=True, _now=NOW)
        assert "LOOM SYSTEM STATUS" in result
        # Should have ANSI codes
        assert "\033[" in result

    def test_full_state(self, tmp_path):
        (tmp_path / ".loom").mkdir(parents=True, exist_ok=True)
        ds = DaemonState(
            running=True,
            started_at="2026-01-30T16:00:00Z",
            last_poll="2026-01-30T17:55:00Z",
            iteration=10,
            completed_issues=[1, 2, 3],
            total_prs_merged=2,
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42,
                    started="2026-01-30T17:00:00Z",
                ),
                "shepherd-2": ShepherdEntry(status="idle"),
            },
            support_roles={
                "architect": SupportRoleEntry(status="idle"),
                "hermit": SupportRoleEntry(status="idle"),
                "guide": SupportRoleEntry(status="running"),
                "champion": SupportRoleEntry(status="idle"),
            },
        )
        snapshot = {
            "computed": {
                "total_ready": 3,
                "total_building": 1,
                "prs_awaiting_review": 2,
                "prs_ready_to_merge": 1,
            },
            "proposals": {
                "architect": [{"number": 10}],
                "hermit": [],
                "curated": [],
            },
        }
        result = output_formatted(snapshot, ds, tmp_path, use_color=False, _now=NOW)
        assert "Running" in result
        assert "Issue #42" in result
        assert "Guide:" in result
        assert "Iteration: 10" in result


# ---------------------------------------------------------------------------
# output_json
# ---------------------------------------------------------------------------


class TestOutputJson:
    def test_valid_json(self):
        snapshot = {"key": "value", "nested": {"a": 1}}
        result = output_json(snapshot)
        parsed = json.loads(result)
        assert parsed["key"] == "value"
        assert parsed["nested"]["a"] == 1


# ---------------------------------------------------------------------------
# CLI (main)
# ---------------------------------------------------------------------------


class TestMain:
    def test_help(self, capsys):
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0
        captured = capsys.readouterr()
        assert "USAGE:" in captured.out
        assert "loom-status" in captured.out

    def test_help_short(self, capsys):
        with pytest.raises(SystemExit) as exc_info:
            main(["-h"])
        assert exc_info.value.code == 0
        captured = capsys.readouterr()
        assert "USAGE:" in captured.out

    def test_unknown_option(self, capsys):
        with pytest.raises(SystemExit) as exc_info:
            main(["--bogus"])
        assert exc_info.value.code == 1
        captured = capsys.readouterr()
        assert "Unknown option" in captured.err


# ---------------------------------------------------------------------------
# TTY detection
# ---------------------------------------------------------------------------


class TestTtyDetection:
    def test_no_color_colors(self):
        c = _Colors(use_color=False)
        assert c.red == ""
        assert c.green == ""
        assert c.bold == ""
        assert c.reset == ""

    def test_color_colors(self):
        c = _Colors(use_color=True)
        assert c.red == "\033[0;31m"
        assert c.green == "\033[0;32m"
        assert c.bold == "\033[1m"
        assert c.reset == "\033[0m"


# ---------------------------------------------------------------------------
# Missing daemon state (graceful degradation)
# ---------------------------------------------------------------------------


class TestGracefulDegradation:
    def test_empty_daemon_state(self, tmp_path):
        ds = DaemonState()
        snapshot = {
            "computed": {
                "total_ready": 0,
                "total_building": 0,
                "prs_awaiting_review": 0,
                "prs_ready_to_merge": 0,
            },
            "proposals": {"architect": [], "hermit": [], "curated": []},
        }
        result = output_formatted(snapshot, ds, tmp_path, use_color=False, _now=NOW)
        # Should not crash, should show degraded state
        assert "LOOM SYSTEM STATUS" in result
        assert "Stopped" in result
        assert "No daemon state available" in result
