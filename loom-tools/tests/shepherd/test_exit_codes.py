"""Tests for shepherd exit codes."""

from __future__ import annotations

import pytest

from loom_tools.shepherd.exit_codes import (
    EXIT_CODE_DESCRIPTIONS,
    ShepherdExitCode,
    describe_exit_code,
)


class TestShepherdExitCode:
    """Test ShepherdExitCode enum."""

    def test_values_are_distinct(self) -> None:
        """All exit codes should have distinct values."""
        values = [code.value for code in ShepherdExitCode]
        assert len(values) == len(set(values))

    def test_success_is_zero(self) -> None:
        """SUCCESS should be 0 for standard shell compatibility."""
        assert ShepherdExitCode.SUCCESS == 0

    def test_builder_failed_is_one(self) -> None:
        """BUILDER_FAILED should be 1 for backwards compatibility."""
        assert ShepherdExitCode.BUILDER_FAILED == 1

    def test_pr_tests_failed_is_two(self) -> None:
        """PR_TESTS_FAILED should be 2."""
        assert ShepherdExitCode.PR_TESTS_FAILED == 2

    def test_shutdown_is_three(self) -> None:
        """SHUTDOWN should be 3."""
        assert ShepherdExitCode.SHUTDOWN == 3

    def test_needs_intervention_is_four(self) -> None:
        """NEEDS_INTERVENTION should be 4."""
        assert ShepherdExitCode.NEEDS_INTERVENTION == 4

    def test_skipped_is_five(self) -> None:
        """SKIPPED should be 5."""
        assert ShepherdExitCode.SKIPPED == 5

    def test_can_use_as_int(self) -> None:
        """Exit codes should be usable as integers."""
        assert int(ShepherdExitCode.SUCCESS) == 0
        assert int(ShepherdExitCode.BUILDER_FAILED) == 1

    def test_can_compare_with_int(self) -> None:
        """Exit codes should be comparable to integers."""
        assert ShepherdExitCode.SUCCESS == 0
        assert ShepherdExitCode.BUILDER_FAILED == 1
        assert ShepherdExitCode.PR_TESTS_FAILED == 2

    def test_can_create_from_int(self) -> None:
        """Exit codes should be creatable from integers."""
        assert ShepherdExitCode(0) == ShepherdExitCode.SUCCESS
        assert ShepherdExitCode(1) == ShepherdExitCode.BUILDER_FAILED
        assert ShepherdExitCode(2) == ShepherdExitCode.PR_TESTS_FAILED


class TestDescribeExitCode:
    """Test describe_exit_code function."""

    def test_describes_success(self) -> None:
        """Should describe SUCCESS exit code."""
        desc = describe_exit_code(0)
        assert "success" in desc.lower()

    def test_describes_builder_failed(self) -> None:
        """Should describe BUILDER_FAILED exit code."""
        desc = describe_exit_code(1)
        assert "builder" in desc.lower() or "failed" in desc.lower()

    def test_describes_pr_tests_failed(self) -> None:
        """Should describe PR_TESTS_FAILED exit code."""
        desc = describe_exit_code(2)
        assert "test" in desc.lower() or "pr" in desc.lower()

    def test_describes_shutdown(self) -> None:
        """Should describe SHUTDOWN exit code."""
        desc = describe_exit_code(3)
        assert "shutdown" in desc.lower()

    def test_describes_needs_intervention(self) -> None:
        """Should describe NEEDS_INTERVENTION exit code."""
        desc = describe_exit_code(4)
        assert "intervention" in desc.lower() or "stuck" in desc.lower()

    def test_describes_skipped(self) -> None:
        """Should describe SKIPPED exit code."""
        desc = describe_exit_code(5)
        assert "skip" in desc.lower() or "complete" in desc.lower()

    def test_unknown_code_returns_message(self) -> None:
        """Should return message for unknown exit codes."""
        desc = describe_exit_code(99)
        assert "unknown" in desc.lower() or "99" in desc


class TestExitCodeDescriptions:
    """Test EXIT_CODE_DESCRIPTIONS constant."""

    def test_all_codes_have_descriptions(self) -> None:
        """All exit codes should have descriptions."""
        for code in ShepherdExitCode:
            assert code in EXIT_CODE_DESCRIPTIONS
            assert len(EXIT_CODE_DESCRIPTIONS[code]) > 0

    def test_descriptions_are_strings(self) -> None:
        """All descriptions should be strings."""
        for desc in EXIT_CODE_DESCRIPTIONS.values():
            assert isinstance(desc, str)
