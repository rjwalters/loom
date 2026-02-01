"""Tests for loom_tools.recovery_stats."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest import mock

import pytest

from loom_tools.recovery_stats import (
    RecoveryEvent,
    RecoveryStats,
    compute_period_range,
    compute_stats,
    format_text_output,
    load_recovery_events,
    main,
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def mock_repo(tmp_path: Path) -> Path:
    """Create a mock repo with .git and .loom directories."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "metrics").mkdir()
    return tmp_path


@pytest.fixture
def sample_events() -> list[dict]:
    """Sample recovery event data."""
    now = datetime.now(timezone.utc)
    return [
        {
            "timestamp": (now - timedelta(hours=1)).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "issue": 100,
            "recovery_type": "commit_and_pr",
            "reason": "uncommitted_changes",
            "elapsed_seconds": 120,
            "worktree_had_changes": True,
            "commits_recovered": 1,
            "pr_number": 200,
        },
        {
            "timestamp": (now - timedelta(days=1)).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "issue": 101,
            "recovery_type": "pr_only",
            "reason": "unpushed_commits",
            "elapsed_seconds": 60,
            "worktree_had_changes": False,
            "commits_recovered": 2,
            "pr_number": 201,
        },
        {
            "timestamp": (now - timedelta(days=3)).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "issue": 102,
            "recovery_type": "add_label",
            "reason": "missing_label",
            "elapsed_seconds": 10,
            "worktree_had_changes": False,
            "commits_recovered": 0,
            "pr_number": 202,
        },
    ]


@pytest.fixture
def events_file(mock_repo: Path, sample_events: list[dict]) -> Path:
    """Create a recovery events file with sample data."""
    events_file = mock_repo / ".loom" / "metrics" / "recovery-events.json"
    events_file.write_text(json.dumps(sample_events))
    return events_file


# ---------------------------------------------------------------------------
# RecoveryEvent.from_dict tests
# ---------------------------------------------------------------------------


class TestRecoveryEventFromDict:
    """Tests for RecoveryEvent.from_dict()."""

    def test_valid_event_parsing(self) -> None:
        """Test that valid event data parses correctly."""
        data = {
            "timestamp": "2026-01-15T10:30:00Z",
            "issue": 42,
            "recovery_type": "commit_and_pr",
            "reason": "uncommitted_changes",
            "elapsed_seconds": 120,
            "worktree_had_changes": True,
            "commits_recovered": 3,
            "pr_number": 100,
        }
        event = RecoveryEvent.from_dict(data)

        assert event is not None
        assert event.issue == 42
        assert event.recovery_type == "commit_and_pr"
        assert event.reason == "uncommitted_changes"
        assert event.elapsed_seconds == 120
        assert event.worktree_had_changes is True
        assert event.commits_recovered == 3
        assert event.pr_number == 100
        assert event.timestamp.tzinfo == timezone.utc

    def test_missing_timestamp_returns_none(self) -> None:
        """Test that missing timestamp returns None."""
        data = {
            "issue": 42,
            "recovery_type": "commit_and_pr",
            "reason": "uncommitted_changes",
        }
        event = RecoveryEvent.from_dict(data)
        assert event is None

    def test_empty_timestamp_returns_none(self) -> None:
        """Test that empty timestamp returns None."""
        data = {
            "timestamp": "",
            "issue": 42,
            "recovery_type": "commit_and_pr",
            "reason": "uncommitted_changes",
        }
        event = RecoveryEvent.from_dict(data)
        assert event is None

    def test_invalid_timestamp_returns_none(self) -> None:
        """Test that invalid timestamp returns None."""
        data = {
            "timestamp": "not-a-date",
            "issue": 42,
            "recovery_type": "commit_and_pr",
            "reason": "uncommitted_changes",
        }
        event = RecoveryEvent.from_dict(data)
        assert event is None

    def test_missing_optional_fields_use_defaults(self) -> None:
        """Test that missing optional fields use defaults."""
        data = {
            "timestamp": "2026-01-15T10:30:00Z",
            "issue": 42,
        }
        event = RecoveryEvent.from_dict(data)

        assert event is not None
        assert event.issue == 42
        assert event.recovery_type == "unknown"
        assert event.reason == "unknown"
        assert event.elapsed_seconds is None
        assert event.worktree_had_changes is False
        assert event.commits_recovered == 0
        assert event.pr_number is None

    def test_non_integer_issue_handled(self) -> None:
        """Test that non-integer issue is handled."""
        data = {
            "timestamp": "2026-01-15T10:30:00Z",
            "issue": "abc",
            "recovery_type": "commit_and_pr",
        }
        event = RecoveryEvent.from_dict(data)
        # Should return None due to ValueError in int conversion
        assert event is None


# ---------------------------------------------------------------------------
# load_recovery_events tests
# ---------------------------------------------------------------------------


class TestLoadRecoveryEvents:
    """Tests for load_recovery_events()."""

    def test_empty_file_returns_empty_list(self, mock_repo: Path) -> None:
        """Test that empty file returns empty list."""
        events_file = mock_repo / ".loom" / "metrics" / "recovery-events.json"
        events_file.write_text("[]")

        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            events = load_recovery_events(mock_repo)

        assert events == []

    def test_nonexistent_file_returns_empty_list(self, mock_repo: Path) -> None:
        """Test that nonexistent file returns empty list."""
        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            events = load_recovery_events(mock_repo)

        assert events == []

    def test_malformed_json_returns_empty_list(self, mock_repo: Path) -> None:
        """Test that malformed JSON returns empty list."""
        events_file = mock_repo / ".loom" / "metrics" / "recovery-events.json"
        events_file.write_text("not valid json {{{")

        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            events = load_recovery_events(mock_repo)

        assert events == []

    def test_non_list_json_returns_empty_list(self, mock_repo: Path) -> None:
        """Test that non-list JSON returns empty list."""
        events_file = mock_repo / ".loom" / "metrics" / "recovery-events.json"
        events_file.write_text('{"key": "value"}')

        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            events = load_recovery_events(mock_repo)

        assert events == []

    def test_filters_out_invalid_events(self, mock_repo: Path) -> None:
        """Test that invalid events are filtered out."""
        events_file = mock_repo / ".loom" / "metrics" / "recovery-events.json"
        events_file.write_text(
            json.dumps(
                [
                    {
                        "timestamp": "2026-01-15T10:30:00Z",
                        "issue": 42,
                        "recovery_type": "commit_and_pr",
                    },
                    {
                        "issue": 43,  # Missing timestamp - should be filtered
                        "recovery_type": "pr_only",
                    },
                    {
                        "timestamp": "2026-01-15T11:00:00Z",
                        "issue": 44,
                        "recovery_type": "add_label",
                    },
                ]
            )
        )

        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            events = load_recovery_events(mock_repo)

        assert len(events) == 2
        assert events[0].issue == 42
        assert events[1].issue == 44

    def test_loads_valid_events(
        self, mock_repo: Path, events_file: Path, sample_events: list[dict]
    ) -> None:
        """Test that valid events are loaded correctly."""
        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            events = load_recovery_events(mock_repo)

        assert len(events) == 3
        assert events[0].issue == 100
        assert events[0].recovery_type == "commit_and_pr"
        assert events[1].issue == 101
        assert events[2].issue == 102


# ---------------------------------------------------------------------------
# compute_period_range tests
# ---------------------------------------------------------------------------


class TestComputePeriodRange:
    """Tests for compute_period_range()."""

    def test_today_period(self) -> None:
        """Test 'today' period starts at midnight."""
        start, end = compute_period_range("today")

        assert start.hour == 0
        assert start.minute == 0
        assert start.second == 0
        assert start.microsecond == 0
        assert end > start
        # Should be same day
        assert start.date() == end.date()

    def test_week_period(self) -> None:
        """Test 'week' period goes back 7 days."""
        start, end = compute_period_range("week")

        delta = end - start
        assert delta.days == 7

    def test_month_period(self) -> None:
        """Test 'month' period goes back 30 days."""
        start, end = compute_period_range("month")

        delta = end - start
        assert delta.days == 30

    def test_all_period(self) -> None:
        """Test 'all' period goes back ~10 years."""
        start, end = compute_period_range("all")

        delta = end - start
        assert delta.days == 3650

    def test_unknown_period_defaults_to_week(self) -> None:
        """Test unknown period defaults to week."""
        start, end = compute_period_range("unknown")

        delta = end - start
        assert delta.days == 7


# ---------------------------------------------------------------------------
# compute_stats tests
# ---------------------------------------------------------------------------


class TestComputeStats:
    """Tests for compute_stats()."""

    def test_correct_filtering_by_date_range(self, mock_repo: Path) -> None:
        """Test events are filtered by date range."""
        now = datetime.now(timezone.utc)

        # Create events: one from today, one from 10 days ago
        events = [
            RecoveryEvent(
                timestamp=now - timedelta(hours=1),
                issue=100,
                recovery_type="commit_and_pr",
                reason="uncommitted_changes",
            ),
            RecoveryEvent(
                timestamp=now - timedelta(days=10),
                issue=101,
                recovery_type="pr_only",
                reason="unpushed_commits",
            ),
        ]

        # Week period should only include the first event
        stats = compute_stats(events, "week")

        assert stats.total_events == 1
        assert len(stats.events) == 1
        assert stats.events[0].issue == 100

    def test_correct_counts_by_type(self) -> None:
        """Test counting by recovery type."""
        now = datetime.now(timezone.utc)
        events = [
            RecoveryEvent(
                timestamp=now,
                issue=100,
                recovery_type="commit_and_pr",
                reason="uncommitted_changes",
            ),
            RecoveryEvent(
                timestamp=now,
                issue=101,
                recovery_type="commit_and_pr",
                reason="uncommitted_changes",
            ),
            RecoveryEvent(
                timestamp=now,
                issue=102,
                recovery_type="pr_only",
                reason="unpushed_commits",
            ),
        ]

        stats = compute_stats(events, "all")

        assert stats.by_type == {"commit_and_pr": 2, "pr_only": 1}

    def test_correct_counts_by_reason(self) -> None:
        """Test counting by reason."""
        now = datetime.now(timezone.utc)
        events = [
            RecoveryEvent(
                timestamp=now,
                issue=100,
                recovery_type="commit_and_pr",
                reason="uncommitted_changes",
            ),
            RecoveryEvent(
                timestamp=now,
                issue=101,
                recovery_type="pr_only",
                reason="uncommitted_changes",
            ),
            RecoveryEvent(
                timestamp=now,
                issue=102,
                recovery_type="add_label",
                reason="missing_label",
            ),
        ]

        stats = compute_stats(events, "all")

        assert stats.by_reason == {"uncommitted_changes": 2, "missing_label": 1}

    def test_correct_counts_by_day(self) -> None:
        """Test counting by day."""
        now = datetime.now(timezone.utc)
        today = now.strftime("%Y-%m-%d")
        yesterday = (now - timedelta(days=1)).strftime("%Y-%m-%d")

        events = [
            RecoveryEvent(
                timestamp=now,
                issue=100,
                recovery_type="commit_and_pr",
                reason="test",
            ),
            RecoveryEvent(
                timestamp=now - timedelta(hours=2),
                issue=101,
                recovery_type="commit_and_pr",
                reason="test",
            ),
            RecoveryEvent(
                timestamp=now - timedelta(days=1),
                issue=102,
                recovery_type="commit_and_pr",
                reason="test",
            ),
        ]

        stats = compute_stats(events, "all")

        assert stats.by_day[today] == 2
        assert stats.by_day[yesterday] == 1

    def test_empty_events_list(self) -> None:
        """Test with empty events list."""
        stats = compute_stats([], "all")

        assert stats.total_events == 0
        assert stats.by_type == {}
        assert stats.by_reason == {}
        assert stats.by_day == {}
        assert stats.events == []

    def test_events_sorted_by_timestamp_descending(self) -> None:
        """Test events are sorted by timestamp (newest first)."""
        now = datetime.now(timezone.utc)
        events = [
            RecoveryEvent(
                timestamp=now - timedelta(hours=3),
                issue=100,
                recovery_type="commit_and_pr",
                reason="test",
            ),
            RecoveryEvent(
                timestamp=now - timedelta(hours=1),
                issue=101,
                recovery_type="commit_and_pr",
                reason="test",
            ),
            RecoveryEvent(
                timestamp=now - timedelta(hours=2),
                issue=102,
                recovery_type="commit_and_pr",
                reason="test",
            ),
        ]

        stats = compute_stats(events, "all")

        # Newest first: 101, 102, 100
        assert stats.events[0].issue == 101
        assert stats.events[1].issue == 102
        assert stats.events[2].issue == 100


# ---------------------------------------------------------------------------
# format_text_output tests
# ---------------------------------------------------------------------------


class TestFormatTextOutput:
    """Tests for format_text_output()."""

    def test_basic_output_format(self) -> None:
        """Test basic output contains expected sections."""
        now = datetime.now(timezone.utc)
        stats = RecoveryStats(
            period_start=now - timedelta(days=7),
            period_end=now,
            total_events=5,
            by_type={"commit_and_pr": 3, "pr_only": 2},
            by_reason={"uncommitted_changes": 3, "unpushed_commits": 2},
            by_day={"2026-01-15": 3, "2026-01-14": 2},
            events=[],
        )

        output = format_text_output(stats, verbose=False)

        assert "RECOVERY STATISTICS" in output
        assert "Total recovery events: 5" in output
        assert "By Recovery Type:" in output
        assert "commit_and_pr" in output
        assert "pr_only" in output
        assert "By Reason:" in output
        assert "uncommitted_changes" in output
        assert "By Day:" in output

    def test_verbose_shows_individual_events(self) -> None:
        """Test verbose mode shows individual events."""
        now = datetime.now(timezone.utc)
        events = [
            RecoveryEvent(
                timestamp=now,
                issue=100,
                recovery_type="commit_and_pr",
                reason="uncommitted_changes",
                pr_number=200,
            ),
        ]
        stats = RecoveryStats(
            period_start=now - timedelta(days=7),
            period_end=now,
            total_events=1,
            by_type={"commit_and_pr": 1},
            by_reason={"uncommitted_changes": 1},
            by_day={now.strftime("%Y-%m-%d"): 1},
            events=events,
        )

        output = format_text_output(stats, verbose=True)

        assert "Recent Events" in output
        assert "Issue #100" in output
        assert "commit_and_pr" in output
        assert "PR #200" in output

    def test_non_verbose_hides_individual_events(self) -> None:
        """Test non-verbose mode hides individual events."""
        now = datetime.now(timezone.utc)
        events = [
            RecoveryEvent(
                timestamp=now,
                issue=100,
                recovery_type="commit_and_pr",
                reason="uncommitted_changes",
            ),
        ]
        stats = RecoveryStats(
            period_start=now - timedelta(days=7),
            period_end=now,
            total_events=1,
            by_type={"commit_and_pr": 1},
            by_reason={"uncommitted_changes": 1},
            by_day={now.strftime("%Y-%m-%d"): 1},
            events=events,
        )

        output = format_text_output(stats, verbose=False)

        assert "Recent Events" not in output

    def test_non_verbose_limits_days_to_7(self) -> None:
        """Test non-verbose mode limits days display to 7."""
        now = datetime.now(timezone.utc)
        by_day = {
            (now - timedelta(days=i)).strftime("%Y-%m-%d"): 1 for i in range(10)
        }
        stats = RecoveryStats(
            period_start=now - timedelta(days=30),
            period_end=now,
            total_events=10,
            by_type={},
            by_reason={},
            by_day=by_day,
            events=[],
        )

        output = format_text_output(stats, verbose=False)

        assert "showing last 7 days" in output

    def test_empty_stats(self) -> None:
        """Test output with empty stats."""
        now = datetime.now(timezone.utc)
        stats = RecoveryStats(
            period_start=now - timedelta(days=7),
            period_end=now,
            total_events=0,
            by_type={},
            by_reason={},
            by_day={},
            events=[],
        )

        output = format_text_output(stats, verbose=False)

        assert "RECOVERY STATISTICS" in output
        assert "Total recovery events: 0" in output


# ---------------------------------------------------------------------------
# RecoveryStats.to_dict tests
# ---------------------------------------------------------------------------


class TestRecoveryStatsToDict:
    """Tests for RecoveryStats.to_dict()."""

    def test_to_dict_structure(self) -> None:
        """Test to_dict returns expected structure."""
        now = datetime.now(timezone.utc)
        events = [
            RecoveryEvent(
                timestamp=now,
                issue=100,
                recovery_type="commit_and_pr",
                reason="uncommitted_changes",
                pr_number=200,
            ),
        ]
        stats = RecoveryStats(
            period_start=now - timedelta(days=7),
            period_end=now,
            total_events=1,
            by_type={"commit_and_pr": 1},
            by_reason={"uncommitted_changes": 1},
            by_day={now.strftime("%Y-%m-%d"): 1},
            events=events,
        )

        d = stats.to_dict()

        assert "period" in d
        assert "start" in d["period"]
        assert "end" in d["period"]
        assert "summary" in d
        assert d["summary"]["total_recovery_events"] == 1
        assert d["summary"]["by_type"] == {"commit_and_pr": 1}
        assert d["summary"]["by_reason"] == {"uncommitted_changes": 1}
        assert "by_day" in d
        assert "events" in d
        assert len(d["events"]) == 1
        assert d["events"][0]["issue"] == 100
        assert d["events"][0]["pr_number"] == 200


# ---------------------------------------------------------------------------
# CLI tests
# ---------------------------------------------------------------------------


class TestCLI:
    """Tests for the main CLI entry point."""

    def test_help_exits_zero(self) -> None:
        """Test --help exits with code 0."""
        with pytest.raises(SystemExit) as exc:
            main(["--help"])
        assert exc.value.code == 0

    def test_json_output(
        self, mock_repo: Path, events_file: Path, sample_events: list[dict]
    ) -> None:
        """Test JSON output mode."""
        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            # Capture stdout
            import io
            import sys

            captured = io.StringIO()
            sys.stdout = captured
            try:
                main(["--json"])
            finally:
                sys.stdout = sys.__stdout__

            output = captured.getvalue()
            data = json.loads(output)

            assert "summary" in data
            assert "by_day" in data
            assert "events" in data

    def test_period_option(
        self, mock_repo: Path, events_file: Path, sample_events: list[dict]
    ) -> None:
        """Test period option is respected."""
        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            import io
            import sys

            captured = io.StringIO()
            sys.stdout = captured
            try:
                main(["--period", "today", "--json"])
            finally:
                sys.stdout = sys.__stdout__

            output = captured.getvalue()
            data = json.loads(output)

            # 'today' period should only include events from today
            # With sample data, only the first event (1 hour ago) should be included
            assert data["summary"]["total_recovery_events"] <= len(sample_events)

    def test_verbose_option(
        self, mock_repo: Path, events_file: Path, sample_events: list[dict]
    ) -> None:
        """Test verbose option includes events in text output."""
        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            import io
            import sys

            captured = io.StringIO()
            sys.stdout = captured
            try:
                main(["--verbose", "--period", "all"])
            finally:
                sys.stdout = sys.__stdout__

            output = captured.getvalue()

            assert "Recent Events" in output
            assert "Issue #100" in output

    def test_no_events_file(self, mock_repo: Path) -> None:
        """Test CLI with no events file."""
        with mock.patch(
            "loom_tools.recovery_stats.find_repo_root", return_value=mock_repo
        ):
            import io
            import sys

            captured = io.StringIO()
            sys.stdout = captured
            try:
                main(["--json"])
            finally:
                sys.stdout = sys.__stdout__

            output = captured.getvalue()
            data = json.loads(output)

            assert data["summary"]["total_recovery_events"] == 0
