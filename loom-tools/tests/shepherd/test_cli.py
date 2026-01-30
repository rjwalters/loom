"""Tests for shepherd CLI."""

from __future__ import annotations

import sys
from io import StringIO
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.cli import _create_config, _parse_args, main
from loom_tools.shepherd.config import ExecutionMode, Phase


class TestParseArgs:
    """Test CLI argument parsing."""

    def test_requires_issue_number(self) -> None:
        """Should require issue number."""
        with pytest.raises(SystemExit):
            _parse_args([])

    def test_parses_issue_number(self) -> None:
        """Should parse issue number."""
        args = _parse_args(["42"])
        assert args.issue == 42

    def test_parses_force_flag(self) -> None:
        """Should parse --force flag."""
        args = _parse_args(["42", "--force"])
        assert args.force is True

    def test_parses_force_short_flag(self) -> None:
        """Should parse -f flag."""
        args = _parse_args(["42", "-f"])
        assert args.force is True

    def test_parses_from_phase(self) -> None:
        """Should parse --from phase."""
        args = _parse_args(["42", "--from", "builder"])
        assert args.start_from == "builder"

    def test_from_validates_phase(self) -> None:
        """Should reject invalid --from phase."""
        with pytest.raises(SystemExit):
            _parse_args(["42", "--from", "invalid"])

    def test_parses_to_phase(self) -> None:
        """Should parse --to phase."""
        args = _parse_args(["42", "--to", "curated"])
        assert args.stop_after == "curated"

    def test_to_validates_phase(self) -> None:
        """Should reject invalid --to phase."""
        with pytest.raises(SystemExit):
            _parse_args(["42", "--to", "invalid"])

    def test_parses_task_id(self) -> None:
        """Should parse --task-id."""
        args = _parse_args(["42", "--task-id", "abc1234"])
        assert args.task_id == "abc1234"


class TestCreateConfig:
    """Test config creation from args."""

    def test_default_mode(self) -> None:
        """Default mode should be DEFAULT."""
        args = _parse_args(["42"])
        config = _create_config(args)
        assert config.mode == ExecutionMode.DEFAULT

    def test_force_mode(self) -> None:
        """--force should set FORCE_MERGE mode."""
        args = _parse_args(["42", "--force"])
        config = _create_config(args)
        assert config.mode == ExecutionMode.FORCE_MERGE

    def test_deprecated_force_merge(self) -> None:
        """--force-merge should set FORCE_MERGE mode with warning."""
        args = _parse_args(["42", "--force-merge"])
        with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            config = _create_config(args)
            assert config.mode == ExecutionMode.FORCE_MERGE
            mock_warn.assert_called()

    def test_deprecated_force_pr(self) -> None:
        """--force-pr should set DEFAULT mode with warning."""
        args = _parse_args(["42", "--force-pr"])
        with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            config = _create_config(args)
            assert config.mode == ExecutionMode.DEFAULT
            mock_warn.assert_called()

    def test_deprecated_wait(self) -> None:
        """--wait should set NORMAL mode with warning."""
        args = _parse_args(["42", "--wait"])
        with patch("loom_tools.shepherd.cli.log_warning") as mock_warn:
            config = _create_config(args)
            assert config.mode == ExecutionMode.NORMAL
            mock_warn.assert_called()

    def test_from_phase_mapping(self) -> None:
        """--from should map to Phase enum."""
        for phase_str, phase_enum in [
            ("curator", Phase.CURATOR),
            ("builder", Phase.BUILDER),
            ("judge", Phase.JUDGE),
            ("merge", Phase.MERGE),
        ]:
            args = _parse_args(["42", "--from", phase_str])
            config = _create_config(args)
            assert config.start_from == phase_enum

    def test_stop_after(self) -> None:
        """--to should set stop_after."""
        args = _parse_args(["42", "--to", "curated"])
        config = _create_config(args)
        assert config.stop_after == "curated"

    def test_task_id(self) -> None:
        """--task-id should set task_id."""
        args = _parse_args(["42", "--task-id", "abc1234"])
        config = _create_config(args)
        assert config.task_id == "abc1234"


class TestMain:
    """Test main entry point."""

    def test_returns_int(self) -> None:
        """main should return an int exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=0):
            with patch("loom_tools.shepherd.cli.ShepherdContext"):
                result = main(["42"])
                assert isinstance(result, int)

    def test_passes_exit_code_from_orchestrate(self) -> None:
        """main should return orchestrate's exit code."""
        with patch("loom_tools.shepherd.cli.orchestrate", return_value=1):
            with patch("loom_tools.shepherd.cli.ShepherdContext"):
                result = main(["42"])
                assert result == 1
