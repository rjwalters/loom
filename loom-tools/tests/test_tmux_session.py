"""Tests for common/tmux_session.py - shared tmux session management."""

from __future__ import annotations

import time
from unittest import mock

from loom_tools.common.tmux_session import (
    PROCESSING_INDICATORS,
    SESSION_PREFIX,
    TMUX_SOCKET,
    TmuxSession,
)


class TestConstants:
    def test_tmux_socket(self) -> None:
        assert TMUX_SOCKET == "loom"

    def test_session_prefix(self) -> None:
        assert SESSION_PREFIX == "loom-"

    def test_processing_indicators(self) -> None:
        assert PROCESSING_INDICATORS == "esc to interrupt"


class TestTmuxSessionInit:
    def test_default_server(self) -> None:
        session = TmuxSession("test-session")
        assert session.name == "test-session"
        assert session.server_name == "loom"

    def test_custom_server(self) -> None:
        session = TmuxSession("test-session", server_name="custom")
        assert session.server_name == "custom"


class TestTmuxSessionExists:
    def test_exists_true(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch("subprocess.run", return_value=mock_result) as mock_run:
            assert session.exists() is True
        mock_run.assert_called_once_with(
            ["tmux", "-L", "loom", "has-session", "-t", "test-session"],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_exists_false(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 1
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.exists() is False

    def test_exists_exception(self) -> None:
        session = TmuxSession("test-session")
        with mock.patch("subprocess.run", side_effect=Exception("tmux not found")):
            assert session.exists() is False


class TestTmuxSessionCapturePone:
    def test_capture_success(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "pane content here"
        with mock.patch("subprocess.run", return_value=mock_result) as mock_run:
            result = session.capture_pane()
        assert result == "pane content here"
        mock_run.assert_called_once_with(
            ["tmux", "-L", "loom", "capture-pane", "-t", "test-session", "-p"],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_capture_failure(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 1
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.capture_pane() == ""

    def test_capture_exception(self) -> None:
        session = TmuxSession("test-session")
        with mock.patch("subprocess.run", side_effect=Exception("error")):
            assert session.capture_pane() == ""


class TestTmuxSessionSendKeys:
    def test_send_keys_success(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch("subprocess.run", return_value=mock_result) as mock_run:
            assert session.send_keys("hello") is True
        mock_run.assert_called_once_with(
            ["tmux", "-L", "loom", "send-keys", "-t", "test-session", "hello"],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_send_keys_with_extra_args(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch("subprocess.run", return_value=mock_result) as mock_run:
            assert session.send_keys("/exit", "C-m") is True
        mock_run.assert_called_once_with(
            ["tmux", "-L", "loom", "send-keys", "-t", "test-session", "/exit", "C-m"],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_send_keys_failure(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 1
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.send_keys("hello") is False

    def test_send_keys_exception(self) -> None:
        session = TmuxSession("test-session")
        with mock.patch("subprocess.run", side_effect=Exception("error")):
            assert session.send_keys("hello") is False


class TestTmuxSessionKill:
    def test_kill_success(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch("subprocess.run", return_value=mock_result) as mock_run:
            assert session.kill() is True
        mock_run.assert_called_once_with(
            ["tmux", "-L", "loom", "kill-session", "-t", "test-session"],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_kill_exception(self) -> None:
        session = TmuxSession("test-session")
        with mock.patch("subprocess.run", side_effect=Exception("error")):
            assert session.kill() is False


class TestTmuxSessionGetShellPid:
    def test_valid_pid(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "12345\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.get_shell_pid() == "12345"

    def test_multiple_panes(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "12345\n67890\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.get_shell_pid() == "12345"

    def test_no_pid(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 1
        mock_result.stdout = ""
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.get_shell_pid() is None

    def test_exception(self) -> None:
        session = TmuxSession("test-session")
        with mock.patch("subprocess.run", side_effect=Exception("error")):
            assert session.get_shell_pid() is None


class TestTmuxSessionGetSessionAge:
    def test_valid_age(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = str(int(time.time()) - 30) + "\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            age = session.get_session_age()
        assert 28 <= age <= 32

    def test_session_not_found(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 1
        mock_result.stdout = ""
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.get_session_age() == -1

    def test_zero_timestamp(self) -> None:
        session = TmuxSession("test-session")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        mock_result.stdout = "0\n"
        with mock.patch("subprocess.run", return_value=mock_result):
            assert session.get_session_age() == -1

    def test_exception(self) -> None:
        session = TmuxSession("test-session")
        with mock.patch("subprocess.run", side_effect=Exception("error")):
            assert session.get_session_age() == -1


class TestTmuxSessionCustomServer:
    def test_custom_server_in_commands(self) -> None:
        session = TmuxSession("test-session", server_name="custom-server")
        mock_result = mock.Mock()
        mock_result.returncode = 0
        with mock.patch("subprocess.run", return_value=mock_result) as mock_run:
            session.exists()
        mock_run.assert_called_once_with(
            ["tmux", "-L", "custom-server", "has-session", "-t", "test-session"],
            capture_output=True,
            text=True,
            check=False,
        )
