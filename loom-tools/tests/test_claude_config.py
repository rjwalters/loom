"""Tests for loom_tools.common.claude_config."""

from __future__ import annotations

import pathlib

import pytest

from loom_tools.common.claude_config import (
    cleanup_agent_config_dir,
    cleanup_all_agent_config_dirs,
    setup_agent_config_dir,
)


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .loom directory."""
    (tmp_path / ".loom").mkdir()
    return tmp_path


class TestSetupAgentConfigDir:
    """Tests for setup_agent_config_dir."""

    def test_creates_config_dir(self, mock_repo: pathlib.Path) -> None:
        result = setup_agent_config_dir("builder-1", mock_repo)
        assert result == mock_repo / ".loom" / "claude-config" / "builder-1"
        assert result.is_dir()

    def test_creates_mutable_dirs(self, mock_repo: pathlib.Path) -> None:
        config_dir = setup_agent_config_dir("test-agent", mock_repo)
        expected_dirs = [
            "projects",
            "todos",
            "debug",
            "file-history",
            "session-env",
            "tasks",
            "plans",
            "shell-snapshots",
            "tmp",
        ]
        for dirname in expected_dirs:
            assert (config_dir / dirname).is_dir(), f"Missing mutable dir: {dirname}"

    def test_symlinks_shared_config_files(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path
    ) -> None:
        # Create fake ~/.claude/ config files in a temp location
        home_claude = pathlib.Path.home() / ".claude"
        if not home_claude.exists():
            pytest.skip("~/.claude/ does not exist")

        config_dir = setup_agent_config_dir("test-agent", mock_repo)

        # Check that symlinks were created for files that exist in ~/.claude/
        for filename in ["settings.json", "config.json"]:
            src = home_claude / filename
            dst = config_dir / filename
            if src.exists():
                assert dst.is_symlink(), f"{filename} should be a symlink"
                assert dst.resolve() == src.resolve()

    def test_idempotent(self, mock_repo: pathlib.Path) -> None:
        """Calling setup twice should not fail or duplicate anything."""
        config_dir1 = setup_agent_config_dir("test-agent", mock_repo)
        config_dir2 = setup_agent_config_dir("test-agent", mock_repo)
        assert config_dir1 == config_dir2
        assert config_dir1.is_dir()
        # Mutable dirs still exist
        assert (config_dir1 / "tmp").is_dir()

    def test_different_agents_get_different_dirs(
        self, mock_repo: pathlib.Path
    ) -> None:
        dir1 = setup_agent_config_dir("agent-1", mock_repo)
        dir2 = setup_agent_config_dir("agent-2", mock_repo)
        assert dir1 != dir2
        assert dir1.is_dir()
        assert dir2.is_dir()


class TestCleanupAgentConfigDir:
    """Tests for cleanup_agent_config_dir."""

    def test_removes_existing_dir(self, mock_repo: pathlib.Path) -> None:
        setup_agent_config_dir("test-agent", mock_repo)
        assert cleanup_agent_config_dir("test-agent", mock_repo) is True
        assert not (mock_repo / ".loom" / "claude-config" / "test-agent").exists()

    def test_returns_false_for_nonexistent(self, mock_repo: pathlib.Path) -> None:
        assert cleanup_agent_config_dir("nonexistent", mock_repo) is False


class TestCleanupAllAgentConfigDirs:
    """Tests for cleanup_all_agent_config_dirs."""

    def test_removes_all_dirs(self, mock_repo: pathlib.Path) -> None:
        setup_agent_config_dir("agent-1", mock_repo)
        setup_agent_config_dir("agent-2", mock_repo)
        setup_agent_config_dir("agent-3", mock_repo)

        count = cleanup_all_agent_config_dirs(mock_repo)
        assert count == 3
        assert not (mock_repo / ".loom" / "claude-config" / "agent-1").exists()
        assert not (mock_repo / ".loom" / "claude-config" / "agent-2").exists()
        assert not (mock_repo / ".loom" / "claude-config" / "agent-3").exists()

    def test_returns_zero_when_no_dirs(self, mock_repo: pathlib.Path) -> None:
        assert cleanup_all_agent_config_dirs(mock_repo) == 0

    def test_returns_zero_when_base_dir_missing(
        self, mock_repo: pathlib.Path
    ) -> None:
        # Base dir doesn't exist yet
        assert cleanup_all_agent_config_dirs(mock_repo) == 0
