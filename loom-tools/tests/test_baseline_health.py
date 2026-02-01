"""Tests for baseline health model, I/O, preflight phase, and CLI."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.common.paths import LoomPaths
from loom_tools.common.state import read_baseline_health, write_baseline_health
from loom_tools.models.baseline_health import BaselineHealth, FailingTest
from loom_tools.shepherd.phases.base import PhaseStatus
from loom_tools.shepherd.phases.preflight import PreflightPhase, _is_cache_stale


# ---------------------------------------------------------------------------
# Model tests
# ---------------------------------------------------------------------------


class TestBaselineHealthModel:
    """Tests for the BaselineHealth dataclass."""

    def test_default_values(self) -> None:
        health = BaselineHealth()
        assert health.status == "unknown"
        assert health.checked_at == ""
        assert health.main_commit == ""
        assert health.failing_tests == []
        assert health.issue_tracking == ""
        assert health.cache_ttl_minutes == 15

    def test_from_dict_full(self) -> None:
        data = {
            "status": "failing",
            "checked_at": "2026-02-01T10:00:00+00:00",
            "main_commit": "abc1234",
            "failing_tests": [
                {"name": "test_foo", "ecosystem": "pytest", "failure_message": "AssertionError"}
            ],
            "issue_tracking": "#2042",
            "cache_ttl_minutes": 30,
        }
        health = BaselineHealth.from_dict(data)
        assert health.status == "failing"
        assert health.main_commit == "abc1234"
        assert len(health.failing_tests) == 1
        assert health.failing_tests[0].name == "test_foo"
        assert health.failing_tests[0].ecosystem == "pytest"
        assert health.issue_tracking == "#2042"
        assert health.cache_ttl_minutes == 30

    def test_from_dict_minimal(self) -> None:
        health = BaselineHealth.from_dict({"status": "healthy"})
        assert health.status == "healthy"
        assert health.failing_tests == []

    def test_to_dict_round_trip(self) -> None:
        health = BaselineHealth(
            status="failing",
            checked_at="2026-02-01T10:00:00+00:00",
            main_commit="abc1234",
            failing_tests=[FailingTest(name="test_bar")],
            issue_tracking="#100",
        )
        data = health.to_dict()
        restored = BaselineHealth.from_dict(data)
        assert restored.status == health.status
        assert restored.main_commit == health.main_commit
        assert len(restored.failing_tests) == 1
        assert restored.failing_tests[0].name == "test_bar"


# ---------------------------------------------------------------------------
# State I/O tests
# ---------------------------------------------------------------------------


class TestBaselineHealthIO:
    """Tests for read/write functions."""

    def test_read_missing_file(self, tmp_path: Path) -> None:
        health = read_baseline_health(tmp_path)
        assert health.status == "unknown"

    def test_write_and_read(self, tmp_path: Path) -> None:
        (tmp_path / ".loom").mkdir()
        health = BaselineHealth(
            status="healthy",
            checked_at="2026-02-01T10:00:00+00:00",
            main_commit="deadbeef",
        )
        write_baseline_health(tmp_path, health)
        restored = read_baseline_health(tmp_path)
        assert restored.status == "healthy"
        assert restored.main_commit == "deadbeef"

    def test_read_corrupted_file(self, tmp_path: Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "baseline-health.json").write_text("not json")
        health = read_baseline_health(tmp_path)
        assert health.status == "unknown"

    def test_read_list_returns_default(self, tmp_path: Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "baseline-health.json").write_text("[]")
        health = read_baseline_health(tmp_path)
        assert health.status == "unknown"


# ---------------------------------------------------------------------------
# Cache staleness tests
# ---------------------------------------------------------------------------


class TestCacheStaleness:
    """Tests for _is_cache_stale."""

    def test_empty_timestamp_is_stale(self) -> None:
        assert _is_cache_stale("", 15) is True

    def test_invalid_timestamp_is_stale(self) -> None:
        assert _is_cache_stale("not-a-timestamp", 15) is True

    def test_recent_timestamp_is_fresh(self) -> None:
        recent = datetime.now(timezone.utc).isoformat()
        assert _is_cache_stale(recent, 15) is False

    def test_old_timestamp_is_stale(self) -> None:
        old = (datetime.now(timezone.utc) - timedelta(minutes=30)).isoformat()
        assert _is_cache_stale(old, 15) is True

    def test_exactly_at_ttl_is_not_stale(self) -> None:
        # 14 minutes ago with 15 min TTL should be fresh
        ts = (datetime.now(timezone.utc) - timedelta(minutes=14)).isoformat()
        assert _is_cache_stale(ts, 15) is False

    def test_naive_timestamp_treated_as_utc(self) -> None:
        # Naive timestamps are treated as UTC
        recent = datetime.now(timezone.utc).replace(tzinfo=None).isoformat()
        assert _is_cache_stale(recent, 15) is False


# ---------------------------------------------------------------------------
# PreflightPhase tests
# ---------------------------------------------------------------------------


def _make_ctx(tmp_path: Path, **overrides: object) -> MagicMock:
    """Create a minimal mock ShepherdContext."""
    ctx = MagicMock()
    ctx.repo_root = tmp_path
    ctx.config.issue = 42
    ctx.config.should_skip_phase.return_value = False
    ctx.config.is_force_mode = False
    for k, v in overrides.items():
        setattr(ctx.config, k, v)
    return ctx


def _write_health(tmp_path: Path, **kwargs: object) -> None:
    """Write a baseline-health.json file."""
    loom_dir = tmp_path / ".loom"
    loom_dir.mkdir(exist_ok=True)
    data = {
        "status": "unknown",
        "checked_at": "",
        "main_commit": "",
        "failing_tests": [],
        "issue_tracking": "",
        "cache_ttl_minutes": 15,
    }
    data.update(kwargs)
    (loom_dir / "baseline-health.json").write_text(json.dumps(data))


class TestPreflightPhase:
    """Tests for the PreflightPhase."""

    def test_skip_when_builder_skipped(self, tmp_path: Path) -> None:
        ctx = _make_ctx(tmp_path)
        ctx.config.should_skip_phase.return_value = True
        phase = PreflightPhase()
        skip, reason = phase.should_skip(ctx)
        assert skip is True
        assert "builder phase skipped" in reason

    def test_no_cache_file_succeeds(self, tmp_path: Path) -> None:
        ctx = _make_ctx(tmp_path)
        phase = PreflightPhase()
        result = phase.run(ctx)
        assert result.status == PhaseStatus.SUCCESS
        assert result.data["baseline_status"] == "unknown"

    def test_healthy_baseline_succeeds(self, tmp_path: Path) -> None:
        _write_health(
            tmp_path,
            status="healthy",
            checked_at=datetime.now(timezone.utc).isoformat(),
            main_commit="abc1234",
        )
        ctx = _make_ctx(tmp_path)
        phase = PreflightPhase()
        result = phase.run(ctx)
        assert result.status == PhaseStatus.SUCCESS
        assert result.data["baseline_status"] == "healthy"

    @patch("loom_tools.shepherd.phases.preflight._get_main_head")
    def test_failing_baseline_blocks(self, mock_head: MagicMock, tmp_path: Path) -> None:
        commit = "abc1234567890"
        mock_head.return_value = commit
        _write_health(
            tmp_path,
            status="failing",
            checked_at=datetime.now(timezone.utc).isoformat(),
            main_commit=commit,
            failing_tests=[{"name": "test_foo", "ecosystem": "", "failure_message": ""}],
            issue_tracking="#2042",
        )
        ctx = _make_ctx(tmp_path)
        phase = PreflightPhase()
        result = phase.run(ctx)
        assert result.status == PhaseStatus.FAILED
        assert result.data["baseline_status"] == "failing"
        assert "test_foo" in result.data["failing_tests"]
        assert result.data["issue_tracking"] == "#2042"

    def test_stale_failing_cache_proceeds(self, tmp_path: Path) -> None:
        old_time = (datetime.now(timezone.utc) - timedelta(minutes=30)).isoformat()
        _write_health(
            tmp_path,
            status="failing",
            checked_at=old_time,
            main_commit="abc1234",
        )
        ctx = _make_ctx(tmp_path)
        phase = PreflightPhase()
        result = phase.run(ctx)
        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("cache_stale") is True

    @patch("loom_tools.shepherd.phases.preflight._get_main_head")
    def test_commit_mismatch_proceeds(self, mock_head: MagicMock, tmp_path: Path) -> None:
        mock_head.return_value = "newcommit123"
        _write_health(
            tmp_path,
            status="failing",
            checked_at=datetime.now(timezone.utc).isoformat(),
            main_commit="oldcommit456",
        )
        ctx = _make_ctx(tmp_path)
        phase = PreflightPhase()
        result = phase.run(ctx)
        assert result.status == PhaseStatus.SUCCESS
        assert result.data.get("commit_mismatch") is True

    def test_unexpected_status_proceeds(self, tmp_path: Path) -> None:
        _write_health(tmp_path, status="weird_value")
        ctx = _make_ctx(tmp_path)
        phase = PreflightPhase()
        result = phase.run(ctx)
        assert result.status == PhaseStatus.SUCCESS

    def test_validate_always_true(self, tmp_path: Path) -> None:
        ctx = _make_ctx(tmp_path)
        phase = PreflightPhase()
        assert phase.validate(ctx) is True


# ---------------------------------------------------------------------------
# CLI tests
# ---------------------------------------------------------------------------


class TestBaselineHealthCLI:
    """Tests for the loom-baseline-health CLI."""

    def test_report_healthy(self, tmp_path: Path) -> None:
        from loom_tools.baseline_health_cli import main as cli_main

        with patch("loom_tools.baseline_health_cli.find_repo_root", return_value=tmp_path):
            (tmp_path / ".loom").mkdir()
            exit_code = cli_main(["report", "--status", "healthy"])
        assert exit_code == 0
        health = read_baseline_health(tmp_path)
        assert health.status == "healthy"

    def test_report_failing_with_tests(self, tmp_path: Path) -> None:
        from loom_tools.baseline_health_cli import main as cli_main

        with patch("loom_tools.baseline_health_cli.find_repo_root", return_value=tmp_path):
            (tmp_path / ".loom").mkdir()
            exit_code = cli_main([
                "report",
                "--status", "failing",
                "--test", "test_foo",
                "--test", "test_bar",
                "--issue", "#2042",
            ])
        assert exit_code == 0
        health = read_baseline_health(tmp_path)
        assert health.status == "failing"
        assert len(health.failing_tests) == 2
        assert health.failing_tests[0].name == "test_foo"
        assert health.issue_tracking == "#2042"

    def test_check_healthy(self, tmp_path: Path) -> None:
        from loom_tools.baseline_health_cli import main as cli_main

        (tmp_path / ".loom").mkdir()
        write_baseline_health(tmp_path, BaselineHealth(status="healthy"))
        with patch("loom_tools.baseline_health_cli.find_repo_root", return_value=tmp_path):
            exit_code = cli_main(["check"])
        assert exit_code == 0

    def test_check_failing(self, tmp_path: Path) -> None:
        from loom_tools.baseline_health_cli import main as cli_main

        (tmp_path / ".loom").mkdir()
        write_baseline_health(tmp_path, BaselineHealth(status="failing"))
        with patch("loom_tools.baseline_health_cli.find_repo_root", return_value=tmp_path):
            exit_code = cli_main(["check"])
        assert exit_code == 1

    def test_check_unknown(self, tmp_path: Path) -> None:
        from loom_tools.baseline_health_cli import main as cli_main

        with patch("loom_tools.baseline_health_cli.find_repo_root", return_value=tmp_path):
            exit_code = cli_main(["check"])
        assert exit_code == 2

    def test_show_healthy(self, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
        from loom_tools.baseline_health_cli import main as cli_main

        (tmp_path / ".loom").mkdir()
        write_baseline_health(
            tmp_path,
            BaselineHealth(status="healthy", main_commit="abc123"),
        )
        with patch("loom_tools.baseline_health_cli.find_repo_root", return_value=tmp_path):
            exit_code = cli_main(["show"])
        assert exit_code == 0
        output = capsys.readouterr().out
        assert "Status: healthy" in output


# ---------------------------------------------------------------------------
# LoomPaths integration test
# ---------------------------------------------------------------------------


class TestLoomPathsBaseline:
    """Tests for baseline_health_file path property."""

    def test_baseline_health_file_path(self, tmp_path: Path) -> None:
        paths = LoomPaths(tmp_path)
        assert paths.baseline_health_file == tmp_path / ".loom" / "baseline-health.json"
