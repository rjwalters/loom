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
