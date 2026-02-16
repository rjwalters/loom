"""Tests for loom_tools.common.claude_config."""

from __future__ import annotations

import pathlib

import pytest

from loom_tools.common.claude_config import (
    _SHARED_CONFIG_FILES,
    _ensure_onboarding_complete,
    _keychain_service_name,
    _resolve_state_file,
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

    def test_claude_json_not_in_shared_config_files(self) -> None:
        """State file .claude.json is handled separately, not in shared list."""
        assert ".claude.json" not in _SHARED_CONFIG_FILES

    def test_mcp_json_not_in_shared_config_files(self) -> None:
        """MCP configs are project-scoped, not user-global — must not be symlinked."""
        assert "mcp.json" not in _SHARED_CONFIG_FILES
        assert ".mcp.json" not in _SHARED_CONFIG_FILES

    def test_symlinks_state_file_from_home_root(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Verify .claude.json is symlinked from ~/.claude.json (home root).

        Claude Code stores onboarding state (hasCompletedOnboarding) in
        ~/.claude.json, not ~/.claude/.claude.json. When CLAUDE_CONFIG_DIR
        is overridden, Claude looks for $CLAUDE_CONFIG_DIR/.claude.json.
        """
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        # State file lives at ~/.claude.json (home root) — include all required fields
        state_file = fake_home / ".claude.json"
        state_file.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        config_dir = setup_agent_config_dir("test-agent", mock_repo)
        dst = config_dir / ".claude.json"
        assert dst.is_symlink(), ".claude.json should be symlinked"
        assert dst.resolve() == state_file.resolve()

    def test_symlinks_state_file_prefers_config_json(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """When ~/.claude/.config.json exists, it takes precedence."""
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        # Both files exist
        (fake_home / ".claude.json").write_text('{"fallback":true}')
        preferred = fake_home / ".claude" / ".config.json"
        preferred.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        config_dir = setup_agent_config_dir("test-agent", mock_repo)
        dst = config_dir / ".claude.json"
        assert dst.is_symlink(), ".claude.json should be symlinked"
        assert dst.resolve() == preferred.resolve()

    def test_missing_state_file_writes_fallback(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """When neither state file exists, a fallback is written."""
        import json

        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        config_dir = setup_agent_config_dir("test-agent", mock_repo)
        dst = config_dir / ".claude.json"
        assert dst.exists(), "Fallback .claude.json should be written"
        assert not dst.is_symlink(), "Should be a real file, not a symlink"
        data = json.loads(dst.read_text())
        assert data["hasCompletedOnboarding"] is True
        assert data["theme"] == "dark"
        assert data["effortCalloutDismissed"] is True
        assert data["opusProMigrationComplete"] is True


class TestEnsureOnboardingComplete:
    """Tests for _ensure_onboarding_complete."""

    def test_noop_when_file_has_required_fields(self, tmp_path: pathlib.Path) -> None:
        import json

        state = tmp_path / ".claude.json"
        original = {
            "hasCompletedOnboarding": True,
            "theme": "monokai",
            "effortCalloutDismissed": True,
            "opusProMigrationComplete": True,
        }
        state.write_text(json.dumps(original))
        _ensure_onboarding_complete(state)
        data = json.loads(state.read_text())
        assert data["theme"] == "monokai"  # unchanged
        assert data == original

    def test_writes_fallback_when_file_missing(self, tmp_path: pathlib.Path) -> None:
        import json

        state = tmp_path / ".claude.json"
        _ensure_onboarding_complete(state)
        assert state.exists()
        data = json.loads(state.read_text())
        assert data["hasCompletedOnboarding"] is True
        assert data["theme"] == "dark"
        assert data["effortCalloutDismissed"] is True
        assert data["opusProMigrationComplete"] is True

    def test_replaces_dangling_symlink(self, tmp_path: pathlib.Path) -> None:
        import json

        state = tmp_path / ".claude.json"
        state.symlink_to(tmp_path / "nonexistent-target")
        assert state.is_symlink()
        assert not state.exists()  # dangling

        _ensure_onboarding_complete(state)
        assert state.exists()
        assert not state.is_symlink()  # replaced with real file
        data = json.loads(state.read_text())
        assert data["hasCompletedOnboarding"] is True

    def test_merges_missing_theme_preserves_existing(self, tmp_path: pathlib.Path) -> None:
        import json

        state = tmp_path / ".claude.json"
        state.write_text(json.dumps({
            "hasCompletedOnboarding": True,
            "effortCalloutDismissed": True,
            "opusProMigrationComplete": True,
        }))
        _ensure_onboarding_complete(state)
        data = json.loads(state.read_text())
        assert data["theme"] == "dark"
        assert data["hasCompletedOnboarding"] is True
        assert data["effortCalloutDismissed"] is True
        assert data["opusProMigrationComplete"] is True

    def test_merges_missing_onboarding_preserves_existing(self, tmp_path: pathlib.Path) -> None:
        import json

        state = tmp_path / ".claude.json"
        state.write_text(json.dumps({"theme": "dark", "customField": "preserved"}))
        _ensure_onboarding_complete(state)
        data = json.loads(state.read_text())
        assert data["hasCompletedOnboarding"] is True
        assert data["customField"] == "preserved"

    def test_replaces_corrupt_json(self, tmp_path: pathlib.Path) -> None:
        import json

        state = tmp_path / ".claude.json"
        state.write_text("not valid json{{{")
        _ensure_onboarding_complete(state)
        data = json.loads(state.read_text())
        assert data["hasCompletedOnboarding"] is True
        assert data["theme"] == "dark"
        assert data["effortCalloutDismissed"] is True
        assert data["opusProMigrationComplete"] is True

    def test_preserves_effort_callout_when_only_theme_missing(self, tmp_path: pathlib.Path) -> None:
        """Regression test: merging must not drop effortCalloutDismissed."""
        import json

        state = tmp_path / ".claude.json"
        state.write_text(json.dumps({
            "hasCompletedOnboarding": True,
            "effortCalloutDismissed": True,
        }))
        _ensure_onboarding_complete(state)
        data = json.loads(state.read_text())
        assert data["theme"] == "dark"
        assert data["effortCalloutDismissed"] is True
        assert data["opusProMigrationComplete"] is True
        assert data["hasCompletedOnboarding"] is True

    def test_preserves_user_theme_choice(self, tmp_path: pathlib.Path) -> None:
        """User's theme choice is not overwritten by the fallback."""
        import json

        state = tmp_path / ".claude.json"
        state.write_text(json.dumps({
            "hasCompletedOnboarding": True,
            "theme": "monokai",
            "effortCalloutDismissed": True,
            "opusProMigrationComplete": True,
            "someOtherSetting": 42,
        }))
        _ensure_onboarding_complete(state)
        data = json.loads(state.read_text())
        assert data["theme"] == "monokai"
        assert data["someOtherSetting"] == 42

    def test_preserves_valid_symlink(self, tmp_path: pathlib.Path) -> None:
        """When symlink target has all required fields, it's left alone."""
        import json

        target = tmp_path / "real-state.json"
        target.write_text(json.dumps({
            "hasCompletedOnboarding": True,
            "theme": "light",
            "effortCalloutDismissed": True,
            "opusProMigrationComplete": True,
        }))
        state = tmp_path / ".claude.json"
        state.symlink_to(target)

        _ensure_onboarding_complete(state)
        assert state.is_symlink()  # symlink preserved
        data = json.loads(state.read_text())
        assert data["theme"] == "light"


class TestResolveStateFile:
    """Tests for _resolve_state_file."""

    def test_prefers_config_json(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        preferred = fake_home / ".claude" / ".config.json"
        preferred.write_text("{}")
        (fake_home / ".claude.json").write_text("{}")
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))
        assert _resolve_state_file() == preferred

    def test_falls_back_to_home_claude_json(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        fallback = fake_home / ".claude.json"
        fallback.write_text("{}")
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))
        assert _resolve_state_file() == fallback

    def test_returns_fallback_path_when_neither_exists(self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
        fake_home = tmp_path / "home"
        fake_home.mkdir()
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))
        # Returns the fallback path even if it doesn't exist
        assert _resolve_state_file() == fake_home / ".claude.json"


class TestKeychainServiceName:
    """Tests for _keychain_service_name."""

    def test_produces_deterministic_hash(self) -> None:
        name1 = _keychain_service_name(pathlib.Path("/some/config/dir"))
        name2 = _keychain_service_name(pathlib.Path("/some/config/dir"))
        assert name1 == name2

    def test_different_dirs_produce_different_names(self) -> None:
        name1 = _keychain_service_name(pathlib.Path("/dir/agent-1"))
        name2 = _keychain_service_name(pathlib.Path("/dir/agent-2"))
        assert name1 != name2

    def test_format_matches_claude_code(self) -> None:
        """Service name format: 'Claude Code-credentials-<8hex>'."""
        name = _keychain_service_name(pathlib.Path("/any/path"))
        assert name.startswith("Claude Code-credentials-")
        suffix = name.split("-")[-1]
        assert len(suffix) == 8
        int(suffix, 16)  # Should be valid hex


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
