"""Tests for builder pre-implementation reproducibility check (issue #2316).

Tests the ability to parse test commands from issue markdown and verify
whether bugs are still reproducible on main before running the builder.
"""

from __future__ import annotations

import json
import subprocess
from unittest.mock import MagicMock, patch
from pathlib import Path

import pytest

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases import BuilderPhase, PhaseStatus


@pytest.fixture
def mock_context() -> MagicMock:
    """Create a mock ShepherdContext."""
    ctx = MagicMock(spec=ShepherdContext)
    ctx.config = ShepherdConfig(issue=42)
    ctx.repo_root = Path("/fake/repo")
    ctx.scripts_dir = Path("/fake/repo/.loom/scripts")
    ctx.worktree_path = Path("/fake/repo/.loom/worktrees/issue-42")
    ctx.pr_number = None
    ctx.label_cache = MagicMock()
    return ctx


class TestBuilderExtractTestCommands:
    """Test _extract_test_commands parsing from issue markdown."""

    def test_extracts_pytest_from_code_block(self) -> None:
        """Should extract pytest command from a fenced code block."""
        builder = BuilderPhase()
        text = "Run:\n```\npytest tests/test_foo.py\n```\n"
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0] == (["pytest", "tests/test_foo.py"], "pytest tests/test_foo.py")

    def test_extracts_pytest_from_inline_code(self) -> None:
        """Should extract pytest command from inline code."""
        builder = BuilderPhase()
        text = "Run `pytest tests/test_bar.py -v` to reproduce."
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0] == (
            ["pytest", "tests/test_bar.py", "-v"],
            "pytest tests/test_bar.py -v",
        )

    def test_extracts_cargo_test(self) -> None:
        """Should extract cargo test command."""
        builder = BuilderPhase()
        text = "```bash\ncargo test test_name\n```"
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0] == (["cargo", "test", "test_name"], "cargo test test_name")

    def test_extracts_pnpm_check_ci(self) -> None:
        """Should extract pnpm check:ci command."""
        builder = BuilderPhase()
        text = "Verify with `pnpm check:ci`."
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0] == (["pnpm", "check:ci"], "pnpm check:ci")

    def test_extracts_python_m_pytest(self) -> None:
        """Should extract 'python -m pytest' form."""
        builder = BuilderPhase()
        text = "```\npython -m pytest tests/ -k test_something\n```"
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0][0] == [
            "python", "-m", "pytest", "tests/", "-k", "test_something"
        ]

    def test_deduplicates_commands(self) -> None:
        """Same command in code block and inline should appear once."""
        builder = BuilderPhase()
        text = (
            "Run `pytest tests/test_foo.py` or:\n"
            "```\npytest tests/test_foo.py\n```\n"
        )
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1

    def test_strips_dollar_prompt(self) -> None:
        """Should strip leading '$ ' from commands."""
        builder = BuilderPhase()
        text = "```\n$ pytest tests/test_foo.py\n```"
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0][0] == ["pytest", "tests/test_foo.py"]

    def test_ignores_non_test_commands(self) -> None:
        """Should not extract non-test commands."""
        builder = BuilderPhase()
        text = "```\nls -la\ncd /tmp\necho hello\n```"
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 0

    def test_ignores_comments_in_code_blocks(self) -> None:
        """Should skip lines starting with '#'."""
        builder = BuilderPhase()
        text = "```\n# run the tests\npytest tests/\n```"
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0][0] == ["pytest", "tests/"]

    def test_no_commands_returns_empty(self) -> None:
        """Issue with no test commands should return empty list."""
        builder = BuilderPhase()
        text = "This is a feature request with no test commands."
        cmds = builder._extract_test_commands(text)
        assert cmds == []

    def test_multiple_commands_from_one_block(self) -> None:
        """Should extract multiple test commands from a single code block."""
        builder = BuilderPhase()
        text = "```\npytest tests/test_a.py\ncargo test foo\n```"
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 2

    def test_pnpm_test_extracted(self) -> None:
        """Should extract pnpm test command."""
        builder = BuilderPhase()
        text = "Run `pnpm test` to check."
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0][0] == ["pnpm", "test"]

    def test_npm_test_extracted(self) -> None:
        """Should extract npm test command."""
        builder = BuilderPhase()
        text = "Verify with `npm test`."
        cmds = builder._extract_test_commands(text)
        assert len(cmds) == 1
        assert cmds[0][0] == ["npm", "test"]


class TestBuilderParseTestCommand:
    """Test _parse_test_command line parsing."""

    def test_bare_pytest(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("pytest") == ["pytest"]

    def test_pytest_with_args(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("pytest tests/ -v") == [
            "pytest",
            "tests/",
            "-v",
        ]

    def test_cargo_test_bare(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("cargo test") == ["cargo", "test"]

    def test_non_test_command_returns_none(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("echo hello") is None
        assert builder._parse_test_command("ls -la") is None
        assert builder._parse_test_command("git status") is None

    def test_empty_line_returns_none(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("") is None
        assert builder._parse_test_command("   ") is None

    def test_comment_line_returns_none(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("# pytest tests/") is None

    def test_dollar_prompt_stripped(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("$ pytest tests/") == [
            "pytest",
            "tests/",
        ]

    def test_pnpm_check_ci_lite(self) -> None:
        builder = BuilderPhase()
        assert builder._parse_test_command("pnpm check:ci:lite") == [
            "pnpm",
            "check:ci:lite",
        ]

    def test_partial_prefix_not_matched(self) -> None:
        """'pytesting' should not match 'pytest' prefix."""
        builder = BuilderPhase()
        assert builder._parse_test_command("pytesting something") is None


class TestBuilderFetchIssueComments:
    """Test _fetch_issue_comments method."""

    def test_success(self, mock_context: MagicMock) -> None:
        """Should return list of comment bodies."""
        builder = BuilderPhase()
        response_json = json.dumps(
            {
                "comments": [
                    {"body": "Try `pytest tests/test_a.py`"},
                    {"body": "I confirmed this fails."},
                ]
            }
        )
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=response_json, stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=completed,
        ):
            comments = builder._fetch_issue_comments(mock_context)
        assert len(comments) == 2
        assert "pytest" in comments[0]

    def test_failure_returns_empty(self, mock_context: MagicMock) -> None:
        """Should return empty list on gh failure."""
        builder = BuilderPhase()
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="error"
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=completed,
        ):
            comments = builder._fetch_issue_comments(mock_context)
        assert comments == []

    def test_os_error_returns_empty(self, mock_context: MagicMock) -> None:
        """Should return empty list on OSError."""
        builder = BuilderPhase()
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=OSError("gh not found"),
        ):
            comments = builder._fetch_issue_comments(mock_context)
        assert comments == []

    def test_empty_comments(self, mock_context: MagicMock) -> None:
        """Should return empty list when no comments exist."""
        builder = BuilderPhase()
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"comments": []}', stderr=""
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=completed,
        ):
            comments = builder._fetch_issue_comments(mock_context)
        assert comments == []


class TestBuilderReproducibilityCheck:
    """Test _run_reproducibility_check pre-implementation verification."""

    def test_no_commands_skips_check(self, mock_context: MagicMock) -> None:
        """Should return None (proceed) when no test commands found."""
        builder = BuilderPhase()
        body_response = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="This is a feature request with no test commands.",
            stderr="",
        )
        comments_response = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"comments": []}', stderr=""
        )

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return body_response
            return comments_response

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=mock_run,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is None  # Proceed with builder

    def test_test_passes_on_main_returns_skipped(
        self, mock_context: MagicMock
    ) -> None:
        """Should return SKIPPED when test passes reliably on main."""
        builder = BuilderPhase()
        body_response = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="Run `pytest tests/test_foo.py` to reproduce.",
            stderr="",
        )
        comments_response = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"comments": []}', stderr=""
        )
        test_pass = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="1 passed", stderr=""
        )

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return body_response
            if call_count == 2:
                return comments_response
            return test_pass

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=mock_run,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.SKIPPED
        assert result.data["no_changes_needed"] is True
        assert result.data["pre_implementation_check"] is True
        # 2 gh calls + 3 test runs
        assert call_count == 5

    def test_test_fails_on_main_returns_none(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None (proceed) when test still fails on main."""
        builder = BuilderPhase()
        body_response = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="Run `pytest tests/test_foo.py` to reproduce.",
            stderr="",
        )
        comments_response = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"comments": []}', stderr=""
        )
        test_fail = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="1 failed", stderr=""
        )

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return body_response
            if call_count == 2:
                return comments_response
            return test_fail

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=mock_run,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is None
        # 2 gh calls + 1 test run (fails immediately)
        assert call_count == 3

    def test_body_fetch_fails_skips_check(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None when issue body cannot be fetched."""
        builder = BuilderPhase()
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="error"
        )
        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            return_value=completed,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is None

    def test_timeout_returns_none(self, mock_context: MagicMock) -> None:
        """Should return None (proceed) when test times out."""
        builder = BuilderPhase()
        body_response = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="Run `pytest tests/test_slow.py` to reproduce.",
            stderr="",
        )
        comments_response = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"comments": []}', stderr=""
        )

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return body_response
            if call_count == 2:
                return comments_response
            raise subprocess.TimeoutExpired(cmd=cmd, timeout=120)

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=mock_run,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is None

    def test_os_error_returns_none(self, mock_context: MagicMock) -> None:
        """Should return None when test command cannot be executed."""
        builder = BuilderPhase()
        body_response = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="Run `pytest tests/test_foo.py` to reproduce.",
            stderr="",
        )
        comments_response = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"comments": []}', stderr=""
        )

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return body_response
            if call_count == 2:
                return comments_response
            raise OSError("pytest not found")

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=mock_run,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is None

    def test_flaky_test_second_run_fails(
        self, mock_context: MagicMock
    ) -> None:
        """Should return None if test passes first but fails on second run."""
        builder = BuilderPhase()
        body_response = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="Run `pytest tests/test_flaky.py` to reproduce.",
            stderr="",
        )
        comments_response = subprocess.CompletedProcess(
            args=[], returncode=0, stdout='{"comments": []}', stderr=""
        )
        test_pass = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="1 passed", stderr=""
        )
        test_fail = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="1 failed", stderr=""
        )

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return body_response
            if call_count == 2:
                return comments_response
            if call_count == 3:
                return test_pass  # First run passes
            return test_fail  # Second run fails

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=mock_run,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is None  # Bug still exists (flaky)

    def test_commands_from_comments(self, mock_context: MagicMock) -> None:
        """Should extract and run test commands from issue comments."""
        builder = BuilderPhase()
        body_response = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="This test is flaky.",
            stderr="",
        )
        comments_json = json.dumps(
            {
                "comments": [
                    {"body": "Try running `cargo test test_foo` to reproduce."},
                ]
            }
        )
        comments_response = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=comments_json, stderr=""
        )
        test_pass = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="ok", stderr=""
        )

        call_count = 0

        def mock_run(cmd, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return body_response
            if call_count == 2:
                return comments_response
            return test_pass

        with patch(
            "loom_tools.shepherd.phases.builder.subprocess.run",
            side_effect=mock_run,
        ):
            result = builder._run_reproducibility_check(mock_context)

        assert result is not None
        assert result.status == PhaseStatus.SKIPPED
        assert result.data["no_changes_needed"] is True
