"""Tests for the orphan_recovery module (spawn-loop port).

Trimmed in Phase 3.1.6 (#3395) to cover only the spawn-loop code paths.
The pre-port version of this file had ~2100 lines testing daemon-state +
progress-file orphan detection; all of that ports away in Phase 3.3 when
the daemon brain is deleted (see docs/migration/daemon-state-consumers.md).

What remains:

- ``OrphanEntry`` / ``RecoveryEntry`` / ``OrphanRecoveryResult`` shape.
- ``_pid_alive`` (the only new helper that isn't shared with stuck_detection).
- ``check_untracked_building`` against ``SpawnLoopState`` (forge cross-check).
- ``check_stale_heartbeats`` against ``SpawnLoopState`` (heartbeat + PID liveness).
- ``recover_issue`` (label flip, claim-check, grace period, dedup comment,
  worktree cleanup) — preserved verbatim from the pre-port behavior since
  this function did not change.
- ``run_orphan_recovery`` end-to-end with spawn-loop inputs.
- ``main`` CLI exit-code convention (0 / 1 / 2).
"""

from __future__ import annotations

import json
import pathlib
from datetime import datetime, timedelta, timezone
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.claim import claim_issue
from loom_tools.common.time_utils import now_utc
from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask
from loom_tools.orphan_recovery import (
    DEFAULT_HEARTBEAT_STALE_THRESHOLD,
    DEFAULT_LABEL_GRACE_PERIOD,
    OrphanEntry,
    OrphanRecoveryResult,
    RecoveryEntry,
    _cleanup_stale_worktree,
    _get_building_label_age,
    _get_heartbeat_stale_threshold,
    _get_label_grace_period,
    _has_recent_orphan_comment,
    _pid_alive,
    check_stale_heartbeats,
    check_untracked_building,
    format_result_human,
    format_result_json,
    main,
    recover_issue,
    run_orphan_recovery,
)


# ─── shape ──────────────────────────────────────────────────────────────────

class TestEntryShapes:
    def test_orphan_entry_to_dict_minimal(self) -> None:
        o = OrphanEntry(type="untracked_building", reason="no_spawn_loop_entry")
        d = o.to_dict()
        assert d == {"type": "untracked_building", "reason": "no_spawn_loop_entry"}

    def test_orphan_entry_to_dict_full(self) -> None:
        o = OrphanEntry(
            type="stale_heartbeat",
            issue=42,
            pid=12345,
            title="some title",
            reason="heartbeat_stale",
            age_seconds=600,
        )
        d = o.to_dict()
        assert d["type"] == "stale_heartbeat"
        assert d["issue"] == 42
        assert d["pid"] == 12345
        assert d["title"] == "some title"
        assert d["reason"] == "heartbeat_stale"
        assert d["age_seconds"] == 600

    def test_recovery_entry_to_dict(self) -> None:
        r = RecoveryEntry(action="reset_issue_label", issue=42, reason="x")
        d = r.to_dict()
        assert d == {"action": "reset_issue_label", "reason": "x", "issue": 42}

    def test_result_to_dict(self) -> None:
        result = OrphanRecoveryResult(
            orphaned=[
                OrphanEntry(
                    type="untracked_building",
                    issue=42,
                    title="title",
                    reason="no_spawn_loop_entry",
                ),
            ],
            recovered=[
                RecoveryEntry(action="reset_issue_label", issue=42, reason="x"),
            ],
            recover_mode=True,
        )
        d = result.to_dict()
        assert d["total_orphaned"] == 1
        assert d["total_recovered"] == 1
        assert d["recover_mode"] is True


# ─── _pid_alive ─────────────────────────────────────────────────────────────

class TestPidAlive:
    def test_zero_pid_returns_false(self) -> None:
        assert _pid_alive(0) is False

    def test_negative_pid_returns_false(self) -> None:
        assert _pid_alive(-1) is False

    def test_dead_pid_returns_false(self) -> None:
        with patch("loom_tools.orphan_recovery.os.kill",
                   side_effect=ProcessLookupError()):
            assert _pid_alive(99999) is False

    def test_alive_owned_pid_returns_true(self) -> None:
        with patch("loom_tools.orphan_recovery.os.kill", return_value=None):
            assert _pid_alive(12345) is True

    def test_alive_unowned_pid_returns_true(self) -> None:
        # PermissionError means the PID is alive but owned by someone else.
        with patch("loom_tools.orphan_recovery.os.kill",
                   side_effect=PermissionError()):
            assert _pid_alive(1) is True

    def test_other_oserror_treated_as_alive(self) -> None:
        with patch("loom_tools.orphan_recovery.os.kill",
                   side_effect=OSError()):
            assert _pid_alive(12345) is True


# ─── thresholds ─────────────────────────────────────────────────────────────

class TestHeartbeatThreshold:
    def test_default_threshold(self) -> None:
        with patch.dict("os.environ", {}, clear=True):
            assert _get_heartbeat_stale_threshold() == DEFAULT_HEARTBEAT_STALE_THRESHOLD

    def test_env_var_override(self) -> None:
        with patch.dict("os.environ", {"LOOM_HEARTBEAT_STALE_THRESHOLD": "600"}):
            assert _get_heartbeat_stale_threshold() == 600

    def test_env_var_invalid(self) -> None:
        with patch.dict("os.environ", {"LOOM_HEARTBEAT_STALE_THRESHOLD": "x"}):
            assert _get_heartbeat_stale_threshold() == DEFAULT_HEARTBEAT_STALE_THRESHOLD


class TestLabelGracePeriod:
    def test_default(self) -> None:
        with patch.dict("os.environ", {}, clear=True):
            assert _get_label_grace_period() == DEFAULT_LABEL_GRACE_PERIOD

    def test_env_override(self) -> None:
        with patch.dict("os.environ", {"LOOM_LABEL_GRACE_PERIOD": "900"}):
            assert _get_label_grace_period() == 900

    def test_env_invalid(self) -> None:
        with patch.dict("os.environ", {"LOOM_LABEL_GRACE_PERIOD": "x"}):
            assert _get_label_grace_period() == DEFAULT_LABEL_GRACE_PERIOD


# ─── check_untracked_building (spawn-loop edition) ──────────────────────────

class TestCheckUntrackedBuilding:
    def test_no_building_issues(self) -> None:
        state = SpawnLoopState(present=True)
        result = OrphanRecoveryResult()
        with patch("loom_tools.orphan_recovery.gh_issue_list", return_value=[]):
            check_untracked_building(state, result)
        assert result.total_orphaned == 0

    def test_tracked_issue_skipped(self) -> None:
        state = SpawnLoopState(
            present=True,
            running=[SpawnLoopTask(issue=42, pid=123)],
        )
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ):
            check_untracked_building(state, result)
        assert result.total_orphaned == 0

    def test_untracked_issue_flagged(self) -> None:
        state = SpawnLoopState(present=True)
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ):
            check_untracked_building(state, result)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "untracked_building"
        assert result.orphaned[0].issue == 42
        assert result.orphaned[0].reason == "no_spawn_loop_entry"

    def test_absent_state_still_cross_checks(self) -> None:
        # When the spawn-loop state file is absent we still want the forge
        # cross-check to run; nothing is tracked locally, so any
        # loom:building issue is orphaned.
        state = SpawnLoopState.absent()
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ):
            check_untracked_building(state, result)
        assert result.total_orphaned == 1

    def test_gh_failure_handled(self) -> None:
        state = SpawnLoopState(present=True)
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            side_effect=Exception("network"),
        ):
            check_untracked_building(state, result)
        assert result.total_orphaned == 0

    def test_valid_claim_skips(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True, exist_ok=True)
        (tmp_path / ".git").mkdir(exist_ok=True)
        claim_issue(tmp_path, 42, "cli-shepherd")

        state = SpawnLoopState(present=True)
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ):
            check_untracked_building(state, result, repo_root=tmp_path)
        assert result.total_orphaned == 0

    def test_no_repo_root_logs_warning(self) -> None:
        state = SpawnLoopState(present=True)
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch("loom_tools.orphan_recovery.log_warning") as mock_warn:
            check_untracked_building(state, result)
        assert any("repo_root is None" in str(c) for c in mock_warn.call_args_list)
        assert result.total_orphaned == 1

    def test_recent_label_within_grace_period_skipped(self) -> None:
        state = SpawnLoopState(present=True)
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._get_building_label_age",
            return_value=60,  # 1 minute ago
        ):
            check_untracked_building(state, result, label_grace_period=600)
        assert result.total_orphaned == 0

    def test_old_label_past_grace_period_flagged(self) -> None:
        state = SpawnLoopState(present=True)
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._get_building_label_age",
            return_value=800,  # past 600s grace
        ):
            check_untracked_building(state, result, label_grace_period=600)
        assert result.total_orphaned == 1

    def test_grace_period_zero_disables_label_age_lookup(self) -> None:
        state = SpawnLoopState(present=True)
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        result = OrphanRecoveryResult()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._get_building_label_age",
        ) as mock_age:
            check_untracked_building(state, result, label_grace_period=0)
        mock_age.assert_not_called()
        assert result.total_orphaned == 1


# ─── check_stale_heartbeats ─────────────────────────────────────────────────

class TestCheckStaleHeartbeats:
    def test_no_tasks(self) -> None:
        state = SpawnLoopState(present=True)
        result = OrphanRecoveryResult()
        check_stale_heartbeats(state, result)
        assert result.total_orphaned == 0

    def test_fresh_heartbeat_skipped(self) -> None:
        state = SpawnLoopState(
            present=True,
            running=[
                SpawnLoopTask(
                    issue=42,
                    pid=12345,
                    last_heartbeat=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
                ),
            ],
        )
        result = OrphanRecoveryResult()
        check_stale_heartbeats(state, result, heartbeat_threshold=300)
        assert result.total_orphaned == 0

    def test_no_heartbeat_field_skipped(self) -> None:
        # Pre-#3411 state files don't have last_heartbeat; nothing to flag.
        state = SpawnLoopState(
            present=True,
            running=[SpawnLoopTask(issue=42, pid=12345, last_heartbeat=None)],
        )
        result = OrphanRecoveryResult()
        check_stale_heartbeats(state, result)
        assert result.total_orphaned == 0

    def test_unparseable_heartbeat_skipped(self) -> None:
        state = SpawnLoopState(
            present=True,
            running=[
                SpawnLoopTask(issue=42, pid=12345, last_heartbeat="not-a-timestamp"),
            ],
        )
        result = OrphanRecoveryResult()
        check_stale_heartbeats(state, result)
        assert result.total_orphaned == 0

    def test_stale_heartbeat_dead_pid_flagged(self) -> None:
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        state = SpawnLoopState(
            present=True,
            running=[SpawnLoopTask(issue=42, pid=99999, last_heartbeat=stale_hb)],
        )
        result = OrphanRecoveryResult()
        with patch("loom_tools.orphan_recovery._pid_alive", return_value=False):
            check_stale_heartbeats(state, result, heartbeat_threshold=300)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "stale_heartbeat"
        assert result.orphaned[0].issue == 42
        assert result.orphaned[0].pid == 99999
        assert result.orphaned[0].age_seconds is not None
        assert result.orphaned[0].age_seconds >= 600

    def test_stale_heartbeat_live_pid_skipped(self) -> None:
        # Loop may have just been SIGSTOPped — never tear down active work.
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        state = SpawnLoopState(
            present=True,
            running=[SpawnLoopTask(issue=42, pid=12345, last_heartbeat=stale_hb)],
        )
        result = OrphanRecoveryResult()
        with patch("loom_tools.orphan_recovery._pid_alive", return_value=True):
            check_stale_heartbeats(state, result, heartbeat_threshold=300)
        assert result.total_orphaned == 0

    def test_multiple_tasks_mixed(self) -> None:
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        fresh = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        state = SpawnLoopState(
            present=True,
            running=[
                SpawnLoopTask(issue=42, pid=12345, last_heartbeat=fresh),
                SpawnLoopTask(issue=99, pid=99999, last_heartbeat=stale),
            ],
        )
        result = OrphanRecoveryResult()
        with patch("loom_tools.orphan_recovery._pid_alive", return_value=False):
            check_stale_heartbeats(state, result, heartbeat_threshold=300)
        assert result.total_orphaned == 1
        assert result.orphaned[0].issue == 99


# ─── recover_issue (preserved from pre-port behavior) ───────────────────────

class TestRecoverIssue:
    def test_label_swap_and_comment(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=None
        ), patch(
            "loom_tools.orphan_recovery._has_recent_orphan_comment", return_value=False
        ), patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "no_spawn_loop_entry", result)

        # Two gh calls: label edit + comment.
        assert mock_gh.call_count == 2
        label_call = mock_gh.call_args_list[0]
        args = label_call[0][0]
        assert "edit" in args
        assert "--remove-label" in args
        assert "loom:building" in args
        assert "--add-label" in args
        assert "loom:issue" in args
        assert result.total_recovered == 1
        assert result.recovered[0].action == "reset_issue_label"

    def test_label_edit_failure_skips_recovery(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._has_recent_orphan_comment", return_value=False
        ), patch(
            "loom_tools.orphan_recovery.gh_run", side_effect=Exception("fail")
        ):
            recover_issue(42, "x", result)
        assert result.total_recovered == 0

    def test_valid_claim_skips_recovery(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True, exist_ok=True)
        (tmp_path / ".git").mkdir(exist_ok=True)
        claim_issue(tmp_path, 42, "cli-sweep")

        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=None
        ), patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "x", result, repo_root=tmp_path)
        mock_gh.assert_not_called()
        assert result.total_recovered == 0

    def test_recent_label_skips_recovery(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=60
        ), patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "x", result, label_grace_period=600)
        mock_gh.assert_not_called()
        assert result.total_recovered == 0

    def test_old_label_proceeds(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=800
        ), patch(
            "loom_tools.orphan_recovery._has_recent_orphan_comment", return_value=False
        ), patch("loom_tools.orphan_recovery.gh_run"):
            recover_issue(42, "x", result, label_grace_period=600)
        assert result.total_recovered == 1

    def test_grace_period_zero_disables_check(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._get_building_label_age"
        ) as mock_age, patch(
            "loom_tools.orphan_recovery._has_recent_orphan_comment", return_value=False
        ), patch("loom_tools.orphan_recovery.gh_run"):
            recover_issue(42, "x", result, label_grace_period=0)
        mock_age.assert_not_called()
        assert result.total_recovered == 1

    def test_worktree_cleanup_recorded(self, tmp_path: pathlib.Path) -> None:
        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=None
        ), patch(
            "loom_tools.orphan_recovery.has_valid_claim", return_value=False
        ), patch(
            "loom_tools.orphan_recovery._cleanup_stale_worktree", return_value=True
        ) as mock_cleanup, patch(
            "loom_tools.orphan_recovery._has_recent_orphan_comment", return_value=False
        ), patch("loom_tools.orphan_recovery.gh_run"):
            recover_issue(42, "x", result, repo_root=tmp_path)
        mock_cleanup.assert_called_once_with(tmp_path, 42)
        actions = {r.action for r in result.recovered}
        assert "cleanup_stale_worktree" in actions
        assert "reset_issue_label" in actions

    def test_recent_comment_skips_second_comment(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)
        with patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=None
        ), patch(
            "loom_tools.orphan_recovery._has_recent_orphan_comment", return_value=True
        ), patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "x", result)
        # Only the label edit, no follow-up comment.
        assert mock_gh.call_count == 1
        assert result.total_recovered == 1


# ─── _has_recent_orphan_comment ─────────────────────────────────────────────

class TestHasRecentOrphanComment:
    def test_detects_recent(self) -> None:
        m = MagicMock()
        m.returncode = 0
        m.stdout = "2026-02-17T21:00:00Z\n"
        with patch(
            "loom_tools.orphan_recovery.gh_run", return_value=m
        ), patch(
            "loom_tools.orphan_recovery.elapsed_seconds", return_value=60
        ):
            assert _has_recent_orphan_comment(42) is True

    def test_allows_old(self) -> None:
        m = MagicMock()
        m.returncode = 0
        m.stdout = "2026-02-17T20:00:00Z\n"
        with patch(
            "loom_tools.orphan_recovery.gh_run", return_value=m
        ), patch(
            "loom_tools.orphan_recovery.elapsed_seconds", return_value=600
        ):
            assert _has_recent_orphan_comment(42) is False

    def test_no_comments(self) -> None:
        m = MagicMock()
        m.returncode = 0
        m.stdout = ""
        with patch("loom_tools.orphan_recovery.gh_run", return_value=m):
            assert _has_recent_orphan_comment(42) is False

    def test_failure_returns_false(self) -> None:
        m = MagicMock()
        m.returncode = 1
        m.stdout = ""
        with patch("loom_tools.orphan_recovery.gh_run", return_value=m):
            assert _has_recent_orphan_comment(42) is False


# ─── _get_building_label_age ────────────────────────────────────────────────

class TestGetBuildingLabelAge:
    def test_returns_age_for_recent_label(self) -> None:
        recent_ts = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        mock_result = type(
            "R", (), {"returncode": 0, "stdout": f'"{recent_ts}"\n'}
        )()
        with patch(
            "loom_tools.orphan_recovery.get_repo_nwo", return_value="owner/repo"
        ), patch(
            "loom_tools.orphan_recovery.gh_run", return_value=mock_result
        ):
            age = _get_building_label_age(42)
        assert age is not None and age < 10

    def test_returns_none_on_no_nwo(self) -> None:
        with patch(
            "loom_tools.orphan_recovery.get_repo_nwo", return_value=None
        ):
            assert _get_building_label_age(42) is None

    def test_returns_none_on_gh_failure(self) -> None:
        mock_result = type("R", (), {"returncode": 1, "stdout": ""})()
        with patch(
            "loom_tools.orphan_recovery.get_repo_nwo", return_value="owner/repo"
        ), patch(
            "loom_tools.orphan_recovery.gh_run", return_value=mock_result
        ):
            assert _get_building_label_age(42) is None

    def test_returns_none_on_null(self) -> None:
        mock_result = type("R", (), {"returncode": 0, "stdout": "null\n"})()
        with patch(
            "loom_tools.orphan_recovery.get_repo_nwo", return_value="owner/repo"
        ), patch(
            "loom_tools.orphan_recovery.gh_run", return_value=mock_result
        ):
            assert _get_building_label_age(42) is None


# ─── _cleanup_stale_worktree ────────────────────────────────────────────────

class TestCleanupStaleWorktree:
    def test_no_worktree(self, tmp_path: pathlib.Path) -> None:
        assert _cleanup_stale_worktree(tmp_path, 42) is False

    def test_clean_worktree_removed(self, tmp_path: pathlib.Path) -> None:
        worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktree_path.mkdir(parents=True)
        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            def side_effect(cmd, **kwargs):
                m = MagicMock()
                m.returncode = 0
                if "log" in cmd:
                    m.stdout = ""
                elif "status" in cmd:
                    m.stdout = ""
                elif "rev-parse" in cmd:
                    m.stdout = "feature/issue-42\n"
                else:
                    m.stdout = ""
                return m
            mock_run.side_effect = side_effect
            assert _cleanup_stale_worktree(tmp_path, 42) is True

    def test_worktree_with_commits_not_removed(self, tmp_path: pathlib.Path) -> None:
        worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktree_path.mkdir(parents=True)
        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            def side_effect(cmd, **kwargs):
                m = MagicMock()
                m.returncode = 0
                if "log" in cmd:
                    m.stdout = "abc1234 some commit\n"
                else:
                    m.stdout = ""
                return m
            mock_run.side_effect = side_effect
            assert _cleanup_stale_worktree(tmp_path, 42) is False

    def test_worktree_with_meaningful_changes_not_removed(
        self, tmp_path: pathlib.Path,
    ) -> None:
        worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
        worktree_path.mkdir(parents=True)
        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            def side_effect(cmd, **kwargs):
                m = MagicMock()
                m.returncode = 0
                if "log" in cmd:
                    m.stdout = ""
                elif "status" in cmd:
                    m.stdout = " M src/main.py\n"
                else:
                    m.stdout = ""
                return m
            mock_run.side_effect = side_effect
            assert _cleanup_stale_worktree(tmp_path, 42) is False


# ─── run_orphan_recovery end-to-end ─────────────────────────────────────────

class TestRunOrphanRecovery:
    def test_absent_spawn_loop_state_runs_forge_check(
        self, tmp_path: pathlib.Path
    ) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ):
            result = run_orphan_recovery(tmp_path, recover=False)
        assert result.total_orphaned == 0

    def test_untracked_building_detected(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        # Empty spawn-loop state present on disk.
        (loom_dir / "spawn-loop-state.json").write_text(
            json.dumps({"started_at": "2026-06-02T00:00:00Z", "running": []})
        )
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=900
        ):
            result = run_orphan_recovery(tmp_path, recover=False)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "untracked_building"

    def test_stale_heartbeat_detected(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        (loom_dir / "spawn-loop-state.json").write_text(
            json.dumps(
                {
                    "started_at": "2026-06-02T00:00:00Z",
                    "running": [
                        {
                            "issue": 42,
                            "pid": 99999,
                            "last_heartbeat": stale_hb,
                        }
                    ],
                }
            )
        )
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ), patch("loom_tools.orphan_recovery._pid_alive", return_value=False):
            result = run_orphan_recovery(tmp_path, recover=False)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "stale_heartbeat"
        assert result.orphaned[0].issue == 42

    def test_recovery_mode_calls_recover_issue(
        self, tmp_path: pathlib.Path
    ) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "spawn-loop-state.json").write_text(
            json.dumps({"started_at": "x", "running": []})
        )
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=900
        ), patch(
            "loom_tools.orphan_recovery.recover_issue"
        ) as mock_recover:
            run_orphan_recovery(tmp_path, recover=True)
        mock_recover.assert_called_once()
        assert mock_recover.call_args.args[0] == 42

    def test_label_grace_period_env_forwarded(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        # Set up both an untracked-building orphan and a stale-heartbeat
        # orphan; both should receive recover_issue calls with grace=0.
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        (loom_dir / "spawn-loop-state.json").write_text(
            json.dumps(
                {
                    "running": [
                        {
                            "issue": 99,
                            "pid": 99999,
                            "last_heartbeat": stale_hb,
                        }
                    ]
                }
            )
        )
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        with patch.dict("os.environ", {"LOOM_LABEL_GRACE_PERIOD": "0"}), patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._pid_alive", return_value=False
        ), patch(
            "loom_tools.orphan_recovery.recover_issue"
        ) as mock_recover:
            run_orphan_recovery(tmp_path, recover=True)
        assert mock_recover.call_count == 2
        for call in mock_recover.call_args_list:
            assert call.kwargs.get("label_grace_period") == 0


# ─── CLI exit codes ─────────────────────────────────────────────────────────

class TestMainCli:
    def test_help_exits_zero(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0

    def test_no_repo_returns_1(self) -> None:
        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            side_effect=FileNotFoundError("no repo"),
        ):
            assert main([]) == 1

    def test_no_orphans_returns_0(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        with patch(
            "loom_tools.orphan_recovery.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ):
            assert main([]) == 0

    def test_orphans_dry_run_returns_2(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        with patch(
            "loom_tools.orphan_recovery.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=900
        ):
            assert main([]) == 2

    def test_orphans_recover_mode_returns_0(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        building = [{"number": 42, "title": "T", "labels": [], "state": "OPEN"}]
        with patch(
            "loom_tools.orphan_recovery.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=building
        ), patch(
            "loom_tools.orphan_recovery._get_building_label_age", return_value=900
        ), patch("loom_tools.orphan_recovery.gh_run"):
            assert main(["--recover"]) == 0

    def test_json_output(
        self, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
    ) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        with patch(
            "loom_tools.orphan_recovery.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ):
            assert main(["--json"]) == 0
        out = capsys.readouterr().out
        data = json.loads(out)
        assert data["total_orphaned"] == 0


# ─── formatters ─────────────────────────────────────────────────────────────

class TestFormatters:
    def test_json_format_empty(self) -> None:
        result = OrphanRecoveryResult()
        data = json.loads(format_result_json(result))
        assert data["total_orphaned"] == 0
        assert data["recovered"] == []

    def test_human_format_no_orphans(self) -> None:
        assert "No orphaned tasks found" in format_result_human(OrphanRecoveryResult())

    def test_human_format_with_orphans_dry_run(self) -> None:
        result = OrphanRecoveryResult(
            orphaned=[
                OrphanEntry(
                    type="untracked_building",
                    issue=42,
                    title="Fix bug",
                    reason="no_spawn_loop_entry",
                ),
                OrphanEntry(
                    type="stale_heartbeat",
                    issue=99,
                    pid=12345,
                    age_seconds=600,
                    reason="heartbeat_stale",
                ),
            ],
        )
        out = format_result_human(result)
        assert "Found 2 orphaned task(s)" in out
        assert "#42" in out
        assert "#99" in out
        assert "--recover" in out

    def test_human_format_with_recovery(self) -> None:
        result = OrphanRecoveryResult(
            orphaned=[
                OrphanEntry(
                    type="untracked_building",
                    issue=42,
                    title="t",
                    reason="no_spawn_loop_entry",
                ),
            ],
            recovered=[
                RecoveryEntry(action="reset_issue_label", issue=42, reason="x"),
            ],
            recover_mode=True,
        )
        out = format_result_human(result)
        assert "Recovered 1 item(s)" in out
