"""Tests for loom_tools.agent_metrics."""

from __future__ import annotations

import json
import pathlib
import sqlite3
from unittest import mock

import pytest

from loom_tools.agent_metrics import (
    CostRow,
    EffectivenessRow,
    FallbackSummary,
    FallbackVelocity,
    SummaryMetrics,
    VelocityRow,
    _get_period_filter,
    _get_role_filter,
    format_costs_text,
    format_effectiveness_text,
    format_summary_fallback_text,
    format_summary_text,
    format_velocity_fallback_text,
    format_velocity_text,
    get_costs,
    get_effectiveness,
    get_summary,
    get_summary_fallback,
    get_velocity,
    get_velocity_fallback,
    main,
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def activity_db(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a test activity database with sample data."""
    db_path = tmp_path / "activity.db"
    conn = sqlite3.connect(str(db_path))
    conn.executescript(
        """
        CREATE TABLE agent_inputs (
            id INTEGER PRIMARY KEY,
            agent_role TEXT,
            timestamp TEXT
        );
        CREATE TABLE resource_usage (
            input_id INTEGER,
            tokens_input INTEGER,
            tokens_output INTEGER,
            cost_usd REAL,
            duration_ms INTEGER,
            FOREIGN KEY (input_id) REFERENCES agent_inputs(id)
        );
        CREATE TABLE prompt_github (
            input_id INTEGER,
            issue_number INTEGER,
            pr_number INTEGER,
            event_type TEXT,
            FOREIGN KEY (input_id) REFERENCES agent_inputs(id)
        );
        CREATE TABLE quality_metrics (
            input_id INTEGER,
            tests_passed INTEGER,
            tests_failed INTEGER,
            FOREIGN KEY (input_id) REFERENCES agent_inputs(id)
        );

        -- Sample data: 3 builder prompts, 2 judge prompts
        INSERT INTO agent_inputs VALUES (1, 'builder', datetime('now', '-1 day'));
        INSERT INTO agent_inputs VALUES (2, 'builder', datetime('now', '-2 days'));
        INSERT INTO agent_inputs VALUES (3, 'builder', datetime('now', '-3 days'));
        INSERT INTO agent_inputs VALUES (4, 'judge', datetime('now', '-1 day'));
        INSERT INTO agent_inputs VALUES (5, 'judge', datetime('now', '-2 days'));

        INSERT INTO resource_usage VALUES (1, 1000, 500, 0.05, 30000);
        INSERT INTO resource_usage VALUES (2, 2000, 1000, 0.10, 45000);
        INSERT INTO resource_usage VALUES (3, 1500, 750, 0.08, 35000);
        INSERT INTO resource_usage VALUES (4, 800, 400, 0.03, 20000);
        INSERT INTO resource_usage VALUES (5, 1200, 600, 0.06, 25000);

        INSERT INTO prompt_github VALUES (1, 42, NULL, 'issue_work');
        INSERT INTO prompt_github VALUES (2, 42, NULL, 'issue_work');
        INSERT INTO prompt_github VALUES (3, 43, NULL, 'issue_work');
        INSERT INTO prompt_github VALUES (4, NULL, 100, 'pr_review');
        INSERT INTO prompt_github VALUES (5, NULL, 101, 'pr_merged');

        -- Builder: 2 pass, 1 fail; Judge: 2 pass
        INSERT INTO quality_metrics VALUES (1, 5, 0);
        INSERT INTO quality_metrics VALUES (2, 3, 0);
        INSERT INTO quality_metrics VALUES (3, 0, 2);
        INSERT INTO quality_metrics VALUES (4, 4, 0);
        INSERT INTO quality_metrics VALUES (5, 6, 0);
        """
    )
    conn.close()
    return db_path


@pytest.fixture
def empty_db(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create an empty activity database (tables exist but no rows)."""
    db_path = tmp_path / "empty.db"
    conn = sqlite3.connect(str(db_path))
    conn.executescript(
        """
        CREATE TABLE agent_inputs (
            id INTEGER PRIMARY KEY,
            agent_role TEXT,
            timestamp TEXT
        );
        CREATE TABLE resource_usage (
            input_id INTEGER,
            tokens_input INTEGER,
            tokens_output INTEGER,
            cost_usd REAL,
            duration_ms INTEGER
        );
        CREATE TABLE prompt_github (
            input_id INTEGER,
            issue_number INTEGER,
            pr_number INTEGER,
            event_type TEXT
        );
        CREATE TABLE quality_metrics (
            input_id INTEGER,
            tests_passed INTEGER,
            tests_failed INTEGER
        );
        """
    )
    conn.close()
    return db_path


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    return tmp_path


# ---------------------------------------------------------------------------
# Period filter tests
# ---------------------------------------------------------------------------


class TestPeriodFilter:
    """Tests for SQL period filtering."""

    def test_today(self) -> None:
        f = _get_period_filter("today")
        assert "start of day" in f
        assert f.startswith("AND")

    def test_week(self) -> None:
        f = _get_period_filter("week")
        assert "-7 days" in f
        assert f.startswith("AND")

    def test_month(self) -> None:
        f = _get_period_filter("month")
        assert "-30 days" in f
        assert f.startswith("AND")

    def test_all(self) -> None:
        f = _get_period_filter("all")
        assert f == ""

    def test_unknown_treated_as_all(self) -> None:
        f = _get_period_filter("unknown")
        assert f == ""


# ---------------------------------------------------------------------------
# Role filter tests
# ---------------------------------------------------------------------------


class TestRoleFilter:
    """Tests for SQL role filtering."""

    def test_empty_role(self) -> None:
        assert _get_role_filter("") == ""

    def test_builder_role(self) -> None:
        f = _get_role_filter("builder")
        assert "agent_role = 'builder'" in f
        assert f.startswith("AND")


# ---------------------------------------------------------------------------
# Summary command tests
# ---------------------------------------------------------------------------


class TestSummary:
    """Tests for the summary command with mocked DB."""

    def test_summary_returns_correct_totals(self, activity_db: pathlib.Path) -> None:
        m = get_summary(activity_db, role="", period="all")
        assert m.total_prompts == 5
        assert m.total_tokens == (1000 + 500 + 2000 + 1000 + 1500 + 750 + 800 + 400 + 1200 + 600)
        assert m.total_cost == pytest.approx(0.32, abs=0.01)
        assert m.issues_count == 2  # issues 42, 43
        assert m.prs_count == 2  # PRs 100, 101

    def test_summary_success_rate(self, activity_db: pathlib.Path) -> None:
        m = get_summary(activity_db, role="", period="all")
        # 4 of 5 prompts have tests_passed>0 and tests_failed=0
        assert m.success_rate == pytest.approx(80.0, abs=0.1)

    def test_summary_role_filter(self, activity_db: pathlib.Path) -> None:
        m = get_summary(activity_db, role="builder", period="all")
        assert m.total_prompts == 3

    def test_summary_role_filter_judge(self, activity_db: pathlib.Path) -> None:
        m = get_summary(activity_db, role="judge", period="all")
        assert m.total_prompts == 2

    def test_summary_empty_db(self, empty_db: pathlib.Path) -> None:
        m = get_summary(empty_db, role="", period="all")
        assert m.total_prompts == 0
        assert m.total_tokens == 0
        assert m.total_cost == 0.0
        assert m.success_rate == 0.0

    def test_summary_to_dict(self, activity_db: pathlib.Path) -> None:
        m = get_summary(activity_db, role="", period="all")
        d = m.to_dict()
        assert "total_prompts" in d
        assert "success_rate" in d
        assert isinstance(d["total_prompts"], int)


# ---------------------------------------------------------------------------
# Effectiveness command tests
# ---------------------------------------------------------------------------


class TestEffectiveness:
    """Tests for the effectiveness command."""

    def test_groups_by_role(self, activity_db: pathlib.Path) -> None:
        rows = get_effectiveness(activity_db, role="", period="all")
        roles = {r.role for r in rows}
        assert "builder" in roles
        assert "judge" in roles

    def test_computes_success_rate(self, activity_db: pathlib.Path) -> None:
        rows = get_effectiveness(activity_db, role="", period="all")
        by_role = {r.role: r for r in rows}

        # Judge: 2/2 = 100%
        assert by_role["judge"].success_rate == pytest.approx(100.0, abs=0.1)
        # Builder: 2/3 ~ 66.7%
        assert by_role["builder"].success_rate == pytest.approx(66.7, abs=0.1)

    def test_role_filter(self, activity_db: pathlib.Path) -> None:
        rows = get_effectiveness(activity_db, role="builder", period="all")
        assert len(rows) == 1
        assert rows[0].role == "builder"

    def test_empty_db(self, empty_db: pathlib.Path) -> None:
        rows = get_effectiveness(empty_db, role="", period="all")
        assert rows == []


# ---------------------------------------------------------------------------
# Costs command tests
# ---------------------------------------------------------------------------


class TestCosts:
    """Tests for the costs command."""

    def test_costs_ordered_by_cost_desc(self, activity_db: pathlib.Path) -> None:
        rows = get_costs(activity_db, issue_number=None)
        assert len(rows) >= 1
        if len(rows) > 1:
            assert rows[0].total_cost >= rows[1].total_cost

    def test_costs_filter_by_issue(self, activity_db: pathlib.Path) -> None:
        rows = get_costs(activity_db, issue_number=42)
        assert len(rows) == 1
        assert rows[0].issue_number == 42
        assert rows[0].prompt_count == 2

    def test_costs_nonexistent_issue(self, activity_db: pathlib.Path) -> None:
        rows = get_costs(activity_db, issue_number=9999)
        assert rows == []

    def test_empty_db(self, empty_db: pathlib.Path) -> None:
        rows = get_costs(empty_db, issue_number=None)
        assert rows == []


# ---------------------------------------------------------------------------
# Velocity command tests
# ---------------------------------------------------------------------------


class TestVelocity:
    """Tests for the velocity command."""

    def test_velocity_groups_by_week(self, activity_db: pathlib.Path) -> None:
        rows = get_velocity(activity_db)
        # All data is within last 8 weeks, should have at least one week
        assert len(rows) >= 1
        for r in rows:
            assert r.week  # Non-empty week string
            assert "W" in r.week  # ISO week format

    def test_velocity_limited_to_8_weeks(self, activity_db: pathlib.Path) -> None:
        rows = get_velocity(activity_db)
        assert len(rows) <= 8

    def test_empty_db(self, empty_db: pathlib.Path) -> None:
        rows = get_velocity(empty_db)
        assert rows == []


# ---------------------------------------------------------------------------
# Fallback path tests
# ---------------------------------------------------------------------------


class TestFallbackSummary:
    """Tests for the fallback summary when DB is unavailable."""

    def test_fallback_with_daemon_state(self, mock_repo: pathlib.Path) -> None:
        # Write a daemon state file
        state_file = mock_repo / ".loom" / "daemon-state.json"
        state_file.write_text(
            json.dumps(
                {
                    "completed_issues": [100, 101, 102],
                    "total_prs_merged": 3,
                }
            )
        )

        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch("loom_tools.agent_metrics.gh_run") as mock_gh:
                # Mock gh issue list and pr list
                mock_gh.side_effect = [
                    mock.Mock(returncode=0, stdout="5"),
                    mock.Mock(returncode=0, stdout="2"),
                ]
                fb = get_summary_fallback(mock_repo)

        assert fb.completed_issues == 3
        assert fb.total_prs_merged == 3
        assert fb.open_issues == 5
        assert fb.open_prs == 2
        assert "not available" in fb.note

    def test_fallback_no_daemon_state(self, mock_repo: pathlib.Path) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch("loom_tools.agent_metrics.gh_run") as mock_gh:
                mock_gh.side_effect = [
                    mock.Mock(returncode=0, stdout="0"),
                    mock.Mock(returncode=0, stdout="0"),
                ]
                fb = get_summary_fallback(mock_repo)

        assert fb.completed_issues == 0
        assert fb.total_prs_merged == 0


class TestFallbackVelocity:
    """Tests for the fallback velocity when DB is unavailable."""

    def test_fallback_with_daemon_state(self, mock_repo: pathlib.Path) -> None:
        state_file = mock_repo / ".loom" / "daemon-state.json"
        state_file.write_text(
            json.dumps(
                {
                    "started_at": "2026-01-23T10:00:00Z",
                    "completed_issues": [1, 2, 3],
                    "total_prs_merged": 2,
                }
            )
        )

        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            fb = get_velocity_fallback(mock_repo)

        assert fb.completed_issues == 3
        assert fb.prs_merged == 2
        assert fb.session_started == "2026-01-23T10:00:00Z"


# ---------------------------------------------------------------------------
# Text formatting tests
# ---------------------------------------------------------------------------


class TestTextFormatting:
    """Tests for text output formatting."""

    def test_summary_text_contains_header(self) -> None:
        m = SummaryMetrics(
            total_prompts=10,
            total_tokens=50000,
            total_cost=1.5,
            issues_count=3,
            prs_count=2,
            success_rate=85.0,
        )
        output = format_summary_text(m, "week")
        assert "Agent Performance Summary" in output
        assert "week" in output
        assert "10" in output
        assert "50K" in output
        assert "$1.5000" in output
        assert "85.0%" in output

    def test_summary_fallback_text(self) -> None:
        fb = FallbackSummary(completed_issues=5, total_prs_merged=3, open_issues=10, open_prs=2)
        output = format_summary_fallback_text(fb)
        assert "daemon state" in output
        assert "5" in output
        assert "not available" in output

    def test_effectiveness_text_table(self) -> None:
        rows = [
            EffectivenessRow(
                role="builder",
                total_prompts=10,
                successful_prompts=8,
                success_rate=80.0,
                avg_cost=0.05,
                avg_duration_sec=30.0,
            ),
            EffectivenessRow(
                role="judge",
                total_prompts=5,
                successful_prompts=5,
                success_rate=100.0,
                avg_cost=0.03,
                avg_duration_sec=20.0,
            ),
        ]
        output = format_effectiveness_text(rows, "week")
        assert "Effectiveness" in output
        assert "builder" in output
        assert "judge" in output

    def test_costs_text_table(self) -> None:
        rows = [
            CostRow(issue_number=42, prompt_count=5, total_cost=0.25, total_tokens=10000),
        ]
        output = format_costs_text(rows)
        assert "#42" in output
        assert "$0.2500" in output

    def test_velocity_text_table(self) -> None:
        rows = [
            VelocityRow(week="2026-W04", prompts=20, issues=3, prs_merged=2, cost=1.5),
        ]
        output = format_velocity_text(rows)
        assert "2026-W04" in output
        assert "20" in output

    def test_velocity_fallback_text(self) -> None:
        fb = FallbackVelocity(
            completed_issues=5, prs_merged=3, session_started="2026-01-23T10:00:00Z"
        )
        output = format_velocity_fallback_text(fb)
        assert "daemon state" in output
        assert "5" in output
        assert "2026-01-23T10:00:00Z" in output


# ---------------------------------------------------------------------------
# JSON formatting tests
# ---------------------------------------------------------------------------


class TestJsonFormatting:
    """Tests for JSON output formatting."""

    def test_summary_json_valid(self, activity_db: pathlib.Path) -> None:
        m = get_summary(activity_db, role="", period="all")
        output = json.dumps(m.to_dict(), indent=2)
        parsed = json.loads(output)
        assert "total_prompts" in parsed
        assert "success_rate" in parsed
        assert isinstance(parsed["total_prompts"], int)
        assert isinstance(parsed["success_rate"], float)

    def test_effectiveness_json_valid(self, activity_db: pathlib.Path) -> None:
        rows = get_effectiveness(activity_db, role="", period="all")
        output = json.dumps([r.to_dict() for r in rows], indent=2)
        parsed = json.loads(output)
        assert isinstance(parsed, list)
        assert len(parsed) >= 1
        assert "role" in parsed[0]
        assert "success_rate" in parsed[0]

    def test_costs_json_valid(self, activity_db: pathlib.Path) -> None:
        rows = get_costs(activity_db, issue_number=None)
        output = json.dumps([r.to_dict() for r in rows], indent=2)
        parsed = json.loads(output)
        assert isinstance(parsed, list)

    def test_velocity_json_valid(self, activity_db: pathlib.Path) -> None:
        rows = get_velocity(activity_db)
        output = json.dumps([r.to_dict() for r in rows], indent=2)
        parsed = json.loads(output)
        assert isinstance(parsed, list)

    def test_fallback_summary_json_valid(self) -> None:
        fb = FallbackSummary(completed_issues=5, total_prs_merged=3, open_issues=10, open_prs=2)
        output = json.dumps(fb.to_dict(), indent=2)
        parsed = json.loads(output)
        assert "note" in parsed
        assert parsed["completed_issues"] == 5


# ---------------------------------------------------------------------------
# ANSI color tests
# ---------------------------------------------------------------------------


class TestColorOutput:
    """Tests for ANSI color code presence in text output."""

    def test_summary_text_has_ansi_when_tty(self) -> None:
        m = SummaryMetrics(success_rate=95.0)
        with mock.patch("loom_tools.agent_metrics._use_color", return_value=True):
            output = format_summary_text(m, "week")
        assert "\033[" in output  # ANSI escape present

    def test_summary_text_no_ansi_when_not_tty(self) -> None:
        m = SummaryMetrics(success_rate=95.0)
        with mock.patch("loom_tools.agent_metrics._use_color", return_value=False):
            output = format_summary_text(m, "week")
        assert "\033[" not in output

    def test_success_rate_color_coding(self) -> None:
        from loom_tools.agent_metrics import _rate_color, _GREEN, _YELLOW, _RED

        assert _rate_color(95.0) == _GREEN
        assert _rate_color(80.0) == _YELLOW
        assert _rate_color(60.0) == _RED


# ---------------------------------------------------------------------------
# CLI integration tests
# ---------------------------------------------------------------------------


class TestCLI:
    """Tests for the main CLI entry point."""

    def test_help_exits_zero(self) -> None:
        with pytest.raises(SystemExit) as exc:
            main(["--help"])
        assert exc.value.code == 0

    def test_summary_with_db(self, activity_db: pathlib.Path, mock_repo: pathlib.Path) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=activity_db
            ):
                rc = main(["summary", "--format", "json", "--period", "all"])
        assert rc == 0

    def test_effectiveness_with_db(
        self, activity_db: pathlib.Path, mock_repo: pathlib.Path
    ) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=activity_db
            ):
                rc = main(["effectiveness", "--format", "json", "--period", "all"])
        assert rc == 0

    def test_costs_with_db(self, activity_db: pathlib.Path, mock_repo: pathlib.Path) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=activity_db
            ):
                rc = main(["costs", "--format", "json"])
        assert rc == 0

    def test_velocity_with_db(self, activity_db: pathlib.Path, mock_repo: pathlib.Path) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=activity_db
            ):
                rc = main(["velocity", "--format", "json"])
        assert rc == 0

    def test_fallback_when_no_db(self, mock_repo: pathlib.Path, tmp_path: pathlib.Path) -> None:
        nonexistent = tmp_path / "no-such-file.db"
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=nonexistent
            ):
                with mock.patch("loom_tools.agent_metrics.gh_run") as mock_gh:
                    mock_gh.return_value = mock.Mock(returncode=0, stdout="0")
                    rc = main(["summary", "--format", "json"])
        assert rc == 0

    def test_role_filter_cli(self, activity_db: pathlib.Path, mock_repo: pathlib.Path) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=activity_db
            ):
                rc = main(["summary", "--role", "builder", "--format", "json", "--period", "all"])
        assert rc == 0

    def test_costs_issue_filter_cli(
        self, activity_db: pathlib.Path, mock_repo: pathlib.Path
    ) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=activity_db
            ):
                rc = main(["costs", "--issue", "42", "--format", "json"])
        assert rc == 0

    def test_not_in_repo(self) -> None:
        with mock.patch(
            "loom_tools.agent_metrics.find_repo_root",
            side_effect=FileNotFoundError("not in repo"),
        ):
            rc = main(["summary"])
        assert rc == 1

    def test_default_command_is_summary(
        self, activity_db: pathlib.Path, mock_repo: pathlib.Path
    ) -> None:
        with mock.patch("loom_tools.agent_metrics.find_repo_root", return_value=mock_repo):
            with mock.patch(
                "loom_tools.agent_metrics._get_activity_db_path", return_value=activity_db
            ):
                rc = main(["--format", "json", "--period", "all"])
        assert rc == 0


# ---------------------------------------------------------------------------
# Bash stub integration test
# ---------------------------------------------------------------------------


class TestBashStub:
    """Test that the bash stub structure is correct."""

    def test_stub_delegates_correctly(self) -> None:
        """Verify the bash stub script structure delegates to Python."""
        import pathlib

        # Find the stub relative to the test file
        tests_dir = pathlib.Path(__file__).parent
        stub_path = tests_dir.parent.parent.parent / ".loom" / "scripts" / "agent-metrics.sh"

        if stub_path.exists():
            content = stub_path.read_text()
            assert "loom-agent-metrics" in content
            assert "exec" in content
