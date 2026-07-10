"""Tests for ``loom_tools.clean.clean_branches``.

These cover the three regressions diagnosed in issue #3471:

1. **Pattern broadening** -- ``clean_branches`` must also delete branches
   whose ``origin/<branch>`` ref no longer exists, regardless of naming
   pattern (the old filter only matched ``feature/issue-*``).
2. **gh pinned to repo root** -- ``gh issue view`` must be called with
   ``cwd=repo_root`` so an unrelated worktree's cwd never leaks in.
3. **Surface gh failures** -- when the ``gh`` probe fails, the branch is
   counted under ``errored_branches`` and a warning is logged. The
   branch must NOT be deleted (issue state is unknown -> fail closed).
"""

from __future__ import annotations

import pathlib
import subprocess
from unittest import mock

import pytest

from loom_tools.clean import CleanupStats, clean_branches


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_completed(stdout: str = "", returncode: int = 0) -> subprocess.CompletedProcess[str]:
    """Build a ``CompletedProcess`` stub for ``subprocess.run`` patches."""
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr="")


class _SubprocessRouter:
    """Route ``subprocess.run`` calls through per-command handlers.

    Tests register handlers keyed off a tuple of leading argv tokens
    (e.g. ``("git", "branch")``). Unrecognized calls raise to surface
    test gaps rather than silently passing.
    """

    def __init__(self) -> None:
        self.calls: list[tuple[list[str], pathlib.Path | None]] = []
        self.deleted_branches: list[str] = []
        self._handlers: list[
            tuple[tuple[str, ...], "callable[[list[str], pathlib.Path | None], subprocess.CompletedProcess[str]]"]
        ] = []

    def register(
        self,
        prefix: tuple[str, ...],
        handler,
    ) -> None:
        self._handlers.append((prefix, handler))

    def __call__(
        self,
        cmd: list[str],
        *args,
        **kwargs,
    ) -> subprocess.CompletedProcess[str]:
        cwd = kwargs.get("cwd")
        self.calls.append((list(cmd), cwd))
        # Track branch deletions for assertion convenience.
        if (
            len(cmd) >= 3
            and cmd[0] == "git"
            and cmd[1] == "branch"
            and cmd[2] == "-D"
        ):
            self.deleted_branches.append(cmd[3])
        for prefix, handler in self._handlers:
            if tuple(cmd[: len(prefix)]) == prefix:
                result = handler(cmd, cwd)
                if kwargs.get("check") and result.returncode != 0:
                    raise subprocess.CalledProcessError(result.returncode, cmd)
                return result
        raise AssertionError(f"Unexpected subprocess.run call: {cmd!r}")


# ---------------------------------------------------------------------------
# Regression: surface gh failures (Fix #3)
# ---------------------------------------------------------------------------


class TestGhFailureSurfaced:
    """When ``gh issue view`` exits non-zero, we must:
    - bump ``stats.errored_branches`` (not silently skip),
    - emit a ``log_warning`` so the operator sees it,
    - NOT delete the branch (state is unknown).
    """

    def test_gh_nonzero_exit_logs_warning_and_bumps_errored(
        self,
        tmp_path: pathlib.Path,
        caplog: pytest.LogCaptureFixture,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        repo_root = tmp_path
        router = _SubprocessRouter()

        # `git branch` -> emit one feature/issue-1 branch whose origin ref
        # exists (so we exercise the issue-state pass).
        router.register(
            ("git", "branch", "--format=%(refname:short)"),
            lambda cmd, cwd: _make_completed(stdout="feature/issue-1\n"),
        )
        # Default-branch + current-branch probes -> emit something sane.
        router.register(
            ("git", "symbolic-ref", "--short", "HEAD"),
            lambda cmd, cwd: _make_completed(stdout="main\n"),
        )
        router.register(
            ("git", "symbolic-ref", "refs/remotes/origin/HEAD"),
            lambda cmd, cwd: _make_completed(stdout="refs/remotes/origin/main\n"),
        )
        router.register(
            ("git", "worktree", "list", "--porcelain"),
            lambda cmd, cwd: _make_completed(stdout=""),
        )
        # `git show-ref --verify` -> remote exists (exit 0).
        router.register(
            ("git", "show-ref", "--verify"),
            lambda cmd, cwd: _make_completed(returncode=0),
        )

        # Mock gh_run to return a non-zero exit (network failure, auth
        # failure, repo mismatch, etc.). This is the regression bait --
        # the old code silently fell through.
        def fake_gh_run(args, *, check=False, cwd=None):  # noqa: ARG001
            return _make_completed(stdout="", returncode=1)

        with mock.patch("loom_tools.clean.subprocess.run", router), mock.patch(
            "loom_tools.clean.gh_run", side_effect=fake_gh_run
        ):
            stats = CleanupStats()
            # caplog only captures stdlib `logging`; the loom_tools log
            # helpers print to stderr. Capture stderr too.
            clean_branches(repo_root, stats, dry_run=False)

        captured = capsys.readouterr()

        # 1. errored counter bumped.
        assert stats.errored_branches == 1, (
            f"expected errored_branches=1, got {stats.errored_branches}"
        )
        # 2. branch NOT deleted (state unknown -> fail closed).
        assert "feature/issue-1" not in router.deleted_branches
        assert stats.cleaned_branches == 0
        # 3. A warning was emitted to stderr that mentions the issue.
        assert (
            "#1" in captured.err and "feature/issue-1" in captured.err
        ), f"expected warning mentioning #1 and the branch, got stderr: {captured.err!r}"


# ---------------------------------------------------------------------------
# Regression: pattern broadening via remote-ref check (Fix #1)
# ---------------------------------------------------------------------------


class TestStaleRemoteRefDetection:
    """Branches whose ``origin/<branch>`` no longer exists are stale."""

    def test_pr_branch_with_missing_remote_is_deleted(
        self,
        tmp_path: pathlib.Path,
    ) -> None:
        repo_root = tmp_path
        router = _SubprocessRouter()

        router.register(
            ("git", "branch", "--format=%(refname:short)"),
            lambda cmd, cwd: _make_completed(stdout="pr-3472\nmain\n"),
        )
        router.register(
            ("git", "symbolic-ref", "--short", "HEAD"),
            lambda cmd, cwd: _make_completed(stdout="main\n"),
        )
        router.register(
            ("git", "symbolic-ref", "refs/remotes/origin/HEAD"),
            lambda cmd, cwd: _make_completed(stdout="refs/remotes/origin/main\n"),
        )
        router.register(
            ("git", "worktree", "list", "--porcelain"),
            lambda cmd, cwd: _make_completed(stdout=""),
        )

        # `git show-ref --verify refs/remotes/origin/<branch>`:
        # - for `pr-3472` -> not found (exit 1, stale).
        # - for `main` -> protected anyway, won't be probed.
        def show_ref(cmd: list[str], cwd: pathlib.Path | None) -> subprocess.CompletedProcess[str]:
            ref = cmd[-1]
            if ref == "refs/remotes/origin/pr-3472":
                return _make_completed(returncode=1)
            return _make_completed(returncode=0)

        router.register(("git", "show-ref", "--verify"), show_ref)

        # `git branch -D pr-3472` -> success.
        router.register(
            ("git", "branch", "-D"),
            lambda cmd, cwd: _make_completed(returncode=0),
        )

        # gh_run must NOT be called for the pattern-broadening pass since
        # `pr-3472` doesn't match `feature/issue-*` AND its remote is
        # already gone, so it's deleted in pass 1.
        gh_run_calls = []

        def fake_gh_run(args, *, check=False, cwd=None):
            gh_run_calls.append((list(args), cwd))
            return _make_completed(stdout="OPEN", returncode=0)

        with mock.patch("loom_tools.clean.subprocess.run", router), mock.patch(
            "loom_tools.clean.gh_run", side_effect=fake_gh_run
        ):
            stats = CleanupStats()
            clean_branches(repo_root, stats, dry_run=False)

        assert "pr-3472" in router.deleted_branches
        assert "main" not in router.deleted_branches
        assert stats.cleaned_branches == 1
        # No gh call needed -- pass-1 catches it.
        assert gh_run_calls == []

    def test_main_and_current_branch_are_never_deleted(
        self,
        tmp_path: pathlib.Path,
    ) -> None:
        """Even if the remote-ref probe fails, ``main`` and the current
        branch must be preserved."""
        repo_root = tmp_path
        router = _SubprocessRouter()

        router.register(
            ("git", "branch", "--format=%(refname:short)"),
            lambda cmd, cwd: _make_completed(stdout="feature/issue-3471\nmain\n"),
        )
        router.register(
            ("git", "symbolic-ref", "--short", "HEAD"),
            lambda cmd, cwd: _make_completed(stdout="feature/issue-3471\n"),
        )
        router.register(
            ("git", "symbolic-ref", "refs/remotes/origin/HEAD"),
            lambda cmd, cwd: _make_completed(stdout="refs/remotes/origin/main\n"),
        )
        router.register(
            ("git", "worktree", "list", "--porcelain"),
            lambda cmd, cwd: _make_completed(stdout=""),
        )
        # show-ref always says "not found" -- protected list must save us.
        router.register(
            ("git", "show-ref", "--verify"),
            lambda cmd, cwd: _make_completed(returncode=1),
        )

        # `git branch -D` should never be reached.
        router.register(
            ("git", "branch", "-D"),
            lambda cmd, cwd: _make_completed(returncode=0),
        )

        with mock.patch("loom_tools.clean.subprocess.run", router), mock.patch(
            "loom_tools.clean.gh_run", side_effect=AssertionError("gh_run should not be called")
        ):
            stats = CleanupStats()
            clean_branches(repo_root, stats, dry_run=False)

        assert router.deleted_branches == [], (
            f"expected no deletions, got {router.deleted_branches!r}"
        )
        assert stats.cleaned_branches == 0
        assert stats.errored_branches == 0


# ---------------------------------------------------------------------------
# Regression: gh pinned to repo root (Fix #2)
# ---------------------------------------------------------------------------


class TestGhPinnedToRepoRoot:
    """`gh issue view` must be invoked with ``cwd=repo_root``."""

    def test_gh_called_with_repo_root_cwd(self, tmp_path: pathlib.Path) -> None:
        repo_root = tmp_path
        router = _SubprocessRouter()

        router.register(
            ("git", "branch", "--format=%(refname:short)"),
            lambda cmd, cwd: _make_completed(stdout="feature/issue-42\n"),
        )
        router.register(
            ("git", "symbolic-ref", "--short", "HEAD"),
            lambda cmd, cwd: _make_completed(stdout="main\n"),
        )
        router.register(
            ("git", "symbolic-ref", "refs/remotes/origin/HEAD"),
            lambda cmd, cwd: _make_completed(stdout="refs/remotes/origin/main\n"),
        )
        router.register(
            ("git", "show-ref", "--verify"),
            lambda cmd, cwd: _make_completed(returncode=0),
        )

        captured_cwd: list[pathlib.Path | None] = []

        def fake_gh_run(args, *, check=False, cwd=None):
            captured_cwd.append(cwd)
            return _make_completed(stdout="OPEN", returncode=0)

        with mock.patch("loom_tools.clean.subprocess.run", router), mock.patch(
            "loom_tools.clean.gh_run", side_effect=fake_gh_run
        ):
            stats = CleanupStats()
            clean_branches(repo_root, stats, dry_run=False)

        assert captured_cwd == [repo_root], (
            f"expected gh_run cwd={repo_root!r}, got {captured_cwd!r}"
        )


# ---------------------------------------------------------------------------
# Two-way is_under_loom gate in evaluate_aggressive_candidate (issue #3537)
# ---------------------------------------------------------------------------


class TestEvaluateAggressiveCandidateGate:
    """Cover the two-way ``is_under_loom`` gate in
    :func:`evaluate_aggressive_candidate`.

    Mirrors ``worktree_root.rs``'s ``gate_matches_default_path_worktree`` /
    ``gate_matches_override_path_worktree`` /
    ``gate_matches_default_substring_even_with_override_set`` /
    ``gate_rejects_unrelated_path``.

    Worktrees are built with ``branch=None`` so the earlier PR / active-shepherd
    branches are skipped and evaluation falls straight through to the gate at
    step 4. "Under loom" worktrees carry a ``.loom-managed`` sentinel so they
    pass the sentinel check and continue to the ``force`` fallback
    (``force_override_unreachable``, a REMOVE decision); an unrelated path is
    rejected at the gate with ``user_owned`` (a KEEP decision). The two decisions
    are the observable proof that the gate accepted vs. rejected the path.
    """

    def _worktree_info(self, path: pathlib.Path):
        from loom_tools.clean import WorktreeInfo

        return WorktreeInfo(path=path, head=None, branch=None)

    def _make_managed(self, path: pathlib.Path) -> None:
        from loom_tools.clean import LOOM_MANAGED_SENTINEL

        path.mkdir(parents=True, exist_ok=True)
        (path / LOOM_MANAGED_SENTINEL).write_text("")

    def test_gate_accepts_default_path_worktree(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worktree under the default .loom/worktrees base is Loom-managed."""
        from loom_tools.clean import DECISION_REMOVE, evaluate_aggressive_candidate

        monkeypatch.delenv("LOOM_WORKTREE_ROOT", raising=False)
        repo_root = tmp_path / "my-repo"
        repo_root.mkdir()
        wt_path = repo_root / ".loom" / "worktrees" / "issue-42"
        self._make_managed(wt_path)

        decision, _reason = evaluate_aggressive_candidate(
            self._worktree_info(wt_path),
            repo_root,
            active_shepherd_issues=set(),
            min_age_seconds=0,
            force=True,
        )
        # Accepted by the gate → proceeds past user_owned to the force fallback.
        assert decision == DECISION_REMOVE

    def test_gate_accepts_override_path_worktree(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worktree under the override root is Loom-managed (override-aware)."""
        from loom_tools.clean import DECISION_REMOVE, evaluate_aggressive_candidate

        override = tmp_path / "ext"
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", str(override))
        repo_root = tmp_path / "my-repo"
        repo_root.mkdir()
        # Override root is namespaced by repo basename: <override>/my-repo/issue-7
        wt_path = override / "my-repo" / "issue-7"
        self._make_managed(wt_path)

        decision, _reason = evaluate_aggressive_candidate(
            self._worktree_info(wt_path),
            repo_root,
            active_shepherd_issues=set(),
            min_age_seconds=0,
            force=True,
        )
        assert decision == DECISION_REMOVE

    def test_gate_accepts_default_substring_with_override_set(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Mixed setup: override configured, but a worktree still lives under the
        historical .loom/worktrees base — the substring branch must still match."""
        from loom_tools.clean import DECISION_REMOVE, evaluate_aggressive_candidate

        override = tmp_path / "ext"
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", str(override))
        repo_root = tmp_path / "my-repo"
        repo_root.mkdir()
        wt_path = repo_root / ".loom" / "worktrees" / "issue-99"
        self._make_managed(wt_path)

        decision, _reason = evaluate_aggressive_candidate(
            self._worktree_info(wt_path),
            repo_root,
            active_shepherd_issues=set(),
            min_age_seconds=0,
            force=True,
        )
        assert decision == DECISION_REMOVE

    def test_gate_rejects_unrelated_path(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A worktree at an unrelated path is not Loom-managed → user_owned."""
        from loom_tools.clean import (
            DECISION_KEEP,
            evaluate_aggressive_candidate,
        )

        override = tmp_path / "ext"
        monkeypatch.setenv("LOOM_WORKTREE_ROOT", str(override))
        repo_root = tmp_path / "my-repo"
        repo_root.mkdir()
        wt_path = tmp_path / "some" / "other" / "place" / "issue-42"
        self._make_managed(wt_path)

        decision, reason = evaluate_aggressive_candidate(
            self._worktree_info(wt_path),
            repo_root,
            active_shepherd_issues=set(),
            min_age_seconds=0,
            force=True,
        )
        assert decision == DECISION_KEEP
        assert reason == "user_owned"
