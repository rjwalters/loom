"""Tests for API error detection in agent_spawn.py."""

from __future__ import annotations

import pathlib

import pytest

from loom_tools.agent_spawn import API_ERROR_PATTERNS, check_log_for_api_errors


class TestCheckLogForApiErrors:
    """Tests for check_log_for_api_errors function."""

    def test_no_file(self, tmp_path: pathlib.Path) -> None:
        result = check_log_for_api_errors(tmp_path / "nonexistent.log")
        assert result is None

    def test_empty_file(self, tmp_path: pathlib.Path) -> None:
        log_file = tmp_path / "test.log"
        log_file.write_text("")
        result = check_log_for_api_errors(log_file)
        assert result is None

    def test_normal_log_no_errors(self, tmp_path: pathlib.Path) -> None:
        log_file = tmp_path / "test.log"
        log_file.write_text(
            "Starting agent\n"
            "Working on issue #42\n"
            "Tests passed\n"
            "PR created\n"
        )
        result = check_log_for_api_errors(log_file)
        assert result is None

    @pytest.mark.parametrize("pattern", API_ERROR_PATTERNS)
    def test_detects_each_pattern(self, pattern: str, tmp_path: pathlib.Path) -> None:
        log_file = tmp_path / "test.log"
        log_file.write_text(
            "Starting agent\n"
            "Working on issue #42\n"
            f"Error: {pattern}\n"
            "Waiting for input...\n"
        )
        result = check_log_for_api_errors(log_file)
        assert result is not None

    def test_only_checks_tail(self, tmp_path: pathlib.Path) -> None:
        log_file = tmp_path / "test.log"
        # Put the error far before the tail window
        lines = ["normal line\n"] * 100
        lines.insert(0, "500 Internal Server Error\n")
        log_file.write_text("".join(lines))
        # With tail_lines=50, the error at line 0 should not be found
        result = check_log_for_api_errors(log_file, tail_lines=50)
        assert result is None

    def test_detects_error_in_tail(self, tmp_path: pathlib.Path) -> None:
        log_file = tmp_path / "test.log"
        lines = ["normal line\n"] * 10
        lines.append("API returned 500 Internal Server Error\n")
        lines.extend(["waiting...\n"] * 5)
        log_file.write_text("".join(lines))
        result = check_log_for_api_errors(log_file, tail_lines=50)
        assert result is not None

    def test_case_insensitive(self, tmp_path: pathlib.Path) -> None:
        log_file = tmp_path / "test.log"
        log_file.write_text("RATE LIMIT EXCEEDED\n")
        result = check_log_for_api_errors(log_file)
        assert result is not None
