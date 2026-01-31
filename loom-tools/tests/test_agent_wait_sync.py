"""Tests for agent_wait module (synchronous agent completion detection)."""

from __future__ import annotations

import json
import pathlib
import re
from unittest import mock

import pytest

from loom_tools.agent_wait import (
    DEFAULT_MIN_IDLE_ELAPSED,
    DEFAULT_POLL_INTERVAL,
    DEFAULT_TIMEOUT,
    IDLE_PROMPT_CONFIRM_COUNT,
    PROCESSING_INDICATORS,
    SESSION_PREFIX,
    TMUX_SOCKET,
    WaitConfig,
    WaitResult,
    check_exit_command,
    check_idle_prompt,
    claude_is_running,
    get_session_age,
    get_session_shell_pid,
    session_exists,
    wait_for_agent,
)


class TestWaitConfig:
    def test_defaults(self) -> None:
        config = WaitConfig(name="test-agent")
        assert config.name == "test-agent"
        assert config.timeout == DEFAULT_TIMEOUT
        assert config.poll_interval == DEFAULT_POLL_INTERVAL
        assert config.min_idle_elapsed == DEFAULT_MIN_IDLE_ELAPSED
        assert config.json_output is False

    def test_custom_values(self) -> None:
        config = WaitConfig(
            name="builder-42",
            timeout=1800,
            poll_interval=10,
            min_idle_elapsed=20,
            json_output=True,
        )
        assert config.name == "builder-42"
        assert config.timeout == 1800
        assert config.poll_interval == 10
        assert config.min_idle_elapsed == 20
        assert config.json_output is True


class TestWaitResult:
    def test_basic_result(self) -> None:
        result = WaitResult(status="completed", name="test-agent", elapsed=120)
        assert result.status == "completed"
        assert result.name == "test-agent"
        assert result.elapsed == 120

    def test_to_dict_completed(self) -> None:
        result = WaitResult(
            status="completed",
            name="test-agent",
            elapsed=120,
            reason="claude_exited",
        )
        d = result.to_dict()
        assert d == {
            "status": "completed",
            "name": "test-agent",
            "elapsed": 120,
            "reason": "claude_exited",
        }

    def test_to_dict_timeout(self) -> None:
        result = WaitResult(status="timeout", name="test-agent", elapsed=3600)
        d = result.to_dict()
        assert d["status"] == "timeout"
        assert d["timeout"] == 3600  # matches bash output format

    def test_to_dict_not_found(self) -> None:
        result = WaitResult(
            status="not_found",
            name="test-agent",
            error="session loom-test-agent",
        )
        d = result.to_dict()
        assert d["status"] == "not_found"
        assert "error" in d

    def test_to_dict_minimal(self) -> None:
        result = WaitResult(status="completed", name="test")
        d = result.to_dict()
        assert d == {"status": "completed", "name": "test"}
        # Empty strings and zero elapsed should not appear
        assert "reason" not in d
        assert "error" not in d
        assert "elapsed" not in d


class TestSessionExists:
    def test_session_exists_true(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session_exists("loom-test") is True

    def test_session_exists_false(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 1
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session_exists("loom-test") is False

    def test_session_exists_exception(self) -> None:
        with mock.patch(
            "subprocess.run",
            side_effect=Exception("tmux not found"),
        ):
            assert session_exists("loom-test") is False


class TestGetSessionAge:
    def test_valid_age(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 0
        # Simulate a session created 30 seconds ago
        import time

        mock_result.stdout = str(int(time.time()) - 30) + "\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            age = get_session_age("loom-test")
        assert 28 <= age <= 32  # Allow small variance

    def test_session_not_found(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 1
        mock_result.stdout = ""
        with mock.patch("subprocess.run", return_value=mock_result):
            assert get_session_age("loom-test") == -1

    def test_zero_timestamp(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "0\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            assert get_session_age("loom-test") == -1

    def test_exception(self) -> None:
        with mock.patch(
            "subprocess.run",
            side_effect=Exception("error"),
        ):
            assert get_session_age("loom-test") == -1


class TestGetSessionShellPid:
    def test_valid_pid(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "12345\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            assert get_session_shell_pid("loom-test") == "12345"

    def test_multiple_panes(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "12345\n67890\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            assert get_session_shell_pid("loom-test") == "12345"

    def test_no_pid(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 1
        mock_result.stdout = ""
        with mock.patch("subprocess.run", return_value=mock_result):
            assert get_session_shell_pid("loom-test") == ""

    def test_exception(self) -> None:
        with mock.patch(
            "subprocess.run",
            side_effect=Exception("error"),
        ):
            assert get_session_shell_pid("loom-test") == ""


class TestClaudeIsRunning:
    def test_direct_child(self) -> None:
        # First pgrep call succeeds (claude is a direct child)
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch(
            "loom_tools.agent_wait.subprocess.run", return_value=mock_result
        ):
            assert claude_is_running("12345") is True

    def test_grandchild(self) -> None:
        # First pgrep call (direct child claude) fails.
        # Second pgrep (list children) succeeds with child PID.
        # Third pgrep (grandchild claude) succeeds.
        call_count = [0]

        def side_effect(*args, **kwargs):
            cmd = args[0]
            result = mock.Mock()
            call_count[0] += 1
            if call_count[0] == 1:
                # pgrep -P 12345 -f claude -> not found
                result.returncode = 1
            elif call_count[0] == 2:
                # pgrep -P 12345 -> child 67890
                result.returncode = 0
                result.stdout = "67890\n"
            elif call_count[0] == 3:
                # pgrep -P 67890 -f claude -> found
                result.returncode = 0
            return result

        with mock.patch("loom_tools.agent_wait.subprocess.run", side_effect=side_effect):
            assert claude_is_running("12345") is True

    def test_not_running(self) -> None:
        mock_result = mock.Mock()
        mock_result.returncode = 1
        mock_result.stdout = ""
        with mock.patch(
            "loom_tools.agent_wait.subprocess.run", return_value=mock_result
        ):
            assert claude_is_running("12345") is False

    def test_exception(self) -> None:
        with mock.patch(
            "loom_tools.agent_wait.subprocess.run",
            side_effect=Exception("pgrep not found"),
        ):
            assert claude_is_running("12345") is False


class TestCheckExitCommand:
    def test_exit_detected(self, tmp_path: pathlib.Path) -> None:
        repo_root = tmp_path
        loom_dir = repo_root / ".loom" / "logs"
        loom_dir.mkdir(parents=True)
        log_file = loom_dir / "loom-test-agent.log"
        log_file.write_text("some output\n❯ /exit\nmore output\n")

        assert check_exit_command("loom-test-agent", str(repo_root)) is True

    def test_exit_with_prompt(self, tmp_path: pathlib.Path) -> None:
        repo_root = tmp_path
        loom_dir = repo_root / ".loom" / "logs"
        loom_dir.mkdir(parents=True)
        log_file = loom_dir / "loom-test-agent.log"
        log_file.write_text("output\n> /exit\n")

        assert check_exit_command("loom-test-agent", str(repo_root)) is True

    def test_no_exit(self, tmp_path: pathlib.Path) -> None:
        repo_root = tmp_path
        loom_dir = repo_root / ".loom" / "logs"
        loom_dir.mkdir(parents=True)
        log_file = loom_dir / "loom-test-agent.log"
        log_file.write_text("still working on implementation...\n")

        assert check_exit_command("loom-test-agent", str(repo_root)) is False

    def test_no_log_file(self, tmp_path: pathlib.Path) -> None:
        assert check_exit_command("loom-test-agent", str(tmp_path)) is False


class TestCheckIdlePrompt:
    def test_idle_at_prompt(self) -> None:
        pane_content = "Some output\nMore output\n❯\n\n"
        with mock.patch("loom_tools.agent_wait.capture_pane", return_value=pane_content):
            assert check_idle_prompt("loom-test") is True

    def test_idle_with_spaces(self) -> None:
        pane_content = "Output\n  ❯  \n"
        with mock.patch("loom_tools.agent_wait.capture_pane", return_value=pane_content):
            assert check_idle_prompt("loom-test") is True

    def test_still_processing(self) -> None:
        pane_content = f"Working...\n{PROCESSING_INDICATORS}\n❯\n"
        with mock.patch("loom_tools.agent_wait.capture_pane", return_value=pane_content):
            assert check_idle_prompt("loom-test") is False

    def test_no_prompt(self) -> None:
        pane_content = "Still working on things\nNo prompt here"
        with mock.patch("loom_tools.agent_wait.capture_pane", return_value=pane_content):
            assert check_idle_prompt("loom-test") is False

    def test_empty_pane(self) -> None:
        with mock.patch("loom_tools.agent_wait.capture_pane", return_value=""):
            assert check_idle_prompt("loom-test") is False


class TestWaitForAgent:
    @pytest.fixture
    def temp_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "logs").mkdir()
        (tmp_path / ".git").mkdir()
        return tmp_path

    def test_session_not_found(self, temp_repo: pathlib.Path) -> None:
        config = WaitConfig(name="nonexistent")
        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch("loom_tools.agent_wait.session_exists", return_value=False):
                result = wait_for_agent(config)

        assert result.status == "not_found"
        assert result.name == "nonexistent"

    def test_no_shell_pid(self, temp_repo: pathlib.Path) -> None:
        config = WaitConfig(name="test-agent")
        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch("loom_tools.agent_wait.session_exists", return_value=True):
                with mock.patch(
                    "loom_tools.agent_wait.get_session_shell_pid", return_value=""
                ):
                    result = wait_for_agent(config)

        assert result.status == "error"
        assert "shell PID" in result.error

    def test_claude_exited(self, temp_repo: pathlib.Path) -> None:
        config = WaitConfig(name="test-agent", poll_interval=0)
        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch("loom_tools.agent_wait.session_exists", return_value=True):
                with mock.patch(
                    "loom_tools.agent_wait.get_session_shell_pid", return_value="12345"
                ):
                    with mock.patch(
                        "loom_tools.agent_wait.check_exit_command", return_value=False
                    ):
                        with mock.patch(
                            "loom_tools.agent_wait.claude_is_running",
                            return_value=False,
                        ):
                            result = wait_for_agent(config)

        assert result.status == "completed"
        assert result.reason == "claude_exited"

    def test_session_destroyed(self, temp_repo: pathlib.Path) -> None:
        config = WaitConfig(name="test-agent", poll_interval=0)
        # First call: session exists + has shell pid. Loop entry: session gone.
        session_calls = [True, False]  # initial check, then loop check

        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch(
                "loom_tools.agent_wait.session_exists", side_effect=session_calls
            ):
                with mock.patch(
                    "loom_tools.agent_wait.get_session_shell_pid", return_value="12345"
                ):
                    result = wait_for_agent(config)

        assert result.status == "completed"
        assert result.reason == "session_destroyed"

    def test_timeout(self, temp_repo: pathlib.Path) -> None:
        config = WaitConfig(name="test-agent", timeout=0, poll_interval=0)

        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch("loom_tools.agent_wait.session_exists", return_value=True):
                with mock.patch(
                    "loom_tools.agent_wait.get_session_shell_pid", return_value="12345"
                ):
                    with mock.patch(
                        "loom_tools.agent_wait.check_exit_command", return_value=False
                    ):
                        with mock.patch(
                            "loom_tools.agent_wait.claude_is_running", return_value=True
                        ):
                            with mock.patch(
                                "loom_tools.agent_wait.check_idle_prompt",
                                return_value=False,
                            ):
                                # session age guard blocks idle check,
                                # timeout=0 triggers timeout
                                with mock.patch(
                                    "loom_tools.agent_wait.get_session_age",
                                    return_value=0,
                                ):
                                    result = wait_for_agent(config)

        assert result.status == "timeout"

    def test_exit_command_detected(self, temp_repo: pathlib.Path) -> None:
        config = WaitConfig(name="test-agent", poll_interval=0)
        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch("loom_tools.agent_wait.session_exists", return_value=True):
                with mock.patch(
                    "loom_tools.agent_wait.get_session_shell_pid", return_value="12345"
                ):
                    with mock.patch(
                        "loom_tools.agent_wait.check_exit_command", return_value=True
                    ):
                        with mock.patch("loom_tools.agent_wait.TmuxSession"):
                            with mock.patch("loom_tools.agent_wait.time.sleep"):
                                result = wait_for_agent(config)

        assert result.status == "completed"
        assert result.reason == "explicit_exit"

    def test_idle_prompt_nonblocking_with_session_age_guard(
        self, temp_repo: pathlib.Path
    ) -> None:
        """Non-blocking mode (timeout=0) with young session should skip idle check."""
        config = WaitConfig(name="test-agent", timeout=0, min_idle_elapsed=10)

        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch("loom_tools.agent_wait.session_exists", return_value=True):
                with mock.patch(
                    "loom_tools.agent_wait.get_session_shell_pid", return_value="12345"
                ):
                    with mock.patch(
                        "loom_tools.agent_wait.check_exit_command", return_value=False
                    ):
                        with mock.patch(
                            "loom_tools.agent_wait.claude_is_running", return_value=True
                        ):
                            # Session is only 2s old, below min_idle_elapsed=10
                            with mock.patch(
                                "loom_tools.agent_wait.get_session_age", return_value=2
                            ):
                                # idle prompt should NOT be checked
                                with mock.patch(
                                    "loom_tools.agent_wait.check_idle_prompt"
                                ) as mock_idle:
                                    result = wait_for_agent(config)

        assert result.status == "timeout"  # Falls through to timeout (=0)
        mock_idle.assert_not_called()

    def test_idle_prompt_nonblocking_mature_session(
        self, temp_repo: pathlib.Path
    ) -> None:
        """Non-blocking mode (timeout=0) with old enough session detects idle."""
        config = WaitConfig(name="test-agent", timeout=0, min_idle_elapsed=10)

        with mock.patch("loom_tools.agent_wait.find_repo_root", return_value=temp_repo):
            with mock.patch("loom_tools.agent_wait.session_exists", return_value=True):
                with mock.patch(
                    "loom_tools.agent_wait.get_session_shell_pid", return_value="12345"
                ):
                    with mock.patch(
                        "loom_tools.agent_wait.check_exit_command", return_value=False
                    ):
                        with mock.patch(
                            "loom_tools.agent_wait.claude_is_running", return_value=True
                        ):
                            with mock.patch(
                                "loom_tools.agent_wait.get_session_age", return_value=30
                            ):
                                with mock.patch(
                                    "loom_tools.agent_wait.check_idle_prompt",
                                    return_value=True,
                                ):
                                    result = wait_for_agent(config)

        assert result.status == "completed"
        assert result.reason == "idle_prompt"


class TestConstants:
    def test_tmux_socket(self) -> None:
        assert TMUX_SOCKET == "loom"

    def test_session_prefix(self) -> None:
        assert SESSION_PREFIX == "loom-"

    def test_defaults_match_bash(self) -> None:
        """Verify defaults match agent-wait.sh constants."""
        assert DEFAULT_TIMEOUT == 3600
        assert DEFAULT_POLL_INTERVAL == 5
        assert DEFAULT_MIN_IDLE_ELAPSED == 10
        assert IDLE_PROMPT_CONFIRM_COUNT == 2

    def test_processing_indicators_match(self) -> None:
        """Verify processing indicators match agent-wait.sh and agent_monitor.py."""
        assert PROCESSING_INDICATORS == "esc to interrupt"
