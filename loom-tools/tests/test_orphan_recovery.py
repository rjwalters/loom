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

import json
import pathlib
from unittest import mock

import pytest

from loom_tools import orphan_recovery
from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask
from loom_tools.orphan_recovery import (
    LivenessEvidence,
    OrphanRecoveryResult,
    _locked_issue_numbers,
    check_untracked_building,
    format_result_human,
    gather_liveness_evidence,
    run_orphan_recovery,
)


@pytest.fixture(autouse=True)
def _isolated_journal_path(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
    """Make the #3953 sweep-journal read hermetic for every test in this module.

    ``gather_liveness_evidence`` resolves the journal path via
    ``_default_journal_path()``, which defaults to the REAL
    ``~/.loom/sweeps.json`` on the machine running the suite. Without this
    fixture, a test's outcome (e.g. ``evidence.available``) would depend on
    whether that file happens to exist on whoever's machine runs the tests.

    Rather than replacing ``_default_journal_path`` itself (which would also
    defeat a test of its own env-var-override logic), this patches
    ``pathlib.Path.home()`` to a fresh, per-test fake home directory and
    clears ``LOOM_SWEEPS_JOURNAL_PATH`` — so ``_default_journal_path()`` runs
    its REAL resolution logic and lands on a path that is absent by default.
    Tests that want journal-present behavior write to this same path
    explicitly via :func:`_write_journal`.
    """
    fake_home = tmp_path / "fake-home"
    fake_home.mkdir(parents=True, exist_ok=True)
    monkeypatch.delenv("LOOM_SWEEPS_JOURNAL_PATH", raising=False)
    monkeypatch.setattr(pathlib.Path, "home", lambda: fake_home)
    return fake_home / ".loom" / "sweeps.json"


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


def _write_journal(journal_path: pathlib.Path, entries: list[dict]) -> None:
    """Write a minimal sweep-journal file at ``journal_path``."""
    journal_path.parent.mkdir(parents=True, exist_ok=True)
    journal_path.write_text(json.dumps({"version": 1, "entries": entries}))


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


# ---------------------------------------------------------------------------
# Machine-level sweep journal (issue #3953)
# ---------------------------------------------------------------------------
#
# The journal gives orphan recovery an authoritative liveness source that
# SURVIVES a daemon restart (unlike the in-memory registry `_query_daemon_live_
# issues` merely stubs). These tests cover: (1) `gather_liveness_evidence`
# reading the journal correctly, and (2) the end-to-end `run_orphan_recovery`
# staleness-threshold selection it drives in `check_untracked_building`.


def test_gather_liveness_journal_absent_file_is_not_a_source(
    tmp_path: pathlib.Path,
) -> None:
    """No journal file at all -> not a source, byte-for-byte pre-#3953 behavior."""
    repo = _make_repo(tmp_path)
    evidence = gather_liveness_evidence(SpawnLoopState.absent(), repo)
    assert evidence.available is False
    assert evidence.journal_present is False
    assert evidence.journal_issues == set()


def test_gather_liveness_journal_present_dead_pid_is_source_not_live(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path,
) -> None:
    """A journal entry with a dead PID makes the journal a source, but the
    issue is NOT added to `live_issues` (it's the strongest orphan evidence,
    not liveness evidence)."""
    repo = _make_repo(tmp_path)
    _write_journal(_isolated_journal_path, [{"repo": str(repo), "issue": 42, "pid": 999999}])

    with mock.patch.object(orphan_recovery, "_pid_alive", return_value=False):
        evidence = gather_liveness_evidence(SpawnLoopState.absent(), repo)

    assert evidence.available is True
    assert "sweep-journal" in evidence.sources
    assert evidence.journal_present is True
    assert evidence.journal_issues == {42}
    assert 42 not in evidence.live_issues


def test_gather_liveness_journal_present_live_pid_is_live(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path,
) -> None:
    """A journal entry with a LIVE PID is proof of life: added to `live_issues`."""
    repo = _make_repo(tmp_path)
    _write_journal(_isolated_journal_path, [{"repo": str(repo), "issue": 42, "pid": 1}])

    with mock.patch.object(orphan_recovery, "_pid_alive", return_value=True):
        evidence = gather_liveness_evidence(SpawnLoopState.absent(), repo)

    assert evidence.available is True
    assert evidence.journal_issues == {42}
    assert 42 in evidence.live_issues


def test_gather_liveness_journal_entry_scoped_to_repo(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path,
) -> None:
    """An entry for a DIFFERENT repo must not leak into this repo's evidence."""
    repo = _make_repo(tmp_path)
    other_repo = tmp_path.parent / "some-other-repo"
    _write_journal(_isolated_journal_path, [{"repo": str(other_repo), "issue": 42, "pid": 1}])

    with mock.patch.object(orphan_recovery, "_pid_alive", return_value=True):
        evidence = gather_liveness_evidence(SpawnLoopState.absent(), repo)

    # The journal file exists (this repo's directory), so it IS a source, but
    # it has no entry scoped to *this* repo.
    assert evidence.journal_present is True
    assert evidence.journal_issues == set()
    assert 42 not in evidence.live_issues


def test_gather_liveness_journal_corrupt_file_degrades_to_absent_entries(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path,
) -> None:
    """A corrupt journal file must not crash -- it degrades to zero entries.

    The file's mere *presence* still marks the journal as a contributing
    source (mirrors `sweep_journal::load`'s tolerant-corrupt-file behavior on
    the Rust side): a corrupt journal is not proof of anything, so no entries
    are read from it, but its presence is not silently ignored either.
    """
    repo = _make_repo(tmp_path)
    _isolated_journal_path.parent.mkdir(parents=True, exist_ok=True)
    _isolated_journal_path.write_text("{ not json")

    evidence = gather_liveness_evidence(SpawnLoopState.absent(), repo)

    assert evidence.journal_present is True
    assert evidence.journal_issues == set()


def test_journal_dead_pid_claim_is_recoverable(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path,
) -> None:
    """AC #3953: a claim whose recorded PID is dead IS reclaimable (subject to
    the standard, short label-age grace period -- not the long no-record one).
    """
    repo = _make_repo(tmp_path)
    _write_journal(_isolated_journal_path, [{"repo": str(repo), "issue": 42, "pid": 999999}])
    gh = _GhRecorder(allow_edit=True)

    with mock.patch.object(
        orphan_recovery, "_pid_alive", return_value=False,
    ), mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "dead sweep after daemon restart"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=700,  # > grace(600), well under 4h
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(
        orphan_recovery, "_has_recent_orphan_comment", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 1
    assert result.orphaned[0].reason == "journal_pid_dead"
    assert "42" in gh.edited_issues


def test_journal_live_pid_claim_still_refuses(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path,
) -> None:
    """AC #3953: still refuses to reclaim when the record shows a live PID —
    even with a very old label."""
    repo = _make_repo(tmp_path)
    _write_journal(_isolated_journal_path, [{"repo": str(repo), "issue": 42, "pid": 1}])
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "_pid_alive", return_value=True,
    ), mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "long-running live sweep"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=99999,
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 0
    assert gh.edited_issues == []


def test_journal_no_record_within_stale_hours_is_not_recovered(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    """No journal entry at all, journal IS a source (has an entry for another
    issue): must NOT reclaim before LOOM_STALE_BUILDING_HOURS, even though the
    label is already past the (much shorter) default grace period."""
    repo = _make_repo(tmp_path)
    _write_journal(_isolated_journal_path, [{"repo": str(repo), "issue": 999, "pid": 1}])
    monkeypatch.setenv("LOOM_STALE_BUILDING_HOURS", "4")
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "_pid_alive", return_value=True,
    ), mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "no journal entry, not stale enough yet"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=3600,  # 1h: > 600s grace, < 4h
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 0
    assert gh.edited_issues == []


def test_journal_no_record_past_stale_hours_is_recovered(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Same setup, but past LOOM_STALE_BUILDING_HOURS -> now reclaimable."""
    repo = _make_repo(tmp_path)
    _write_journal(_isolated_journal_path, [{"repo": str(repo), "issue": 999, "pid": 1}])
    monkeypatch.setenv("LOOM_STALE_BUILDING_HOURS", "4")
    gh = _GhRecorder(allow_edit=True)

    with mock.patch.object(
        orphan_recovery, "_pid_alive", return_value=True,
    ), mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "no journal entry, now stale"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=20000,  # > 4h
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(
        orphan_recovery, "_has_recent_orphan_comment", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 1
    assert result.orphaned[0].reason == "no_journal_record_stale"
    assert "42" in gh.edited_issues


def test_journal_absent_falls_back_to_label_grace_period(
    tmp_path: pathlib.Path,
) -> None:
    """No journal file at all (only a lock-based source): unaffected by
    #3953 -- the standard label_grace_period alone governs, matching
    pre-#3953 behavior exactly."""
    repo = _make_repo(tmp_path)
    _make_lock(repo, 999)  # unrelated active source
    gh = _GhRecorder(allow_edit=True)

    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "no journal at all"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=700,  # > grace(600s)
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(
        orphan_recovery, "_has_recent_orphan_comment", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        result = run_orphan_recovery(repo, recover=True, verbose=True)

    assert result.total_orphaned == 1
    assert result.orphaned[0].reason == "no_spawn_loop_entry"
    assert "42" in gh.edited_issues


def test_get_stale_building_hours_default_and_override(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("LOOM_STALE_BUILDING_HOURS", raising=False)
    assert orphan_recovery._get_stale_building_hours() == orphan_recovery.DEFAULT_STALE_BUILDING_HOURS

    monkeypatch.setenv("LOOM_STALE_BUILDING_HOURS", "2.5")
    assert orphan_recovery._get_stale_building_hours() == 2.5

    # Non-positive / unparseable falls back to the default.
    monkeypatch.setenv("LOOM_STALE_BUILDING_HOURS", "0")
    assert orphan_recovery._get_stale_building_hours() == orphan_recovery.DEFAULT_STALE_BUILDING_HOURS
    monkeypatch.setenv("LOOM_STALE_BUILDING_HOURS", "garbage")
    assert orphan_recovery._get_stale_building_hours() == orphan_recovery.DEFAULT_STALE_BUILDING_HOURS


def test_default_journal_path_env_override(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
    custom = tmp_path / "custom-sweeps.json"
    monkeypatch.setenv("LOOM_SWEEPS_JOURNAL_PATH", str(custom))
    assert orphan_recovery._default_journal_path() == custom
    monkeypatch.delenv("LOOM_SWEEPS_JOURNAL_PATH", raising=False)


def test_journal_repo_matches_exact_and_resolved(tmp_path: pathlib.Path) -> None:
    repo = _make_repo(tmp_path)
    assert orphan_recovery._journal_repo_matches(str(repo), repo) is True
    assert orphan_recovery._journal_repo_matches(str(repo) + "/", repo) is True
    assert orphan_recovery._journal_repo_matches("/some/other/repo", repo) is False


# ---------------------------------------------------------------------------
# Watched entries — issue #3975: a staleness-gated skip must never be silent.
# ---------------------------------------------------------------------------


def test_no_record_stale_gate_skip_is_watched_not_silent(
    tmp_path: pathlib.Path, _isolated_journal_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Companion to ``test_journal_no_record_within_stale_hours_is_not_recovered``:
    the exact scenario that used to be invisible without --verbose must now
    show up in ``result.watched`` (issue #3975)."""
    repo = _make_repo(tmp_path)
    _write_journal(_isolated_journal_path, [{"repo": str(repo), "issue": 999, "pid": 1}])
    monkeypatch.setenv("LOOM_STALE_BUILDING_HOURS", "4")
    gh = _GhRecorder(allow_edit=False)

    with mock.patch.object(
        orphan_recovery, "_pid_alive", return_value=True,
    ), mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "no journal entry, not stale enough yet"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=3600,  # 1h: > 600s grace, < 4h
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ), mock.patch.object(orphan_recovery, "gh_run", gh):
        # verbose=False -- the default, non-verbose invocation that used to
        # leave this skip completely invisible.
        result = run_orphan_recovery(repo, recover=True, verbose=False)

    assert result.total_orphaned == 0
    assert gh.edited_issues == []
    assert result.total_watched == 1
    watched = result.watched[0]
    assert watched.issue == 42
    assert watched.reason == "no_journal_record_stale"
    assert watched.age_seconds == 3600
    assert watched.threshold_seconds == pytest.approx(4 * 3600)


def test_label_grace_gate_skip_is_watched_not_silent(tmp_path: pathlib.Path) -> None:
    """A fresh loom:building label (within the short grace period) with no
    journal at all -- must also be watched, not silently dropped."""
    repo = _make_repo(tmp_path)
    _make_lock(repo, 999)  # unrelated active source

    with mock.patch.object(
        orphan_recovery, "gh_issue_list",
        return_value=[{"number": 42, "title": "freshly labeled"}],
    ), mock.patch.object(
        orphan_recovery, "_get_building_label_age", return_value=30,  # well under 600s grace
    ), mock.patch.object(
        orphan_recovery, "has_valid_claim", return_value=False,
    ):
        result = run_orphan_recovery(repo, recover=False, verbose=False)

    assert result.total_orphaned == 0
    assert result.total_watched == 1
    watched = result.watched[0]
    assert watched.issue == 42
    assert watched.reason == "no_spawn_loop_entry"
    assert watched.age_seconds == 30
    assert watched.threshold_seconds == orphan_recovery.DEFAULT_LABEL_GRACE_PERIOD


def test_watched_entries_serialize_in_to_dict(tmp_path: pathlib.Path) -> None:
    result = OrphanRecoveryResult()
    result.watched.append(
        orphan_recovery.WatchedEntry(
            issue=7, title="t", reason="no_spawn_loop_entry",
            age_seconds=10, threshold_seconds=600.0,
        )
    )
    d = result.to_dict()
    assert d["total_watched"] == 1
    assert d["watched"][0]["issue"] == 7
    assert d["watched"][0]["reason"] == "no_spawn_loop_entry"


def test_format_result_human_lists_watched_entries_even_with_zero_orphans() -> None:
    result = OrphanRecoveryResult()
    result.watched.append(
        orphan_recovery.WatchedEntry(
            issue=42, title="freshly labeled", reason="no_spawn_loop_entry",
            age_seconds=30, threshold_seconds=600.0,
        )
    )
    text = format_result_human(result)
    assert "No orphaned tasks found" in text
    assert "#42" in text
    assert "no_spawn_loop_entry" in text
