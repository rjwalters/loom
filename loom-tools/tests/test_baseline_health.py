"""Tests for baseline health model, I/O, and CLI.

Phase 3.3 (#3400): TestCacheStaleness and TestPreflightPhase removed —
shepherd/phases/preflight.py deleted with the shepherd brain.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from loom_tools.common.paths import LoomPaths
from loom_tools.common.state import read_baseline_health, write_baseline_health
from loom_tools.models.baseline_health import BaselineHealth, FailingTest


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
