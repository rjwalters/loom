"""Tests for gather_zero_output_postmortem diagnostic function.

See issue #2766.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from loom_tools.shepherd.phases.base import gather_zero_output_postmortem


@pytest.fixture
def log_dir(tmp_path: Path) -> Path:
    """Create a temporary log directory."""
    d = tmp_path / "logs"
    d.mkdir()
    return d


class TestGatherZeroOutputPostmortem:
    """Tests for the gather_zero_output_postmortem function."""

    def test_missing_log_file(self, log_dir: Path) -> None:
        """Returns diagnostic dict when log file doesn't exist."""
        log_path = log_dir / "nonexistent.log"
        result = gather_zero_output_postmortem(
            log_path, wait_exit_code=1, wall_clock_seconds=5.0
        )
        assert result["log_exists"] is False
        assert result["wait_exit_code"] == 1
        assert result["wall_clock_seconds"] == 5.0
        assert "no log file" in result["summary"]

    def test_cli_never_started(self, log_dir: Path) -> None:
        """Detects when CLI never started (no sentinel)."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Pre-flight starting\n"
            "[2026-02-18T07:00:01Z] [ERROR] API endpoint unreachable\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["cli_started"] is False
        assert "CLI never started" in result["summary"]
        assert result["log_errors"] == ["API endpoint unreachable"]

    def test_cli_started_zero_output(self, log_dir: Path) -> None:
        """Detects when CLI started but produced zero output."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting CLI\n"
            "# CLAUDE_CLI_START\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["cli_started"] is True
        assert result["cli_output_chars_cleaned"] == 0
        assert "zero output" in result["summary"]

    def test_auth_preflight_failure_detected(self, log_dir: Path) -> None:
        """Detects auth pre-flight failure sentinel."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting\n"
            "# AUTH_PREFLIGHT_FAILED\n"
            "[2026-02-18T07:00:01Z] [ERROR] Authentication check failed\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["auth_preflight_failed"] is True
        assert "auth pre-flight FAILED" in result["summary"]

    def test_mcp_preflight_failure_detected(self, log_dir: Path) -> None:
        """Detects MCP pre-flight failure sentinel."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting\n"
            "# MCP_PREFLIGHT_FAILED\n"
            "# CLAUDE_CLI_START\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["mcp_preflight_failed"] is True
        assert "MCP pre-flight FAILED" in result["summary"]

    def test_rate_limit_detected(self, log_dir: Path) -> None:
        """Detects rate limit indicators in CLI output."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting\n"
            "# CLAUDE_CLI_START\n"
            "You've used 95% of your weekly limit\n"
            "Stop and wait for limit to reset\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["has_rate_limit"] is True
        assert len(result["rate_limit_indicators"]) >= 1
        assert "rate limit detected" in result["summary"]

    def test_cli_lifetime_estimation(self, log_dir: Path) -> None:
        """Estimates CLI lifetime from log timestamps."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting\n"
            "# CLAUDE_CLI_START\n"
            "[2026-02-18T07:00:03Z] [INFO] Done\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["log_duration_seconds"] == 3.0
        assert result["cli_crashed_on_startup"] is True

    def test_long_session_not_crash(self, log_dir: Path) -> None:
        """Sessions lasting > 5s are not classified as crash-on-startup."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting\n"
            "# CLAUDE_CLI_START\n"
            "[2026-02-18T07:00:30Z] [INFO] Done\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["log_duration_seconds"] == 30.0
        assert result["cli_crashed_on_startup"] is False

    def test_sidecar_exit_code_included(self, log_dir: Path) -> None:
        """Sidecar exit code is included in diagnostics and summary."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting\n"
            "# CLAUDE_CLI_START\n"
        )
        result = gather_zero_output_postmortem(
            log_path,
            wait_exit_code=0,
            sidecar_exit_code=1,
            wall_clock_seconds=10.0,
        )
        assert result["sidecar_exit_code"] == 1
        assert result["wait_exit_code"] == 0
        assert "sidecar=1" in result["summary"]

    def test_wall_clock_in_summary(self, log_dir: Path) -> None:
        """Wall-clock time appears in the summary."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:00:00Z] [INFO] Starting\n"
            "# CLAUDE_CLI_START\n"
        )
        result = gather_zero_output_postmortem(
            log_path, wait_exit_code=1, wall_clock_seconds=45.2
        )
        assert result["wall_clock_seconds"] == 45.2
        assert "wall: 45s" in result["summary"]

    def test_log_tail_included(self, log_dir: Path) -> None:
        """Last 15 lines of log are included."""
        lines = [f"[2026-02-18T07:00:00Z] Line {i}" for i in range(20)]
        log_path = log_dir / "test.log"
        log_path.write_text("\n".join(lines))
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert len(result["log_tail"]) == 15
        assert "Line 19" in result["log_tail"][-1]

    def test_comprehensive_failure(self, log_dir: Path) -> None:
        """Test a realistic zero-output failure scenario."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "[2026-02-18T07:08:04Z] [INFO] Claude CLI found: /opt/homebrew/bin/claude\n"
            "[2026-02-18T07:08:04Z] [INFO] API endpoint reachable (curl)\n"
            "[2026-02-18T07:08:04Z] [INFO] All pre-flight checks passed\n"
            "[2026-02-18T07:08:04Z] [INFO] Starting Claude CLI\n"
            "# CLAUDE_CLI_START\n"
            "[2026-02-18T07:08:04Z] [INFO] Done\n"
        )
        result = gather_zero_output_postmortem(
            log_path,
            wait_exit_code=1,
            sidecar_exit_code=1,
            wall_clock_seconds=5.0,
        )
        assert result["cli_started"] is True
        assert result["auth_preflight_failed"] is False
        assert result["mcp_preflight_failed"] is False
        assert result["has_rate_limit"] is False
        assert result["log_duration_seconds"] == 0.0
        assert result["cli_crashed_on_startup"] is True
        assert "summary" in result

    def test_no_timestamps_graceful(self, log_dir: Path) -> None:
        """Handles logs without parseable timestamps gracefully."""
        log_path = log_dir / "test.log"
        log_path.write_text(
            "Some output without timestamps\n"
            "# CLAUDE_CLI_START\n"
            "More output\n"
        )
        result = gather_zero_output_postmortem(log_path, wait_exit_code=1)
        assert result["log_duration_seconds"] is None
        assert result["cli_crashed_on_startup"] is None
