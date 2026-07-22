"""Tests for loom_tools.common.claude_config."""

from __future__ import annotations

import pathlib
import shutil
from unittest import mock

import pytest

from loom_tools.common.claude_config import (
    _PROJECT_CLAUDE_LINKS,
    _SHARED_CONFIG_FILES,
    _copy_settings_without_plugins,
    _ensure_onboarding_complete,
    _keychain_service_name,
    _link_project_claude_dirs,
    _resolve_state_file,
    cleanup_agent_config_dir,
    cleanup_all_agent_config_dirs,
    resolve_claude_base_dir,
    resolve_projects_dir,
    setup_agent_config_dir,
    validate_agent_config_dir,
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

        # config.json should be symlinked
        src = home_claude / "config.json"
        dst = config_dir / "config.json"
        if src.exists():
            assert dst.is_symlink(), "config.json should be a symlink"
            assert dst.resolve() == src.resolve()

        # settings.json should be a COPY (not symlink) with enabledPlugins stripped
        settings_src = home_claude / "settings.json"
        settings_dst = config_dir / "settings.json"
        if settings_src.exists():
            import json

            assert settings_dst.exists(), "settings.json should exist"
            assert not settings_dst.is_symlink(), "settings.json should NOT be a symlink"
            data = json.loads(settings_dst.read_text())
            assert "enabledPlugins" not in data, "enabledPlugins should be stripped"

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

    def test_settings_json_not_in_shared_config_files(self) -> None:
        """settings.json is copied (not symlinked) to strip enabledPlugins."""
        assert "settings.json" not in _SHARED_CONFIG_FILES

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

    def test_settings_json_fallback_when_source_missing(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """When ~/.claude/settings.json doesn't exist, a minimal fallback
        settings.json is written to prevent Claude Code from falling back
        to the global settings file with enabledPlugins (#3065)."""
        import json

        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        # No settings.json in fake home
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        config_dir = setup_agent_config_dir("test-agent", mock_repo)
        settings_dst = config_dir / "settings.json"
        assert settings_dst.exists(), "Fallback settings.json should be created"
        assert not settings_dst.is_symlink(), "settings.json should NOT be a symlink"
        data = json.loads(settings_dst.read_text())
        assert "enabledPlugins" not in data
        assert data == {}

    def test_settings_json_fallback_when_copy_fails(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """When _copy_settings_without_plugins fails (corrupt source), a minimal
        fallback settings.json is written (#3065)."""
        import json

        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        # Write corrupt settings.json
        (fake_home / ".claude" / "settings.json").write_text("not valid json{{{")
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        config_dir = setup_agent_config_dir("test-agent", mock_repo)
        settings_dst = config_dir / "settings.json"
        assert settings_dst.exists(), "Fallback settings.json should be created"
        data = json.loads(settings_dst.read_text())
        assert "enabledPlugins" not in data
        assert data == {}

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


class TestLinkProjectClaudeDirs:
    """Tests for the project-claude link logic (issue #3346).

    The per-agent ``CLAUDE_CONFIG_DIR`` must expose the project's
    ``.claude/commands`` and ``.claude/agents`` so Claude Code 2.1+ can
    resolve namespaced slash commands like ``/loom:shepherd``.
    """

    def test_constant_has_commands_and_agents(self) -> None:
        """The link list must include the directories Claude Code consults."""
        assert "commands" in _PROJECT_CLAUDE_LINKS
        assert "agents" in _PROJECT_CLAUDE_LINKS

    def test_creates_symlinks_to_project_claude_dirs(
        self, mock_repo: pathlib.Path
    ) -> None:
        """When ``<repo>/.claude/commands`` exists it must be linked into the
        agent config dir as a symlink pointing at the project path."""
        project_commands = mock_repo / ".claude" / "commands" / "loom"
        project_commands.mkdir(parents=True)
        (project_commands / "shepherd.md").write_text("# shepherd role\n")

        config_dir = setup_agent_config_dir("shepherd-1", mock_repo)
        dst = config_dir / "commands"
        assert dst.is_symlink(), "commands/ should be a symlink"
        assert dst.resolve() == (mock_repo / ".claude" / "commands").resolve()
        # The role file is reachable via the link.
        assert (dst / "loom" / "shepherd.md").is_file()

    def test_skips_when_project_dir_missing(self, mock_repo: pathlib.Path) -> None:
        """Repos without ``.claude/commands`` get no link and no error."""
        config_dir = setup_agent_config_dir("shepherd-1", mock_repo)
        assert not (config_dir / "commands").exists()
        assert not (config_dir / "agents").exists()

    def test_links_agents_dir_when_present(self, mock_repo: pathlib.Path) -> None:
        agents = mock_repo / ".claude" / "agents"
        agents.mkdir(parents=True)
        (agents / "loom-shepherd.md").write_text("# agent\n")

        config_dir = setup_agent_config_dir("shepherd-1", mock_repo)
        dst = config_dir / "agents"
        assert dst.is_symlink()
        assert dst.resolve() == agents.resolve()

    def test_idempotent_keeps_existing_correct_link(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Re-running setup with the same project paths must be a no-op."""
        commands = mock_repo / ".claude" / "commands"
        commands.mkdir(parents=True)

        setup_agent_config_dir("shepherd-1", mock_repo)
        config_dir = mock_repo / ".loom" / "claude-config" / "shepherd-1"
        first_target = (config_dir / "commands").resolve()

        # Run again; link must still be present and point at the same place.
        setup_agent_config_dir("shepherd-1", mock_repo)
        assert (config_dir / "commands").is_symlink()
        assert (config_dir / "commands").resolve() == first_target

    def test_refreshes_stale_symlink(self, mock_repo: pathlib.Path) -> None:
        """If the link points at a stale path, setup must replace it."""
        commands = mock_repo / ".claude" / "commands"
        commands.mkdir(parents=True)

        config_dir = mock_repo / ".loom" / "claude-config" / "shepherd-1"
        config_dir.mkdir(parents=True)

        # Pre-create a wrong-target symlink.
        wrong = mock_repo / "wrong-target"
        wrong.mkdir()
        (config_dir / "commands").symlink_to(wrong)

        setup_agent_config_dir("shepherd-1", mock_repo)

        assert (config_dir / "commands").is_symlink()
        assert (config_dir / "commands").resolve() == commands.resolve()

    def test_preserves_existing_non_symlink_directory(
        self, mock_repo: pathlib.Path
    ) -> None:
        """If ``commands/`` already exists as a real directory (e.g. operator
        placed content there), we must not destroy it."""
        commands = mock_repo / ".claude" / "commands"
        commands.mkdir(parents=True)

        config_dir = mock_repo / ".loom" / "claude-config" / "shepherd-1"
        config_dir.mkdir(parents=True)
        # Operator-placed plain directory.
        plain = config_dir / "commands"
        plain.mkdir()
        (plain / "user-file.md").write_text("# user content\n")

        setup_agent_config_dir("shepherd-1", mock_repo)

        # Plain dir preserved; not converted to symlink.
        assert plain.is_dir() and not plain.is_symlink()
        assert (plain / "user-file.md").is_file()

    def test_link_helper_called_directly(self, mock_repo: pathlib.Path) -> None:
        """The helper itself must be safe to call without a full setup."""
        commands = mock_repo / ".claude" / "commands"
        commands.mkdir(parents=True)

        config_dir = mock_repo / "agent-conf"
        config_dir.mkdir()

        _link_project_claude_dirs(mock_repo, config_dir)

        assert (config_dir / "commands").is_symlink()
        assert (config_dir / "commands").resolve() == commands.resolve()

    def test_validate_fails_when_project_link_missing(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """If the project has ``.claude/commands`` but the agent's link is
        missing, validation must fail so the dir gets rebuilt."""
        commands = mock_repo / ".claude" / "commands"
        commands.mkdir(parents=True)

        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        (fake_home / ".claude.json").write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("shepherd-1", mock_repo)
        config_dir = mock_repo / ".loom" / "claude-config" / "shepherd-1"
        assert validate_agent_config_dir("shepherd-1", mock_repo) is True

        # Break the link.
        (config_dir / "commands").unlink()
        assert validate_agent_config_dir("shepherd-1", mock_repo) is False

    def test_validate_tolerates_missing_project_dir(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """If the project itself doesn't have ``.claude/commands``, the link
        is not required and validation must still pass."""
        # No project .claude/commands created.
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        (fake_home / ".claude.json").write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("shepherd-1", mock_repo)
        assert validate_agent_config_dir("shepherd-1", mock_repo) is True


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

    def test_symlink_target_updated_not_destroyed_when_theme_null(
        self, tmp_path: pathlib.Path
    ) -> None:
        """When symlink points to a valid target with theme=null, write to
        target directly — do NOT destroy the symlink.  See issue #2835.

        Previously _ensure_onboarding_complete would unlink() the symlink
        and write a standalone file, severing the connection to the global
        ~/.claude.json.  This caused agent sessions to run with minimal
        state (only the 4 required fields) instead of the full user config,
        resulting in degraded sessions (missing project history, settings,
        keychain state, etc.).
        """
        import json

        # Simulate ~/.claude.json that has theme=null (json null maps to None)
        global_state = tmp_path / "home-claude.json"
        global_state.write_text(json.dumps({
            "hasCompletedOnboarding": True,
            "theme": None,  # null — the bug trigger
            "effortCalloutDismissed": True,
            "opusProMigrationComplete": True,
            "projects": {"some/path": {"lastCost": 0.5}},
            "userPref": "preserved",
        }))

        state = tmp_path / ".claude.json"
        state.symlink_to(global_state)

        _ensure_onboarding_complete(state)

        # Symlink must be preserved — not replaced with a standalone file
        assert state.is_symlink(), (
            "symlink was destroyed; _ensure_onboarding_complete should write "
            "to the symlink target, not replace the symlink with a new file"
        )

        # Target must have theme set
        data = json.loads(state.read_text())
        assert data["theme"] == "dark"

        # Full content must be preserved (not just the 4 required fields)
        assert data["userPref"] == "preserved"
        assert "projects" in data


class TestCopySettingsWithoutPlugins:
    """Tests for _copy_settings_without_plugins."""

    def test_strips_enabled_plugins(self, tmp_path: pathlib.Path) -> None:
        import json

        src = tmp_path / "settings.json"
        src.write_text(json.dumps({
            "enabledPlugins": {"rust-analyzer-lsp@official": True},
            "model": "sonnet",
            "alwaysThinkingEnabled": True,
        }))
        dst = tmp_path / "out-settings.json"
        assert _copy_settings_without_plugins(src, dst) is True
        data = json.loads(dst.read_text())
        assert "enabledPlugins" not in data
        assert data["model"] == "sonnet"
        assert data["alwaysThinkingEnabled"] is True

    def test_preserves_all_other_keys(self, tmp_path: pathlib.Path) -> None:
        import json

        src = tmp_path / "settings.json"
        src.write_text(json.dumps({
            "enabledPlugins": {"swift-lsp@official": True},
            "model": "opus",
            "skipDangerousModePermissionPrompt": True,
            "customSetting": 42,
        }))
        dst = tmp_path / "out.json"
        _copy_settings_without_plugins(src, dst)
        data = json.loads(dst.read_text())
        assert data["model"] == "opus"
        assert data["skipDangerousModePermissionPrompt"] is True
        assert data["customSetting"] == 42

    def test_no_plugins_key_still_copies(self, tmp_path: pathlib.Path) -> None:
        import json

        src = tmp_path / "settings.json"
        src.write_text(json.dumps({"model": "haiku"}))
        dst = tmp_path / "out.json"
        assert _copy_settings_without_plugins(src, dst) is True
        data = json.loads(dst.read_text())
        assert data == {"model": "haiku"}

    def test_missing_src_returns_false(self, tmp_path: pathlib.Path) -> None:
        dst = tmp_path / "out.json"
        assert _copy_settings_without_plugins(tmp_path / "nope.json", dst) is False
        assert not dst.exists()

    def test_corrupt_json_returns_false(self, tmp_path: pathlib.Path) -> None:
        src = tmp_path / "settings.json"
        src.write_text("not json{{{")
        dst = tmp_path / "out.json"
        assert _copy_settings_without_plugins(src, dst) is False

    def test_non_object_json_returns_false(self, tmp_path: pathlib.Path) -> None:
        src = tmp_path / "settings.json"
        src.write_text("[1, 2, 3]")
        dst = tmp_path / "out.json"
        assert _copy_settings_without_plugins(src, dst) is False


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

    def test_tolerates_file_vanishing_during_rmtree(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Cleanup must not crash when a file (e.g. .claude.json.lock)
        vanishes between rmtree listing the directory and unlinking the
        entry.  This is the race condition from issue #3097.
        """
        import sys

        setup_agent_config_dir("test-agent", mock_repo)
        config_dir = mock_repo / ".loom" / "claude-config" / "test-agent"

        # Create a fake lock file that Claude Code would leave behind
        lock_file = config_dir / ".claude.json.lock"
        lock_file.write_text("")

        # Patch shutil.rmtree to simulate the race: the first call raises
        # FileNotFoundError for the lock file (as if it vanished mid-removal),
        # then the retry with ignore_errors succeeds.
        original_rmtree = shutil.rmtree

        def flaky_rmtree(path, *, onerror=None, onexc=None, ignore_errors=False, **kwargs):
            if ignore_errors:
                # Second (belt-and-suspenders) call — let it proceed normally
                return original_rmtree(path, ignore_errors=True)
            # First call — simulate a file vanishing mid-walk
            exc = FileNotFoundError(
                2, "No such file or directory", str(lock_file)
            )
            if sys.version_info >= (3, 12) and onexc is not None:
                onexc(None, str(lock_file), exc)
                return original_rmtree(path, ignore_errors=True)
            elif onerror is not None:
                onerror(None, str(lock_file), (FileNotFoundError, exc, None))
                return original_rmtree(path, ignore_errors=True)
            return original_rmtree(path)

        with mock.patch("loom_tools.common.claude_config.shutil.rmtree", side_effect=flaky_rmtree):
            result = cleanup_agent_config_dir("test-agent", mock_repo)

        assert result is True


class TestValidateAgentConfigDir:
    """Tests for validate_agent_config_dir.

    These tests verify that validate_agent_config_dir correctly identifies
    both healthy and corrupted config directories so that agent_spawn.py
    can reinitialize corrupted dirs before each retry attempt.  See issue #2909.
    """

    def test_returns_false_for_nonexistent_dir(self, mock_repo: pathlib.Path) -> None:
        """A directory that has never been created is not valid (but not corrupted)."""
        assert validate_agent_config_dir("nonexistent-agent", mock_repo) is False

    def test_returns_true_for_healthy_dir(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A freshly set-up config dir must pass validation."""
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        state_file = fake_home / ".claude.json"
        state_file.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("healthy-agent", mock_repo)
        assert validate_agent_config_dir("healthy-agent", mock_repo) is True

    def test_returns_false_when_claude_json_is_dangling_symlink(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Dangling .claude.json symlink indicates a corrupted config dir.

        This is the primary corruption pattern from issue #2909: the first
        builder attempt had a valid symlink, but the target was removed or
        the symlink was replaced and the target disappeared.
        """
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        state_file = fake_home / ".claude.json"
        state_file.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("test-agent", mock_repo)

        # Simulate the state-file target disappearing (dangling symlink)
        config_dir = mock_repo / ".loom" / "claude-config" / "test-agent"
        state_dst = config_dir / ".claude.json"
        # Remove and replace with a dangling symlink
        state_dst.unlink()
        state_dst.symlink_to(mock_repo / "nonexistent-target.json")

        assert validate_agent_config_dir("test-agent", mock_repo) is False

    def test_returns_false_when_claude_json_is_missing(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Missing .claude.json (deleted after setup) fails validation."""
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        state_file = fake_home / ".claude.json"
        state_file.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("test-agent", mock_repo)

        config_dir = mock_repo / ".loom" / "claude-config" / "test-agent"
        state_dst = config_dir / ".claude.json"
        state_dst.unlink()

        assert validate_agent_config_dir("test-agent", mock_repo) is False

    def test_returns_false_when_mutable_dir_missing(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Missing mutable directory (e.g., 'tmp') fails validation."""
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        state_file = fake_home / ".claude.json"
        state_file.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("test-agent", mock_repo)

        # Remove a mutable directory
        config_dir = mock_repo / ".loom" / "claude-config" / "test-agent"
        shutil.rmtree(config_dir / "tmp")

        assert validate_agent_config_dir("test-agent", mock_repo) is False

    def test_returns_true_when_claude_json_is_plain_file(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A plain file .claude.json (no symlink) is valid — the fallback case."""
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        # No state file exists — setup writes a fallback plain file
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("test-agent", mock_repo)

        config_dir = mock_repo / ".loom" / "claude-config" / "test-agent"
        state_dst = config_dir / ".claude.json"
        # Verify it's a plain file (not a symlink) in this case
        assert not state_dst.is_symlink()
        assert state_dst.exists()

        assert validate_agent_config_dir("test-agent", mock_repo) is True

    def test_reinitialize_removes_and_recreates_corrupted_dir(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """After validation failure, cleanup+setup produces a healthy dir.

        This tests the full recovery flow used by agent_spawn.py:
        validate → fail → cleanup → setup → validate → pass
        """
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        state_file = fake_home / ".claude.json"
        state_file.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("test-agent", mock_repo)

        # Corrupt: replace .claude.json symlink with a dangling symlink
        config_dir = mock_repo / ".loom" / "claude-config" / "test-agent"
        state_dst = config_dir / ".claude.json"
        state_dst.unlink()
        state_dst.symlink_to(mock_repo / "nonexistent-target.json")

        # Validation must fail
        assert validate_agent_config_dir("test-agent", mock_repo) is False

        # Recovery: cleanup + setup
        cleanup_agent_config_dir("test-agent", mock_repo)
        setup_agent_config_dir("test-agent", mock_repo)

        # Validation must now pass
        assert validate_agent_config_dir("test-agent", mock_repo) is True


    def test_returns_false_when_settings_json_missing(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Missing settings.json means Claude Code could fall back to
        global settings with enabledPlugins, causing ghost sessions (#3065)."""
        fake_home = mock_repo / "fake-home"
        fake_home.mkdir()
        (fake_home / ".claude").mkdir()
        state_file = fake_home / ".claude.json"
        state_file.write_text(
            '{"hasCompletedOnboarding":true,"theme":"dark",'
            '"effortCalloutDismissed":true,"opusProMigrationComplete":true}'
        )
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))

        setup_agent_config_dir("test-agent", mock_repo)

        # Remove settings.json to simulate incomplete setup
        config_dir = mock_repo / ".loom" / "claude-config" / "test-agent"
        settings = config_dir / "settings.json"
        if settings.exists():
            settings.unlink()

        assert validate_agent_config_dir("test-agent", mock_repo) is False


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

    def test_tolerates_file_vanishing_during_rmtree(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Same race condition as issue #3097 but for bulk cleanup."""
        setup_agent_config_dir("agent-1", mock_repo)
        config_dir = mock_repo / ".loom" / "claude-config" / "agent-1"
        (config_dir / ".claude.json.lock").write_text("")

        # The real _rmtree_with_retry handles the error via onerror callback,
        # so we just verify that cleanup_all does not raise when lock files
        # are present and can potentially vanish.
        count = cleanup_all_agent_config_dirs(mock_repo)
        assert count == 1
        assert not config_dir.exists()


class TestResolveProjectsDir:
    """Tests for the CLAUDE_CONFIG_DIR-aware base/projects resolvers (#3726)."""

    def test_base_defaults_to_home_claude(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
        fake_home = tmp_path / "home"
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))
        assert resolve_claude_base_dir() == fake_home / ".claude"
        assert resolve_projects_dir() == fake_home / ".claude" / "projects"

    def test_base_honours_claude_config_dir_override(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        override = tmp_path / ".loom" / "claude-config" / "builder-1"
        monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(override))
        assert resolve_claude_base_dir() == override
        # HARD AC (#3726 CORRECTION 2): projects/ resolves under the override,
        # NOT ~/.claude/projects.
        assert resolve_projects_dir() == override / "projects"

    def test_override_expands_user(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("CLAUDE_CONFIG_DIR", "~/isolated")
        assert resolve_claude_base_dir() == pathlib.Path.home() / "isolated"

    def test_empty_override_falls_back_to_home(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # An empty string is falsy — treat as unset.
        monkeypatch.setenv("CLAUDE_CONFIG_DIR", "")
        fake_home = tmp_path / "home"
        monkeypatch.setattr(pathlib.Path, "home", staticmethod(lambda: fake_home))
        assert resolve_claude_base_dir() == fake_home / ".claude"
