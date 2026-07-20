"""Tests for ``loom_tools.orphan_recovery`` — SAFETY-critical fail-safe (#3651).

The headline property proven here: after ``spawn-loop.sh`` (the only writer of
``.loom/spawn-loop-state.json``) was deleted in v0.11.0, orphan recovery must
**never** flip a live ``loom:building`` claim back to ``loom:issue`` (nor clean
its worktree) just because no roster writer exists. Absent authoritative
liveness data ⇒ treat every building issue as ALIVE (emit zero orphans).

Test map to acceptance criteria:

- ``test_no_liveness_source_emits_zero_orphans`` — the regression itself: no
  state file, no daemon, no locks ⇒ a stale, unclaimed building issue is NOT
  orphaned and NOT recovered.
- ``test_active_lock_protects_live_building_issue`` — AC (a): an active
  ``.loom/locks/issue-<N>/`` lock protects the issue even with no state file.
- ``test_genuinely_dead_claim_is_recoverable`` / ``_via_state_roster`` — AC (b):
  when an authoritative source IS present but does not list the issue, a
  genuinely-dead claim is still recovered.
"""

from __future__ import annotations

import pathlib
from unittest import mock

from loom_tools import orphan_recovery
from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask
from loom_tools.orphan_recovery import (
    LivenessEvidence,
    OrphanRecoveryResult,
    _locked_issue_numbers,
    check_untracked_building,
    gather_liveness_evidence,
    run_orphan_recovery,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo root with a ``.loom`` directory."""
    (tmp_path / ".loom").mkdir(parents=True, exist_ok=True)
    return tmp_path


def _make_lock(repo_root: pathlib.Path, issue: int) -> None:
    """Create a ``.loom/locks/issue-<N>/`` worktree-lifetime lock dir."""
    (repo_root / ".loom" / "locks" / f"issue-{issue}").mkdir(parents=True, exist_ok=True)


class _GhRecorder:
    """Record ``gh_run`` calls and refuse destructive label edits by default.

    A test that expects recovery passes ``allow_edit=True``.
    """

    def __init__(self, *, allow_edit: bool = False) -> None:
        self.calls: list[list[str]] = []
        self.allow_edit = allow_edit

    def __call__(self, args, **kwargs):  # noqa: ANN001 - test stub
        self.calls.append(list(args))
        if args[:2] == ["issue", "edit"] and not self.allow_edit:
            raise AssertionError(
                f"Unexpected destructive `gh issue edit` call: {args!r}"
            )
        return mock.Mock(returncode=0, stdout="", stderr="")

    @property
    def edited_issues(self) -> list[str]:
        return [c[2] for c in self.calls if c[:2] == ["issue", "edit"]]


# ---------------------------------------------------------------------------
# gather_liveness_evidence / _locked_issue_numbers unit tests
# ---------------------------------------------------------------------------


def test_locked_issue_numbers_reads_issue_dirs(tmp_path: pathlib.Path) -> None:
    repo = _make_repo(tmp_path)
    _make_lock(repo, 42)
    _make_lock(repo, 7)
    # Non-issue lock (worktree.sh's repo-global lock) must be ignored.
    (repo / ".loom" / "locks" / "worktree-add").mkdir(parents=True, exist_ok=True)
    assert _locked_issue_numbers(repo) == {42, 7}


def test_locked_issue_numbers_missing_dir_is_empty(tmp_path: pathlib.Path) -> None:
    repo = _make_repo(tmp_path)
    assert _locked_issue_numbers(repo) == set()


def test_gather_liveness_unavailable_when_no_sources(tmp_path: pathlib.Path) -> None:
    repo = _make_repo(tmp_path)
    evidence = gather_liveness_evidence(SpawnLoopState.absent(), repo)
    assert evidence.available is False
    assert evidence.live_issues == set()
    assert evidence.sources == []


def test_gather_liveness_available_from_locks(tmp_path: pathlib.Path) -> None:
    repo = _make_repo(tmp_path)
    _make_lock(repo, 42)
    evidence = gather_liveness_evidence(SpawnLoopState.absent(), repo)
    assert evidence.available is True
    assert evidence.live_issues == {42}
    assert ".loom/locks" in evidence.sources


def test_gather_liveness_available_from_present_state(tmp_path: pathlib.Path) -> None:
    repo = _make_repo(tmp_path)
    state = SpawnLoopState(running=[SpawnLoopTask(issue=5, pid=123)], present=True)
    evidence = gather_liveness_evidence(state, repo)
    assert evidence.available is True
    assert evidence.live_issues == {5}
    assert "spawn-loop-state.json" in evidence.sources


def test_gather_liveness_present_but_empty_is_available(tmp_path: pathlib.Path) -> None:
    """A present-but-empty roster IS an authoritative source (says 'none live')."""
    repo = _make_repo(tmp_path)
    state = SpawnLoopState(running=[], present=True)
    evidence = gather_liveness_evidence(state, repo)
    assert evidence.available is True
    assert evidence.live_issues == set()


# ---------------------------------------------------------------------------
# check_untracked_building fail-safe
# ---------------------------------------------------------------------------


def test_check_untracked_building_no_source_emits_zero(tmp_path: pathlib.Path) -> None:
    repo = _make_repo(tmp_path)
    result = OrphanRecoveryResult()
    evidence = LivenessEvidence(available=False)

    # gh must never be consulted when we have no liveness evidence.
    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        side_effect=AssertionError("gh_issue_list must not be called"),
    ):
        check_untracked_building(evidence, result, repo_root=repo)

    assert result.total_orphaned == 0


# ---------------------------------------------------------------------------
# run_orphan_recovery — end-to-end safety property
# ---------------------------------------------------------------------------


def test_no_liveness_source_emits_zero_orphans(tmp_path: pathlib.Path) -> None:
    """THE regression (#3651): no state file, no daemon, no locks.

    A stale, unclaimed ``loom:building`` issue must NOT be orphaned and must NOT
    be recovered — even under ``--recover``.
    """
    repo = _make_repo(tmp_path)  # no spawn-loop-state.json, no claims, no locks
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "live sweep, building > 10 min"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=9999,
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 0, "live building issue must not be orphaned"
    assert result.total_recovered == 0, "no recovery may occur without evidence"
    assert gh.edited_issues == [], "no loom:building -> loom:issue flip allowed"


def test_active_lock_protects_live_building_issue(tmp_path: pathlib.Path) -> None:
    """AC (a): an active ``.loom/locks/issue-42/`` lock keeps #42 alive.

    Even with the label older than the grace window and an 'abandoned' claim,
    the lock is authoritative liveness evidence, so #42 is not orphaned.
    """
    repo = _make_repo(tmp_path)
    _make_lock(repo, 42)  # <-- live sweep marker
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "live sweep with lock"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=9999,
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 0
    assert result.total_recovered == 0
    assert gh.edited_issues == []


def test_genuinely_dead_claim_is_recoverable(tmp_path: pathlib.Path) -> None:
    """AC (b): with a live source present that does NOT list #42, #42 recovers.

    A decoy lock for a *different* issue (#999) makes ``.loom/locks`` an
    authoritative source. #42 has no lock, an old label, and no valid claim, so
    it is a genuine orphan and IS recovered.
    """
    repo = _make_repo(tmp_path)
    _make_lock(repo, 999)  # some other live sweep — makes locks an active source
    gh = _GhRecorder(allow_edit=True)

    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "genuinely orphaned"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=9999,
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(
        orphan_recovery, "_has_recent_orphan_comment", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 1
    assert result.orphaned[0].issue == 42
    assert result.orphaned[0].type == "untracked_building"
    assert "42" in gh.edited_issues, "genuine orphan should be recovered"


def test_genuinely_dead_claim_recoverable_via_state_roster(
    tmp_path: pathlib.Path,
) -> None:
    """AC (b) variant: a present-but-empty state roster also enables recovery."""
    repo = _make_repo(tmp_path)
    gh = _GhRecorder(allow_edit=True)

    empty_present = SpawnLoopState(running=[], present=True)

    with mock.patch.object(
        orphan_recovery, "read_spawn_loop_state", return_value=empty_present,
    ), mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "genuinely orphaned"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=9999,
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(
        orphan_recovery, "_has_recent_orphan_comment", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 1
    assert "42" in gh.edited_issues


def test_valid_claim_protects_issue_even_with_source(tmp_path: pathlib.Path) -> None:
    """Defense-in-depth: a valid file-based claim still protects a building
    issue when a liveness source is present but does not list it."""
    repo = _make_repo(tmp_path)
    _make_lock(repo, 999)  # active source, but not our issue
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "claimed CLI sweep"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=9999,
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=True,  # <-- claim held
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 0
    assert gh.edited_issues == []


def test_label_grace_protects_issue_even_with_source(tmp_path: pathlib.Path) -> None:
    """Defense-in-depth: a recently-applied label protects a building issue."""
    repo = _make_repo(tmp_path)
    _make_lock(repo, 999)  # active source, but not our issue
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "freshly claimed"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=5,  # < grace
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 0
    assert gh.edited_issues == []


def test_stale_heartbeat_path_unaffected(tmp_path: pathlib.Path) -> None:
    """The stale-heartbeat orphan path still flags a dead-pid task when a
    roster is present (independent of the untracked-building fail-safe)."""
    repo = _make_repo(tmp_path)
    task = SpawnLoopTask(issue=77, pid=424242, last_heartbeat="2000-01-01T00:00:00Z")
    state = SpawnLoopState(running=[task], present=True)
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "read_spawn_loop_state", return_value=state,
    ), mock.patch.object(
        orphan_recovery, "gh_issue_list", return_value=[],
    ), mock.patch.object(
        orphan_recovery, "_pid_alive", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=False, verbose=True)

    stale = [o for o in result.orphaned if o.type == "stale_heartbeat"]
    assert len(stale) == 1
    assert stale[0].issue == 77
