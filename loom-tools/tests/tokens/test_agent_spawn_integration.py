"""Integration tests for OAuth-token rotation in agent_spawn.

Issue #3236: verify that ``CLAUDE_CODE_OAUTH_TOKEN`` is correctly injected
into the spawned ``claude-wrapper.sh`` invocation when a workspace token
pool is present, and that it is omitted (preserving Keychain-auth backward
compatibility) when the pool is absent.

These tests do not require the ``loom_tools.tokens`` package (#3235) to
exist — the helper is exercised in three configurations:

  1. No ``.loom/tokens/`` directory (returns None).
  2. ``.loom/tokens/`` present but the selection module unavailable
     (returns None — defensive against #3235 not being merged yet).
  3. Selection module available (mocked) — returns the chosen token.
"""

from __future__ import annotations

import importlib
import os
import pathlib
import subprocess
import sys
import types
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.agent_spawn import _select_oauth_token, spawn_agent
from loom_tools.common.repo import clear_repo_cache


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git, .loom, and .loom/scripts dirs."""
    clear_repo_cache()
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "roles").mkdir()
    (tmp_path / ".loom" / "logs").mkdir()
    (tmp_path / ".loom" / "scripts").mkdir(parents=True)
    return tmp_path


# ---------------------------------------------------------------------------
# _select_oauth_token unit tests
# ---------------------------------------------------------------------------


class TestSelectOAuthToken:
    """Tests for the _select_oauth_token helper."""

    def test_returns_none_when_tokens_dir_missing(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Backward-compat: no .loom/tokens/ -> Keychain auth path."""
        assert not (mock_repo / ".loom" / "tokens").exists()
        assert _select_oauth_token(mock_repo) is None

    def test_returns_none_when_selection_module_missing(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Backward-compat: tokens dir exists but #3235 not merged."""
        (mock_repo / ".loom" / "tokens").mkdir()
        # Belt-and-suspenders: ensure the module really is absent.
        sys.modules.pop("loom_tools.tokens", None)
        sys.modules.pop("loom_tools.tokens.select", None)
        # The real codebase has no loom_tools.tokens module yet.
        assert _select_oauth_token(mock_repo) is None

    def test_returns_token_when_selection_module_present(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Token is returned when both the dir and the module are present."""
        (mock_repo / ".loom" / "tokens").mkdir()

        # Synthesize a fake loom_tools.tokens.select module.
        fake_pkg = types.ModuleType("loom_tools.tokens")
        fake_pkg.__path__ = []  # mark as package
        fake_select = types.ModuleType("loom_tools.tokens.select")
        fake_select.select_token = MagicMock(return_value="sk-ant-oat01-test-xyz")
        sys.modules["loom_tools.tokens"] = fake_pkg
        sys.modules["loom_tools.tokens.select"] = fake_select
        try:
            assert _select_oauth_token(mock_repo) == "sk-ant-oat01-test-xyz"
            fake_select.select_token.assert_called_once_with(mock_repo)
        finally:
            sys.modules.pop("loom_tools.tokens.select", None)
            sys.modules.pop("loom_tools.tokens", None)

    def test_returns_none_when_selection_raises(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Defensive: selection errors fall through to Keychain auth."""
        (mock_repo / ".loom" / "tokens").mkdir()

        fake_pkg = types.ModuleType("loom_tools.tokens")
        fake_pkg.__path__ = []
        fake_select = types.ModuleType("loom_tools.tokens.select")
        fake_select.select_token = MagicMock(
            side_effect=RuntimeError("all tokens exhausted")
        )
        sys.modules["loom_tools.tokens"] = fake_pkg
        sys.modules["loom_tools.tokens.select"] = fake_select
        try:
            assert _select_oauth_token(mock_repo) is None
        finally:
            sys.modules.pop("loom_tools.tokens.select", None)
            sys.modules.pop("loom_tools.tokens", None)


# ---------------------------------------------------------------------------
# spawn_agent integration tests — verify env-prefix in the tmux send-keys cmd
# ---------------------------------------------------------------------------


def _capture_tmux_calls(mock_repo: pathlib.Path) -> list[list[str]]:
    """Return a list of the tmux argv arrays passed to subprocess.run."""
    captured: list[list[str]] = []

    def fake_run(cmd, **kwargs):  # type: ignore[no-untyped-def]
        captured.append(list(cmd))
        # Return success for every tmux call.  has-session needs to return 0
        # so the spawn loop sees the session as alive; new-session also OK.
        return subprocess.CompletedProcess(args=cmd, returncode=0, stdout="", stderr="")

    return captured, fake_run  # type: ignore[return-value]


class TestSpawnAgentTokenInjection:
    """Verify CLAUDE_CODE_OAUTH_TOKEN flows into the spawned command."""

    def _make_wrapper(self, mock_repo: pathlib.Path) -> pathlib.Path:
        """Create an executable claude-wrapper.sh stub."""
        wrapper = mock_repo / ".loom" / "scripts" / "claude-wrapper.sh"
        wrapper.write_text("#!/bin/bash\nexit 0\n")
        wrapper.chmod(0o755)
        return wrapper

    def _run_spawn(
        self, mock_repo: pathlib.Path, env: dict[str, str] | None = None
    ) -> list[list[str]]:
        """Drive spawn_agent through the send-keys step and return tmux calls."""
        self._make_wrapper(mock_repo)
        # Make a builder role file so validate_role would pass (not strictly
        # needed for spawn_agent itself but mirrors a realistic call site).
        (mock_repo / ".loom" / "roles" / "builder.md").write_text("# builder")

        captured: list[list[str]] = []

        def fake_run(cmd, **kwargs):  # type: ignore[no-untyped-def]
            captured.append(list(cmd))
            stdout = ""
            # _get_pane_pid asks for pane_pid; return something so the verify
            # loop sees a process and exits early.
            if "list-panes" in cmd:
                stdout = "12345\n"
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout=stdout, stderr=""
            )

        with patch("loom_tools.agent_spawn.subprocess.run", side_effect=fake_run):
            with patch(
                "loom_tools.agent_spawn._is_claude_running", return_value=True
            ):
                # Stub the bypass-permissions modal poll.  Otherwise
                # spawn_agent drives the real _auto_accept_bypass_prompt loop,
                # which polls the (mocked, always-empty) pane for up to
                # DEFAULT_BYPASS_POLL_TIMEOUT (15s) of real time.sleep per
                # test — three of these serialized read as a hang under a
                # short wall-clock cap (see issue #3749).  We only assert on
                # the send-keys command line, which is emitted before this
                # poll, so stubbing it out is behavior-preserving here.
                with patch(
                    "loom_tools.agent_spawn._auto_accept_bypass_prompt",
                    return_value=True,
                ):
                    with patch.dict(os.environ, env or {}, clear=False):
                        spawn_agent(
                            role="builder",
                            name="test-agent",
                            args="",
                            worktree="",
                            repo_root=mock_repo,
                            verify_timeout=2,
                        )
        return captured

    def _find_send_keys_cmd(self, calls: list[list[str]]) -> str:
        """Return the command-string passed to `tmux send-keys` for claude_cmd."""
        for call in calls:
            if (
                "send-keys" in call
                and any(
                    "claude-wrapper.sh" in part or "claude " in part
                    for part in call
                )
            ):
                # send-keys -t SESSION 'CMD' C-m -> CMD is the second-to-last arg
                return call[-2]
        raise AssertionError(f"No send-keys call with claude found in {calls!r}")

    def test_no_token_when_pool_absent(self, mock_repo: pathlib.Path) -> None:
        """Backward-compat: no token pool -> no env-token injected."""
        # Ensure no inherited env token leaks into the test.
        env = {"CLAUDE_CODE_OAUTH_TOKEN": ""}
        calls = self._run_spawn(mock_repo, env=env)
        cmd = self._find_send_keys_cmd(calls)
        assert "CLAUDE_CODE_OAUTH_TOKEN=" not in cmd
        # Sanity: the wrapper invocation IS present.
        assert "claude-wrapper.sh" in cmd

    def test_token_injected_when_pool_present(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Pool exists + selection works -> env-token in command line."""
        (mock_repo / ".loom" / "tokens").mkdir()

        fake_pkg = types.ModuleType("loom_tools.tokens")
        fake_pkg.__path__ = []
        fake_select = types.ModuleType("loom_tools.tokens.select")
        fake_select.select_token = MagicMock(return_value="sk-ant-oat01-AAA")
        sys.modules["loom_tools.tokens"] = fake_pkg
        sys.modules["loom_tools.tokens.select"] = fake_select
        try:
            env = {"CLAUDE_CODE_OAUTH_TOKEN": ""}
            calls = self._run_spawn(mock_repo, env=env)
            cmd = self._find_send_keys_cmd(calls)
            assert "CLAUDE_CODE_OAUTH_TOKEN='sk-ant-oat01-AAA'" in cmd
            # The token prefix MUST come BEFORE the wrapper script invocation
            # otherwise it would be interpreted as an argument, not an env var.
            tok_pos = cmd.index("CLAUDE_CODE_OAUTH_TOKEN=")
            wrap_pos = cmd.index("claude-wrapper.sh")
            assert tok_pos < wrap_pos
        finally:
            sys.modules.pop("loom_tools.tokens.select", None)
            sys.modules.pop("loom_tools.tokens", None)

    def test_inherited_env_token_takes_precedence(
        self, mock_repo: pathlib.Path
    ) -> None:
        """User-set CLAUDE_CODE_OAUTH_TOKEN is propagated, pool not consulted."""
        (mock_repo / ".loom" / "tokens").mkdir()
        fake_pkg = types.ModuleType("loom_tools.tokens")
        fake_pkg.__path__ = []
        fake_select = types.ModuleType("loom_tools.tokens.select")
        fake_select.select_token = MagicMock(return_value="from-pool-XXX")
        sys.modules["loom_tools.tokens"] = fake_pkg
        sys.modules["loom_tools.tokens.select"] = fake_select
        try:
            env = {"CLAUDE_CODE_OAUTH_TOKEN": "from-caller-YYY"}
            calls = self._run_spawn(mock_repo, env=env)
            cmd = self._find_send_keys_cmd(calls)
            assert "CLAUDE_CODE_OAUTH_TOKEN='from-caller-YYY'" in cmd
            assert "from-pool-XXX" not in cmd
            # Selection must NOT have been called — caller env wins.
            fake_select.select_token.assert_not_called()
        finally:
            sys.modules.pop("loom_tools.tokens.select", None)
            sys.modules.pop("loom_tools.tokens", None)


# ---------------------------------------------------------------------------
# Keychain-cleanup regression: ensure the cleanup path is non-fatal when
# the per-agent Keychain entry was never written (i.e. env-token was used).
# ---------------------------------------------------------------------------


class TestKeychainCleanupWithEnvToken:
    """Regression test for AC #3 from the curator (#3236).

    When env-token bypasses Keychain entirely, the per-terminal Keychain
    entry is never written.  The existing cleanup logic must no-op rather
    than error.
    """

    def test_cleanup_agent_config_dir_idempotent(
        self, mock_repo: pathlib.Path
    ) -> None:
        """Calling cleanup on an agent that never wrote a Keychain entry
        must not raise, must not fail, and must remove the config dir."""
        from loom_tools.common.claude_config import (
            cleanup_agent_config_dir,
            setup_agent_config_dir,
        )

        # Setup a per-agent config dir but never invoke `claude` (so no
        # Keychain entry is created).  Cleanup should still succeed.
        config_dir = setup_agent_config_dir("test-agent", mock_repo)
        assert config_dir.exists()

        removed = cleanup_agent_config_dir("test-agent", mock_repo)
        assert removed is True
        assert not config_dir.exists()
