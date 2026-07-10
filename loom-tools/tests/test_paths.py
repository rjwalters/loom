"""Tests for loom_tools.common.paths module."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from loom_tools.common.paths import LoomPaths, NamingConventions, is_worktree_path


class TestLoomPaths:
    """Tests for LoomPaths class."""

    def test_loom_dir(self, tmp_path: Path) -> None:
        """Test loom_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.loom_dir == tmp_path / ".loom"

    def test_scripts_dir(self, tmp_path: Path) -> None:
        """Test scripts_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.scripts_dir == tmp_path / ".loom" / "scripts"

    def test_worktrees_dir(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Test worktrees_dir property (default, no override)."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        paths = LoomPaths(tmp_path)
        assert paths.worktrees_dir == tmp_path / ".loom" / "worktrees"

    def test_logs_dir(self, tmp_path: Path) -> None:
        """Test logs_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.logs_dir == tmp_path / ".loom" / "logs"

    def test_health_metrics_file(self, tmp_path: Path) -> None:
        """Test health_metrics_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.health_metrics_file == tmp_path / ".loom" / "health-metrics.json"

    def test_alerts_file(self, tmp_path: Path) -> None:
        """Test alerts_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.alerts_file == tmp_path / ".loom" / "alerts.json"

    def test_stuck_history_file(self, tmp_path: Path) -> None:
        """Test stuck_history_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.stuck_history_file == tmp_path / ".loom" / "stuck-history.json"

    def test_config_file(self, tmp_path: Path) -> None:
        """Test config_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.config_file == tmp_path / ".loom" / "config.json"

    def test_stop_daemon_file(self, tmp_path: Path) -> None:
        """Test stop_daemon_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.stop_daemon_file == tmp_path / ".loom" / "stop-daemon"

    def test_stop_shepherds_file(self, tmp_path: Path) -> None:
        """Test stop_shepherds_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.stop_shepherds_file == tmp_path / ".loom" / "stop-shepherds"

    def test_worktree_path(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Test worktree_path method (default, no override)."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        paths = LoomPaths(tmp_path)
        assert paths.worktree_path(42) == tmp_path / ".loom" / "worktrees" / "issue-42"
        assert paths.worktree_path(123) == tmp_path / ".loom" / "worktrees" / "issue-123"

    def test_builder_log_file(self, tmp_path: Path) -> None:
        """Test builder_log_file method."""
        paths = LoomPaths(tmp_path)
        assert paths.builder_log_file(42) == tmp_path / ".loom" / "logs" / "loom-builder-issue-42.log"


class TestWorktreeRootResolution:
    """Tests for the override-aware worktree-root resolver.

    Mirrors the non-gate cases in loom-daemon/src/worktree_root.rs's test module
    and the bash loom_worktree_root() precedence. pytest's monkeypatch fixture
    isolates env vars per test, so no manual save/restore is needed.
    """

    def _repo(self, tmp_path: Path) -> Path:
        """Create and return a repo root with a fixed basename under tmp_path."""
        repo_root = tmp_path / "my-repo"
        repo_root.mkdir()
        return repo_root

    def _write_config(self, repo_root: Path, body: dict) -> None:
        loom_dir = repo_root / ".loom"
        loom_dir.mkdir(exist_ok=True)
        (loom_dir / "config.json").write_text(json.dumps(body))

    def test_default_when_no_override(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """No override → byte-for-byte default path."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = self._repo(tmp_path)
        paths = LoomPaths(repo_root)
        assert paths.worktrees_dir == repo_root / ".loom" / "worktrees"
        assert (
            paths.worktree_path(42)
            == repo_root / ".loom" / "worktrees" / "issue-42"
        )

    def test_env_override_namespaces_by_basename(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Absolute env override is namespaced by repo basename."""
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "/Volumes/Stripe")
        repo_root = self._repo(tmp_path)
        assert LoomPaths(repo_root).worktrees_dir == Path("/Volumes/Stripe/my-repo")
        assert (
            LoomPaths(repo_root).worktree_path(7)
            == Path("/Volumes/Stripe/my-repo/issue-7")
        )

    def test_env_override_strips_trailing_slash(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A trailing slash on the env override is stripped before namespacing."""
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "/Volumes/Stripe/")
        repo_root = self._repo(tmp_path)
        assert LoomPaths(repo_root).worktrees_dir == Path("/Volumes/Stripe/my-repo")

    def test_config_override_namespaces_by_basename(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Absolute config override is namespaced by repo basename."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = self._repo(tmp_path)
        self._write_config(repo_root, {"worktree": {"root": "/Volumes/Ext"}})
        assert LoomPaths(repo_root).worktrees_dir == Path("/Volumes/Ext/my-repo")

    def test_env_beats_config(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Env var takes precedence over the config key when both are set."""
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "/Volumes/Env")
        repo_root = self._repo(tmp_path)
        self._write_config(repo_root, {"worktree": {"root": "/Volumes/Config"}})
        assert LoomPaths(repo_root).worktrees_dir == Path("/Volumes/Env/my-repo")

    def test_relative_env_override_falls_back_to_default(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A relative env override warns and falls back to the default path."""
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "relative/path")
        repo_root = self._repo(tmp_path)
        with pytest.warns(UserWarning, match="must be an absolute path"):
            got = LoomPaths(repo_root).worktrees_dir
        assert got == repo_root / ".loom" / "worktrees"

    def test_relative_config_override_falls_back_to_default(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A relative config override warns and falls back to the default path."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = self._repo(tmp_path)
        self._write_config(repo_root, {"worktree": {"root": "relative/path"}})
        with pytest.warns(UserWarning, match="must be an absolute path"):
            got = LoomPaths(repo_root).worktrees_dir
        assert got == repo_root / ".loom" / "worktrees"

    def test_empty_env_override_falls_through_to_default(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """An empty env var is treated as unset (falls through to default)."""
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "")
        repo_root = self._repo(tmp_path)
        assert LoomPaths(repo_root).worktrees_dir == repo_root / ".loom" / "worktrees"

    def test_missing_config_key_uses_default(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A config file without worktree.root uses the default."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = self._repo(tmp_path)
        self._write_config(repo_root, {"terminals": []})
        assert LoomPaths(repo_root).worktrees_dir == repo_root / ".loom" / "worktrees"

    def test_malformed_config_uses_default(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A malformed config file soft-fails to the default (no crash)."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = self._repo(tmp_path)
        loom_dir = repo_root / ".loom"
        loom_dir.mkdir(exist_ok=True)
        (loom_dir / "config.json").write_text("{not valid json")
        assert LoomPaths(repo_root).worktrees_dir == repo_root / ".loom" / "worktrees"

    def test_missing_config_file_uses_default(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """No config file at all soft-fails to the default."""
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = self._repo(tmp_path)
        assert LoomPaths(repo_root).worktrees_dir == repo_root / ".loom" / "worktrees"


class TestIsWorktreePath:
    """Tests for the two-way is_worktree_path gate.

    Mirrors worktree_root.rs::gate_* cases: default-path match, override-path
    match, mixed-setup substring match with an override configured, and rejection
    of unrelated paths.
    """

    def _repo(self, tmp_path: Path) -> Path:
        repo_root = tmp_path / "my-repo"
        repo_root.mkdir()
        return repo_root

    def test_gate_matches_default_path_worktree(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = self._repo(tmp_path)
        wt = repo_root / ".loom" / "worktrees" / "issue-42"
        assert is_worktree_path(wt, repo_root)

    def test_gate_matches_override_path_worktree(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "/Volumes/Stripe")
        repo_root = self._repo(tmp_path)
        wt = Path("/Volumes/Stripe/my-repo/issue-42")
        assert is_worktree_path(wt, repo_root)

    def test_gate_matches_default_substring_even_with_override_set(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Mixed setup: override configured, worktree still under default base."""
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "/Volumes/Stripe")
        repo_root = self._repo(tmp_path)
        wt = repo_root / ".loom" / "worktrees" / "issue-99"
        assert is_worktree_path(wt, repo_root)

    def test_gate_rejects_unrelated_path(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", "/Volumes/Stripe")
        repo_root = self._repo(tmp_path)
        unrelated = Path("/some/other/place/issue-42")
        assert not is_worktree_path(unrelated, repo_root)


class TestNamingConventions:
    """Tests for NamingConventions class."""

    def test_branch_name(self) -> None:
        """Test branch_name static method."""
        assert NamingConventions.branch_name(42) == "feature/issue-42"
        assert NamingConventions.branch_name(123) == "feature/issue-123"
        assert NamingConventions.branch_name(1) == "feature/issue-1"

    def test_worktree_name(self) -> None:
        """Test worktree_name static method."""
        assert NamingConventions.worktree_name(42) == "issue-42"
        assert NamingConventions.worktree_name(123) == "issue-123"
        assert NamingConventions.worktree_name(1) == "issue-1"

    def test_issue_from_branch_valid(self) -> None:
        """Test issue_from_branch with valid branch names."""
        assert NamingConventions.issue_from_branch("feature/issue-42") == 42
        assert NamingConventions.issue_from_branch("feature/issue-123") == 123
        assert NamingConventions.issue_from_branch("feature/issue-1") == 1

    def test_issue_from_branch_invalid(self) -> None:
        """Test issue_from_branch with invalid branch names."""
        assert NamingConventions.issue_from_branch("main") is None
        assert NamingConventions.issue_from_branch("feature/other") is None
        assert NamingConventions.issue_from_branch("feature/issue-") is None
        assert NamingConventions.issue_from_branch("feature/issue-abc") is None
        assert NamingConventions.issue_from_branch("") is None

    def test_issue_from_worktree_valid(self) -> None:
        """Test issue_from_worktree with valid worktree names."""
        assert NamingConventions.issue_from_worktree("issue-42") == 42
        assert NamingConventions.issue_from_worktree("issue-123") == 123
        assert NamingConventions.issue_from_worktree("issue-1") == 1

    def test_issue_from_worktree_invalid(self) -> None:
        """Test issue_from_worktree with invalid worktree names."""
        assert NamingConventions.issue_from_worktree("main") is None
        assert NamingConventions.issue_from_worktree("issue-") is None
        assert NamingConventions.issue_from_worktree("issue-abc") is None
        assert NamingConventions.issue_from_worktree("") is None
        assert NamingConventions.issue_from_worktree("terminal-1") is None

    def test_roundtrip_branch(self) -> None:
        """Test that branch_name and issue_from_branch are inverses."""
        for issue in [1, 42, 123, 9999]:
            branch = NamingConventions.branch_name(issue)
            assert NamingConventions.issue_from_branch(branch) == issue

    def test_roundtrip_worktree(self) -> None:
        """Test that worktree_name and issue_from_worktree are inverses."""
        for issue in [1, 42, 123, 9999]:
            worktree = NamingConventions.worktree_name(issue)
            assert NamingConventions.issue_from_worktree(worktree) == issue

    def test_constants(self) -> None:
        """Test that class constants are correctly defined."""
        assert NamingConventions.BRANCH_PREFIX == "feature/issue-"
        assert NamingConventions.WORKTREE_PREFIX == "issue-"

    # --- pr_title tests ---

    def test_pr_title_already_has_prefix(self) -> None:
        """Titles that already start with a conventional commit prefix are kept."""
        assert NamingConventions.pr_title("fix: resolve crash on startup") == "fix: resolve crash on startup"
        assert NamingConventions.pr_title("feat: add dark mode toggle") == "feat: add dark mode toggle"
        assert NamingConventions.pr_title("refactor: simplify auth flow") == "refactor: simplify auth flow"
        assert NamingConventions.pr_title("docs: update README") == "docs: update README"
        assert NamingConventions.pr_title("test: add unit tests for parser") == "test: add unit tests for parser"
        assert NamingConventions.pr_title("chore: bump dependencies") == "chore: bump dependencies"
        assert NamingConventions.pr_title("perf: cache database queries") == "perf: cache database queries"

    def test_pr_title_prefix_case_normalised(self) -> None:
        """Prefix casing is normalised to lowercase."""
        assert NamingConventions.pr_title("Fix: resolve crash") == "fix: resolve crash"
        assert NamingConventions.pr_title("FEAT: add feature") == "feat: add feature"
        assert NamingConventions.pr_title("Refactor: clean up code") == "refactor: clean up code"

    def test_pr_title_no_prefix_gets_feat(self) -> None:
        """Titles without a prefix get 'feat:' prepended."""
        assert NamingConventions.pr_title("Add dark mode toggle") == "feat: add dark mode toggle"
        assert NamingConventions.pr_title("Builder should generate descriptive PR titles") == "feat: builder should generate descriptive PR titles"

    def test_pr_title_empty_with_issue_number(self) -> None:
        """Empty titles fall back to issue number."""
        assert NamingConventions.pr_title("", 42) == "feat: implement changes for issue #42"
        assert NamingConventions.pr_title("  ", 42) == "feat: implement changes for issue #42"

    def test_pr_title_empty_without_issue_number(self) -> None:
        """Empty titles without issue number produce generic title."""
        assert NamingConventions.pr_title("") == "feat: implement changes"
        assert NamingConventions.pr_title(None) == "feat: implement changes"  # type: ignore[arg-type]

    def test_pr_title_never_returns_issue_n(self) -> None:
        """Verify we never produce the old 'Issue #N' format."""
        result = NamingConventions.pr_title("", 42)
        assert not result.startswith("Issue #")
        result2 = NamingConventions.pr_title("Some title", 42)
        assert not result2.startswith("Issue #")
