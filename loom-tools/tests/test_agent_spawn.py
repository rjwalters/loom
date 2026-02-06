"""Tests for loom_tools.agent_spawn."""

from __future__ import annotations

import json
import pathlib
import subprocess
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.agent_spawn import (
    SESSION_PREFIX,
    TMUX_SOCKET,
    SpawnConfig,
    SpawnResult,
    check_claude_cli,
    check_stop_signals,
    check_tmux,
    run,
    session_exists,
    session_is_alive,
    session_is_stuck,
    validate_role,
    validate_worktree,
)
from loom_tools.common.repo import clear_repo_cache


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    clear_repo_cache()
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "roles").mkdir()
    (tmp_path / ".loom" / "logs").mkdir()
    (tmp_path / ".loom" / "scripts").mkdir(parents=True)
    return tmp_path


class TestSpawnResult:
    """Tests for SpawnResult dataclass."""

    def test_to_dict_spawned(self) -> None:
        result = SpawnResult(
            status="spawned",
            name="test-agent",
            session="loom-test-agent",
            on_demand=True,
            log="/tmp/test.log",
        )
        d = result.to_dict()
        assert d["status"] == "spawned"
        assert d["name"] == "test-agent"
        assert d["session"] == "loom-test-agent"
        assert d["on_demand"] is True
        assert d["log"] == "/tmp/test.log"

    def test_to_dict_error(self) -> None:
        result = SpawnResult(
            status="error",
            name="test-agent",
            error="spawn_failed",
        )
        d = result.to_dict()
        assert d["status"] == "error"
        assert d["error"] == "spawn_failed"
        assert "on_demand" not in d

    def test_to_dict_no_empty_fields(self) -> None:
        result = SpawnResult(status="spawned", name="test")
        d = result.to_dict()
        assert "error" not in d
        assert "log" not in d


class TestSessionNaming:
    """Tests for session naming conventions."""

    def test_session_prefix(self) -> None:
        assert SESSION_PREFIX == "loom-"

    def test_tmux_socket(self) -> None:
        assert TMUX_SOCKET == "loom"

    def test_session_name_format(self) -> None:
        name = "shepherd-1"
        session_name = f"{SESSION_PREFIX}{name}"
        assert session_name == "loom-shepherd-1"


class TestCheckTmux:
    """Tests for tmux validation."""

    @patch("shutil.which", return_value=None)
    def test_tmux_not_installed(self, _mock_which: MagicMock) -> None:
        assert check_tmux() is False

    @patch("shutil.which", return_value="/usr/bin/tmux")
    @patch("subprocess.run")
    def test_tmux_installed(
        self, mock_run: MagicMock, _mock_which: MagicMock
    ) -> None:
        mock_run.return_value = subprocess.CompletedProcess(
            args=["tmux", "-V"], returncode=0, stdout="tmux 3.4\n"
        )
        assert check_tmux() is True


class TestCheckClaudeCli:
    """Tests for Claude CLI validation."""

    @patch("shutil.which", return_value=None)
    def test_claude_not_installed(self, _mock_which: MagicMock) -> None:
        assert check_claude_cli() is False

    @patch("shutil.which", return_value="/usr/local/bin/claude")
    def test_claude_installed(self, _mock_which: MagicMock) -> None:
        assert check_claude_cli() is True


class TestValidateRole:
    """Tests for role validation."""

    def test_role_in_loom_roles(self, mock_repo: pathlib.Path) -> None:
        role_file = mock_repo / ".loom" / "roles" / "builder.md"
        role_file.write_text("# Builder role")
        assert validate_role("builder", mock_repo) is True

    def test_role_in_claude_commands(self, mock_repo: pathlib.Path) -> None:
        commands_dir = mock_repo / ".claude" / "commands"
        commands_dir.mkdir(parents=True)
        (commands_dir / "custom.md").write_text("# Custom role")
        assert validate_role("custom", mock_repo) is True

    def test_role_not_found(self, mock_repo: pathlib.Path) -> None:
        assert validate_role("nonexistent", mock_repo) is False

    def test_role_symlink(self, mock_repo: pathlib.Path) -> None:
        # Create a target file and symlink
        target = mock_repo / ".loom" / "roles" / "target.md"
        target.write_text("# Target")
        link = mock_repo / ".loom" / "roles" / "alias.md"
        link.symlink_to(target)
        assert validate_role("alias", mock_repo) is True


class TestValidateWorktree:
    """Tests for worktree validation."""

    def test_nonexistent_path(self, tmp_path: pathlib.Path) -> None:
        assert validate_worktree(tmp_path / "nonexistent") is False

    @patch("subprocess.run")
    def test_valid_worktree(
        self, mock_run: MagicMock, tmp_path: pathlib.Path
    ) -> None:
        mock_run.return_value = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=".git\n"
        )
        assert validate_worktree(tmp_path) is True

    @patch("subprocess.run")
    def test_not_git_repo(
        self, mock_run: MagicMock, tmp_path: pathlib.Path
    ) -> None:
        mock_run.return_value = subprocess.CompletedProcess(
            args=[], returncode=128, stdout="", stderr="not a git repository"
        )
        assert validate_worktree(tmp_path) is False


class TestCheckStopSignals:
    """Tests for stop signal detection."""

    def test_no_signals(self, mock_repo: pathlib.Path) -> None:
        assert check_stop_signals("builder-1", mock_repo) is False

    def test_global_stop_signal(self, mock_repo: pathlib.Path) -> None:
        (mock_repo / ".loom" / "stop-daemon").write_text("stop")
        assert check_stop_signals("builder-1", mock_repo) is True

    def test_shepherd_stop_signal(self, mock_repo: pathlib.Path) -> None:
        (mock_repo / ".loom" / "stop-shepherds").write_text("stop")
        # Should block shepherd agents
        assert check_stop_signals("shepherd-1", mock_repo) is True
        # Should not block non-shepherd agents
        assert check_stop_signals("builder-1", mock_repo) is False

    def test_per_agent_stop_signal(self, mock_repo: pathlib.Path) -> None:
        signals_dir = mock_repo / ".loom" / "signals"
        signals_dir.mkdir()
        (signals_dir / "stop-builder-1").write_text("stop")
        assert check_stop_signals("builder-1", mock_repo) is True
        assert check_stop_signals("builder-2", mock_repo) is False


class TestSessionExists:
    """Tests for session existence checking."""

    @patch("loom_tools.agent_spawn._tmux")
    def test_session_exists(self, mock_tmux: MagicMock) -> None:
        mock_tmux.return_value = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=""
        )
        assert session_exists("test") is True

    @patch("loom_tools.agent_spawn._tmux")
    def test_session_not_exists(self, mock_tmux: MagicMock) -> None:
        mock_tmux.return_value = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=""
        )
        assert session_exists("test") is False

    @patch("loom_tools.agent_spawn._tmux")
    def test_tmux_not_available(self, mock_tmux: MagicMock) -> None:
        mock_tmux.side_effect = FileNotFoundError
        assert session_exists("test") is False


class TestSessionIsAlive:
    """Tests for session alive checking."""

    @patch("loom_tools.agent_spawn._tmux")
    def test_session_alive_with_windows(self, mock_tmux: MagicMock) -> None:
        mock_tmux.return_value = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="0: bash [80x24]\n"
        )
        assert session_is_alive("test") is True

    @patch("loom_tools.agent_spawn._tmux")
    def test_session_not_alive(self, mock_tmux: MagicMock) -> None:
        mock_tmux.return_value = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=""
        )
        assert session_is_alive("test") is False


class TestSessionIsStuck:
    """Tests for stuck session detection."""

    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=None)
    def test_no_shell_pid_is_stuck(
        self, _mock_pid: MagicMock, mock_repo: pathlib.Path
    ) -> None:
        assert session_is_stuck("test", mock_repo, 300) is True

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=False)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value="12345")
    def test_no_claude_process_is_stuck(
        self,
        _mock_pid: MagicMock,
        _mock_running: MagicMock,
        mock_repo: pathlib.Path,
    ) -> None:
        assert session_is_stuck("test", mock_repo, 300) is True

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value="12345")
    def test_healthy_session_not_stuck(
        self,
        _mock_pid: MagicMock,
        _mock_running: MagicMock,
        mock_repo: pathlib.Path,
    ) -> None:
        # Create a fresh log file (not idle)
        log_file = mock_repo / ".loom" / "logs" / "loom-test.log"
        log_file.write_text("recent output")
        assert session_is_stuck("test", mock_repo, 300) is False


class TestSpawnConfig:
    """Tests for SpawnConfig defaults."""

    def test_default_config(self) -> None:
        config = SpawnConfig()
        assert config.role == ""
        assert config.name == ""
        assert config.wait_timeout == 3600
        assert config.on_demand is False
        assert config.fresh is False
        assert config.json_output is False

    def test_stuck_threshold_from_env(self) -> None:
        with patch.dict("os.environ", {"LOOM_STUCK_SESSION_THRESHOLD": "600"}):
            config = SpawnConfig()
            assert config.stuck_threshold == 600

    def test_verify_timeout_from_env(self) -> None:
        with patch.dict("os.environ", {"LOOM_SPAWN_VERIFY_TIMEOUT": "20"}):
            config = SpawnConfig()
            assert config.verify_timeout == 20


class TestRunListMode:
    """Tests for --list mode."""

    @patch("loom_tools.agent_spawn.list_sessions")
    def test_list_returns_zero(self, mock_list: MagicMock) -> None:
        config = SpawnConfig(do_list=True)
        assert run(config) == 0
        mock_list.assert_called_once()


class TestRunCheckMode:
    """Tests for --check mode."""

    @patch("loom_tools.agent_spawn.session_exists", return_value=True)
    def test_check_exists(self, _mock_exists: MagicMock) -> None:
        config = SpawnConfig(check_name="shepherd-1")
        assert run(config) == 0

    @patch("loom_tools.agent_spawn.session_exists", return_value=False)
    def test_check_not_exists(self, _mock_exists: MagicMock) -> None:
        config = SpawnConfig(check_name="shepherd-1")
        assert run(config) == 1


class TestRunValidation:
    """Tests for validation in run()."""

    def test_missing_role(self) -> None:
        config = SpawnConfig(name="test")
        assert run(config) == 1

    def test_missing_name(self) -> None:
        config = SpawnConfig(role="builder")
        assert run(config) == 1

    @patch("loom_tools.agent_spawn.find_repo_root", side_effect=FileNotFoundError)
    def test_not_in_repo(self, _mock_root: MagicMock) -> None:
        config = SpawnConfig(role="builder", name="test")
        assert run(config) == 1

    @patch("loom_tools.agent_spawn.check_tmux", return_value=False)
    @patch("loom_tools.agent_spawn.find_repo_root")
    def test_tmux_not_available(
        self, _mock_root: MagicMock, _mock_tmux: MagicMock
    ) -> None:
        config = SpawnConfig(role="builder", name="test")
        assert run(config) == 1

    @patch("loom_tools.agent_spawn.check_claude_cli", return_value=False)
    @patch("loom_tools.agent_spawn.check_tmux", return_value=True)
    @patch("loom_tools.agent_spawn.find_repo_root")
    def test_claude_not_available(
        self,
        _mock_root: MagicMock,
        _mock_tmux: MagicMock,
        _mock_claude: MagicMock,
    ) -> None:
        config = SpawnConfig(role="builder", name="test")
        assert run(config) == 1


class TestAnsiStripping:
    """Tests for ANSI escape sequence stripping in log output."""

    def test_ansi_strip_regex_removes_color_codes(self) -> None:
        """Test that the sed regex removes standard ANSI color codes."""
        import re

        # The regex pattern used in pipe-pane (Python escaped version)
        pattern = r"\x1b\[[?0-9;]*[a-zA-Z]"

        test_cases = [
            # Standard color codes
            ("\x1b[0m", ""),  # Reset
            ("\x1b[31m", ""),  # Red foreground
            ("\x1b[1;32m", ""),  # Bold green
            ("\x1b[38;2;226;141;109m", ""),  # 24-bit color (RGB)
            # Cursor movement
            ("\x1b[2C", ""),  # Move cursor right
            ("\x1b[6A", ""),  # Move cursor up
            # Terminal mode queries (like ?2026h/l)
            ("\x1b[?2026h", ""),  # Private mode set
            ("\x1b[?2026l", ""),  # Private mode reset
            # Text with escape sequences
            ("\x1b[31mError\x1b[0m", "Error"),  # Red "Error"
            ("Normal \x1b[1mBold\x1b[0m text", "Normal Bold text"),
            ("\x1b[?2026l\x1b[?2026h\n\x1b[2C\x1b[6A\x1b[38;2;226;141;109mComposing…\x1b[39m",
             "\nComposing…"),
        ]

        for input_str, expected in test_cases:
            result = re.sub(pattern, "", input_str)
            assert result == expected, f"Failed for {repr(input_str)}: got {repr(result)}"

    def test_ansi_strip_regex_removes_osc_sequences(self) -> None:
        """Test that the sed regex removes OSC (Operating System Command) sequences."""
        import re

        # OSC sequence pattern (title setting, etc.)
        pattern = r"\x1b\][^\x07]*\x07"

        test_cases = [
            # Title setting sequence: ESC ] 0 ; title BEL
            ("\x1b]0;Terminal Title\x07", ""),
            # Icon name: ESC ] 1 ; name BEL
            ("\x1b]1;icon\x07", ""),
            # Window title: ESC ] 2 ; title BEL
            ("\x1b]2;Window\x07", ""),
            # With surrounding text
            ("Before\x1b]0;title\x07After", "BeforeAfter"),
        ]

        for input_str, expected in test_cases:
            result = re.sub(pattern, "", input_str)
            assert result == expected, f"Failed for {repr(input_str)}: got {repr(result)}"

    def test_combined_ansi_stripping(self) -> None:
        """Test both ANSI and OSC patterns together as used in spawn_agent."""
        import re

        # Combined pattern matching what's in agent_spawn.py
        ansi_pattern = r"\x1b\[[?0-9;]*[a-zA-Z]"
        osc_pattern = r"\x1b\][^\x07]*\x07"

        def strip_ansi(text: str) -> str:
            text = re.sub(ansi_pattern, "", text)
            text = re.sub(osc_pattern, "", text)
            return text

        # Real-world example from the issue diagnostic
        sample_output = (
            "\x1b[?2026l\x1b[?2026h\n"
            "\x1b[2C\x1b[6A\x1b[38;2;226;141;109mComposing…\x1b[39m\n"
            "\x1b[?2026l\n"
        )
        # After stripping: the first line has escape codes then newline,
        # second line has escape codes then "Composing…" then newline,
        # third line has escape code then newline
        expected = "\nComposing…\n\n"
        assert strip_ansi(sample_output) == expected

        # Another example with colors and OSC
        colored_with_title = (
            "\x1b]0;claude\x07"  # Set window title
            "\x1b[1;32m✓\x1b[0m Task complete\n"  # Green checkmark
            "\x1b[31mError:\x1b[0m Something went wrong\n"  # Red error
        )
        expected = "✓ Task complete\nError: Something went wrong\n"
        assert strip_ansi(colored_with_title) == expected

    @patch("loom_tools.agent_spawn._tmux")
    def test_spawn_agent_uses_python_log_filter(self, mock_tmux: MagicMock) -> None:
        """Test that spawn_agent sets up pipe-pane with Python log filter."""
        from loom_tools.agent_spawn import spawn_agent

        mock_tmux.return_value = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=""
        )

        # Create a mock repo with required structure
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            (tmp_path / ".git").mkdir()
            (tmp_path / ".loom" / "logs").mkdir(parents=True)
            (tmp_path / ".loom" / "roles").mkdir(parents=True)
            (tmp_path / ".loom" / "roles" / "builder.md").write_text("# Builder")

            # Call spawn_agent (it will fail at verification, but we just need
            # to check the pipe-pane call)
            spawn_agent(
                role="builder",
                name="test-agent",
                args="",
                worktree="",
                repo_root=tmp_path,
                verify_timeout=0,  # Skip verification
            )

            # Find the pipe-pane call
            pipe_pane_calls = [
                call for call in mock_tmux.call_args_list
                if call[0] and call[0][0] == "pipe-pane"
            ]

            assert len(pipe_pane_calls) >= 1, "pipe-pane should be called"

            # Check the pipe-pane command uses Python log filter with sed fallback
            pipe_pane_call = pipe_pane_calls[0]
            pipe_cmd = pipe_pane_call[0][3] if len(pipe_pane_call[0]) > 3 else ""

            assert "python3 -m loom_tools.log_filter" in pipe_cmd, \
                f"pipe-pane should use Python log filter, got: {pipe_cmd}"
            assert "sed" in pipe_cmd, \
                f"pipe-pane should include sed fallback, got: {pipe_cmd}"
            assert ">>" in pipe_cmd, \
                f"command should append to log file: {pipe_cmd}"
