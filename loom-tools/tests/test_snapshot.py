"""Tests for loom_tools.snapshot."""

from __future__ import annotations

import json
import pathlib
from datetime import datetime, timezone
from unittest import mock

import pytest

from loom_tools.models.daemon_state import (
    BlockedIssueRetry,
    DaemonState,
    ShepherdEntry,
    SupportRoleEntry,
    SystematicFailure as SystematicFailureModel,
)
# Rename to avoid conflict with SystematicFailureState from snapshot
SystematicFailure = SystematicFailureModel
from loom_tools.snapshot import (
    EnhancedProgress,
    PipelineHealth,
    SnapshotConfig,
    SupportRoleState,
    SystematicFailureState,
    TmuxPool,
    build_snapshot,
    compute_health,
    compute_pipeline_health,
    compute_recommended_actions,
    compute_shepherd_progress,
    compute_support_role_state,
    compute_systematic_failure_state,
    detect_orphaned_shepherds,
    detect_tmux_pool,
    main,
    sort_issues_by_strategy,
    validate_task_ids,
)

FIXTURES = pathlib.Path(__file__).parent / "fixtures"


def _load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text())


# Fixed "now" for deterministic tests
NOW = datetime(2026, 1, 30, 18, 0, 0, tzinfo=timezone.utc)


def _cfg(**overrides: object) -> SnapshotConfig:
    """Create a config with test-friendly defaults."""
    kw: dict = {
        "issue_threshold": 3,
        "max_shepherds": 3,
        "max_proposals": 5,
        "architect_cooldown": 1800,
        "hermit_cooldown": 1800,
        "guide_interval": 900,
        "champion_interval": 600,
        "doctor_interval": 300,
        "auditor_interval": 600,
        "judge_interval": 300,
        "issue_strategy": "fifo",
        "heartbeat_stale_threshold": 120,
        "tmux_socket": "loom",
        "systematic_failure_cooldown": 1800,
        "systematic_failure_max_probes": 3,
    }
    kw.update(overrides)
    return SnapshotConfig(**kw)


# ---------------------------------------------------------------------------
# SnapshotConfig
# ---------------------------------------------------------------------------


class TestSnapshotConfig:
    def test_defaults(self) -> None:
        cfg = SnapshotConfig()
        assert cfg.issue_threshold == 3
        assert cfg.max_shepherds == 3
        assert cfg.issue_strategy == "fifo"
        assert cfg.heartbeat_stale_threshold == 120

    def test_from_env(self) -> None:
        env = {
            "LOOM_ISSUE_THRESHOLD": "5",
            "LOOM_MAX_SHEPHERDS": "6",
            "LOOM_ISSUE_STRATEGY": "lifo",
            "LOOM_TMUX_SOCKET": "test",
        }
        with mock.patch.dict("os.environ", env, clear=False):
            cfg = SnapshotConfig.from_env()
        assert cfg.issue_threshold == 5
        assert cfg.max_shepherds == 6
        assert cfg.issue_strategy == "lifo"
        assert cfg.tmux_socket == "test"

    def test_from_env_invalid_int(self) -> None:
        with mock.patch.dict("os.environ", {"LOOM_ISSUE_THRESHOLD": "abc"}):
            cfg = SnapshotConfig.from_env()
        assert cfg.issue_threshold == 3  # fallback to default

    def test_to_dict(self) -> None:
        cfg = _cfg()
        d = cfg.to_dict()
        assert d["issue_threshold"] == 3
        assert d["max_shepherds"] == 3
        assert d["max_proposals"] == 5
        assert d["issue_strategy"] == "fifo"
        assert d["max_retry_count"] == 3
        assert d["retry_cooldown"] == 1800
        assert d["systematic_failure_threshold"] == 3
        assert d["systematic_failure_cooldown"] == 1800
        assert d["systematic_failure_max_probes"] == 3

    def test_from_env_retry_config(self) -> None:
        env = {
            "LOOM_MAX_RETRY_COUNT": "5",
            "LOOM_RETRY_COOLDOWN": "3600",
            "LOOM_RETRY_BACKOFF_MULTIPLIER": "3",
            "LOOM_RETRY_MAX_COOLDOWN": "28800",
            "LOOM_SYSTEMATIC_FAILURE_THRESHOLD": "4",
            "LOOM_SYSTEMATIC_FAILURE_COOLDOWN": "3600",
            "LOOM_SYSTEMATIC_FAILURE_MAX_PROBES": "5",
        }
        with mock.patch.dict("os.environ", env, clear=False):
            cfg = SnapshotConfig.from_env()
        assert cfg.max_retry_count == 5
        assert cfg.retry_cooldown == 3600
        assert cfg.retry_backoff_multiplier == 3
        assert cfg.retry_max_cooldown == 28800
        assert cfg.systematic_failure_threshold == 4
        assert cfg.systematic_failure_cooldown == 3600
        assert cfg.systematic_failure_max_probes == 5


# ---------------------------------------------------------------------------
# Issue sorting
# ---------------------------------------------------------------------------


class TestSortIssues:
    def _make_issues(self) -> list[dict]:
        return [
            {"number": 1, "title": "Old", "createdAt": "2026-01-01T00:00:00Z", "labels": []},
            {"number": 2, "title": "Urgent new", "createdAt": "2026-01-20T00:00:00Z",
             "labels": [{"name": "loom:urgent"}]},
            {"number": 3, "title": "New", "createdAt": "2026-01-15T00:00:00Z", "labels": []},
            {"number": 4, "title": "Urgent old", "createdAt": "2026-01-05T00:00:00Z",
             "labels": [{"name": "loom:urgent"}]},
        ]

    def test_fifo_urgent_first(self) -> None:
        result = sort_issues_by_strategy(self._make_issues(), "fifo")
        numbers = [i["number"] for i in result]
        # Urgent issues first (oldest urgent first), then non-urgent (oldest first)
        assert numbers == [4, 2, 1, 3]

    def test_lifo_urgent_first(self) -> None:
        result = sort_issues_by_strategy(self._make_issues(), "lifo")
        numbers = [i["number"] for i in result]
        # Urgent issues first (newest urgent first), then non-urgent (newest first)
        assert numbers == [2, 4, 3, 1]

    def test_priority_same_as_fifo(self) -> None:
        issues = self._make_issues()
        fifo_result = sort_issues_by_strategy(issues, "fifo")
        priority_result = sort_issues_by_strategy(issues, "priority")
        assert [i["number"] for i in fifo_result] == [i["number"] for i in priority_result]

    def test_unknown_strategy_falls_back_to_fifo(self) -> None:
        issues = self._make_issues()
        fifo_result = sort_issues_by_strategy(issues, "fifo")
        fallback_result = sort_issues_by_strategy(issues, "unknown_strategy")
        assert [i["number"] for i in fifo_result] == [i["number"] for i in fallback_result]

    def test_no_urgent(self) -> None:
        issues = [
            {"number": 1, "createdAt": "2026-01-10T00:00:00Z", "labels": []},
            {"number": 2, "createdAt": "2026-01-05T00:00:00Z", "labels": []},
        ]
        result = sort_issues_by_strategy(issues, "fifo")
        assert [i["number"] for i in result] == [2, 1]

    def test_all_urgent(self) -> None:
        issues = [
            {"number": 1, "createdAt": "2026-01-10T00:00:00Z",
             "labels": [{"name": "loom:urgent"}]},
            {"number": 2, "createdAt": "2026-01-05T00:00:00Z",
             "labels": [{"name": "loom:urgent"}]},
        ]
        result = sort_issues_by_strategy(issues, "fifo")
        assert [i["number"] for i in result] == [2, 1]

    def test_empty_list(self) -> None:
        assert sort_issues_by_strategy([], "fifo") == []


# ---------------------------------------------------------------------------
# Support role idle computation
# ---------------------------------------------------------------------------


class TestSupportRoleState:
    def _make_daemon_state(self, **role_overrides: dict) -> DaemonState:
        roles = {}
        for role_name, data in role_overrides.items():
            roles[role_name] = SupportRoleEntry.from_dict(data)
        return DaemonState(support_roles=roles)

    def test_idle_role_needs_trigger_when_never_run(self) -> None:
        ds = self._make_daemon_state()
        result = compute_support_role_state(ds, _cfg(), _now=NOW)
        # Guide not in support_roles at all => never run => needs trigger
        assert result["guide"].needs_trigger is True

    def test_running_role_never_needs_trigger(self) -> None:
        ds = self._make_daemon_state(guide={"status": "running"})
        result = compute_support_role_state(ds, _cfg(), _now=NOW)
        assert result["guide"].needs_trigger is False

    def test_idle_exceeded_interval(self) -> None:
        # Champion idle for 700s, interval is 600s => needs trigger
        ds = self._make_daemon_state(champion={
            "status": "idle",
            "last_completed": "2026-01-30T17:48:20Z",  # 700s before NOW
        })
        result = compute_support_role_state(ds, _cfg(), _now=NOW)
        assert result["champion"].needs_trigger is True
        assert result["champion"].idle_seconds == 700

    def test_idle_within_interval(self) -> None:
        # Doctor idle for 100s, interval is 300s => no trigger
        ds = self._make_daemon_state(doctor={
            "status": "idle",
            "last_completed": "2026-01-30T17:58:20Z",  # 100s before NOW
        })
        result = compute_support_role_state(ds, _cfg(), _now=NOW)
        assert result["doctor"].needs_trigger is False
        assert result["doctor"].idle_seconds == 100

    def test_architect_uses_cooldown(self) -> None:
        # Architect uses architect_cooldown (1800s), not a shorter interval
        ds = self._make_daemon_state(architect={
            "status": "idle",
            "last_completed": "2026-01-30T17:29:00Z",  # 1860s before NOW
        })
        result = compute_support_role_state(ds, _cfg(), _now=NOW)
        assert result["architect"].interval == 1800
        assert result["architect"].needs_trigger is True

    def test_all_seven_roles_present(self) -> None:
        ds = self._make_daemon_state()
        result = compute_support_role_state(ds, _cfg(), _now=NOW)
        assert set(result.keys()) == {"guide", "champion", "doctor", "auditor", "judge", "architect", "hermit"}


# ---------------------------------------------------------------------------
# Heartbeat staleness
# ---------------------------------------------------------------------------


class TestHeartbeatStaleness:
    def test_fresh_heartbeat(self, tmp_path: pathlib.Path) -> None:
        # Setup progress dir with a fresh progress file
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        # 30 seconds ago — fresh
        (progress_dir / "shepherd-abc1234.json").write_text(json.dumps({
            "task_id": "abc1234",
            "issue": 100,
            "status": "working",
            "last_heartbeat": "2026-01-30T17:59:30Z",
        }))
        cfg = _cfg(heartbeat_stale_threshold=120)
        result = compute_shepherd_progress(tmp_path, cfg, _now=NOW)
        assert len(result) == 1
        assert result[0].heartbeat_stale is False
        assert result[0].heartbeat_age_seconds == 30

    def test_stale_heartbeat(self, tmp_path: pathlib.Path) -> None:
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        # 300 seconds ago — stale (threshold is 120)
        (progress_dir / "shepherd-def5678.json").write_text(json.dumps({
            "task_id": "def5678",
            "issue": 200,
            "status": "working",
            "last_heartbeat": "2026-01-30T17:55:00Z",
        }))
        cfg = _cfg(heartbeat_stale_threshold=120)
        result = compute_shepherd_progress(tmp_path, cfg, _now=NOW)
        assert len(result) == 1
        assert result[0].heartbeat_stale is True
        assert result[0].heartbeat_age_seconds == 300

    def test_missing_heartbeat(self, tmp_path: pathlib.Path) -> None:
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        (progress_dir / "shepherd-aaa0000.json").write_text(json.dumps({
            "task_id": "aaa0000",
            "issue": 300,
            "status": "working",
        }))
        result = compute_shepherd_progress(tmp_path, _cfg(), _now=NOW)
        assert len(result) == 1
        assert result[0].heartbeat_age_seconds == -1
        assert result[0].heartbeat_stale is False

    def test_no_progress_dir(self, tmp_path: pathlib.Path) -> None:
        (tmp_path / ".loom").mkdir(parents=True)
        result = compute_shepherd_progress(tmp_path, _cfg(), _now=NOW)
        assert result == []


# ---------------------------------------------------------------------------
# Orphaned shepherd detection
# ---------------------------------------------------------------------------


class TestOrphanedShepherdDetection:
    def test_untracked_building_issue(self) -> None:
        ds = DaemonState(shepherds={
            "shepherd-1": ShepherdEntry(status="working", issue=100),
        })
        building = [
            {"number": 100},  # tracked
            {"number": 200},  # NOT tracked
        ]
        progress: list[EnhancedProgress] = []
        result = detect_orphaned_shepherds(ds, building, progress)
        assert len(result) == 1
        assert result[0]["type"] == "untracked_building"
        assert result[0]["issue"] == 200

    def test_no_orphans_when_all_tracked(self) -> None:
        ds = DaemonState(shepherds={
            "shepherd-1": ShepherdEntry(status="working", issue=100),
            "shepherd-2": ShepherdEntry(status="working", issue=200),
        })
        building = [{"number": 100}, {"number": 200}]
        result = detect_orphaned_shepherds(ds, building, [])
        assert result == []

    def test_stale_heartbeat_orphan(self) -> None:
        ds = DaemonState()
        progress = [EnhancedProgress(
            raw={"task_id": "abc1234", "issue": 300, "status": "working"},
            heartbeat_age_seconds=500,
            heartbeat_stale=True,
        )]
        result = detect_orphaned_shepherds(ds, [], progress)
        assert len(result) == 1
        assert result[0]["type"] == "stale_heartbeat"
        assert result[0]["task_id"] == "abc1234"

    def test_active_progress_prevents_orphan(self) -> None:
        """Building issue has an active (non-stale) progress file — not orphaned."""
        ds = DaemonState()
        building = [{"number": 100}]
        progress = [EnhancedProgress(
            raw={"task_id": "xxx0000", "issue": 100, "status": "working"},
            heartbeat_age_seconds=30,
            heartbeat_stale=False,
        )]
        result = detect_orphaned_shepherds(ds, building, progress)
        assert result == []

    def test_empty_state(self) -> None:
        result = detect_orphaned_shepherds(DaemonState(), [], [])
        assert result == []


# ---------------------------------------------------------------------------
# Task ID validation
# ---------------------------------------------------------------------------


class TestTaskIdValidation:
    def test_valid_hex(self) -> None:
        ds = DaemonState(shepherds={
            "s-1": ShepherdEntry(task_id="abc1234"),
        })
        assert validate_task_ids(ds) == []

    def test_invalid_too_short(self) -> None:
        ds = DaemonState(shepherds={
            "s-1": ShepherdEntry(task_id="abc"),
        })
        result = validate_task_ids(ds)
        assert len(result) == 1
        assert result[0]["task_id"] == "abc"
        assert result[0]["location"] == "shepherds"

    def test_invalid_non_hex(self) -> None:
        ds = DaemonState(shepherds={
            "s-1": ShepherdEntry(task_id="xyz1234"),
        })
        result = validate_task_ids(ds)
        assert len(result) == 1

    def test_support_role_task_ids(self) -> None:
        ds = DaemonState(support_roles={
            "guide": SupportRoleEntry(task_id="22"),  # invalid
            "champion": SupportRoleEntry(task_id="abcdef0"),  # valid
        })
        result = validate_task_ids(ds)
        assert len(result) == 1
        assert result[0]["key"] == "guide"
        assert result[0]["location"] == "support_roles"

    def test_null_task_id_ignored(self) -> None:
        ds = DaemonState(shepherds={
            "s-1": ShepherdEntry(task_id=None),
        })
        assert validate_task_ids(ds) == []

    def test_empty_task_id_ignored(self) -> None:
        ds = DaemonState(shepherds={
            "s-1": ShepherdEntry(task_id=""),
        })
        assert validate_task_ids(ds) == []


# ---------------------------------------------------------------------------
# Recommended actions engine
# ---------------------------------------------------------------------------


class TestRecommendedActions:
    def _base_kwargs(self) -> dict:
        return {
            "ready_count": 0,
            "building_count": 0,
            "blocked_count": 0,
            "total_proposals": 0,
            "architect_count": 0,
            "hermit_count": 0,
            "review_count": 0,
            "changes_count": 0,
            "merge_count": 0,
            "available_shepherd_slots": 3,
            "needs_work_generation": False,
            "architect_cooldown_ok": True,
            "hermit_cooldown_ok": True,
            "support_roles": {r: SupportRoleState() for r in
                              ("guide", "champion", "doctor", "auditor", "judge", "architect", "hermit")},
            "orphaned_count": 0,
            "invalid_task_id_count": 0,
            "systematic_failure_active": False,
            "systematic_failure_state": None,
            "pipeline_health": None,
        }

    def test_empty_pipeline_returns_wait(self) -> None:
        actions, demand = compute_recommended_actions(**self._base_kwargs())
        assert "wait" in actions
        assert demand["champion_demand"] is False

    def test_spawn_shepherds(self) -> None:
        kw = self._base_kwargs()
        kw["ready_count"] = 2
        actions, _ = compute_recommended_actions(**kw)
        assert "spawn_shepherds" in actions
        assert "wait" not in actions

    def test_promote_proposals(self) -> None:
        kw = self._base_kwargs()
        kw["total_proposals"] = 3
        actions, _ = compute_recommended_actions(**kw)
        assert "promote_proposals" in actions

    def test_trigger_architect(self) -> None:
        kw = self._base_kwargs()
        kw["needs_work_generation"] = True
        kw["architect_cooldown_ok"] = True
        kw["architect_count"] = 0
        actions, _ = compute_recommended_actions(**kw)
        assert "trigger_architect" in actions

    def test_no_trigger_architect_when_running(self) -> None:
        kw = self._base_kwargs()
        kw["needs_work_generation"] = True
        kw["support_roles"]["architect"] = SupportRoleState(status="running")
        actions, _ = compute_recommended_actions(**kw)
        assert "trigger_architect" not in actions

    def test_no_trigger_architect_at_max_proposals(self) -> None:
        kw = self._base_kwargs()
        kw["needs_work_generation"] = True
        kw["architect_count"] = 2
        actions, _ = compute_recommended_actions(**kw)
        assert "trigger_architect" not in actions

    def test_trigger_hermit(self) -> None:
        kw = self._base_kwargs()
        kw["needs_work_generation"] = True
        kw["hermit_cooldown_ok"] = True
        kw["hermit_count"] = 0
        actions, _ = compute_recommended_actions(**kw)
        assert "trigger_hermit" in actions

    def test_check_stuck(self) -> None:
        kw = self._base_kwargs()
        kw["building_count"] = 2
        actions, _ = compute_recommended_actions(**kw)
        assert "check_stuck" in actions

    def test_check_stuck_alone_adds_wait(self) -> None:
        kw = self._base_kwargs()
        kw["building_count"] = 1
        actions, _ = compute_recommended_actions(**kw)
        assert "check_stuck" in actions
        assert "wait" in actions

    def test_demand_champion(self) -> None:
        kw = self._base_kwargs()
        kw["merge_count"] = 1
        actions, demand = compute_recommended_actions(**kw)
        assert "spawn_champion_demand" in actions
        assert demand["champion_demand"] is True

    def test_demand_doctor(self) -> None:
        kw = self._base_kwargs()
        kw["changes_count"] = 1
        actions, demand = compute_recommended_actions(**kw)
        assert "spawn_doctor_demand" in actions
        assert demand["doctor_demand"] is True

    def test_demand_judge(self) -> None:
        kw = self._base_kwargs()
        kw["review_count"] = 1
        actions, demand = compute_recommended_actions(**kw)
        assert "spawn_judge_demand" in actions
        assert demand["judge_demand"] is True

    def test_demand_skips_interval_trigger(self) -> None:
        """When demand-based trigger fires, interval trigger should not."""
        kw = self._base_kwargs()
        kw["merge_count"] = 1
        kw["support_roles"]["champion"] = SupportRoleState(needs_trigger=True)
        actions, demand = compute_recommended_actions(**kw)
        assert "spawn_champion_demand" in actions
        assert "trigger_champion" not in actions
        assert demand["champion_demand"] is True

    def test_interval_trigger_when_no_demand(self) -> None:
        kw = self._base_kwargs()
        kw["support_roles"]["guide"] = SupportRoleState(needs_trigger=True)
        actions, _ = compute_recommended_actions(**kw)
        assert "trigger_guide" in actions

    def test_recover_orphans(self) -> None:
        kw = self._base_kwargs()
        kw["orphaned_count"] = 2
        actions, _ = compute_recommended_actions(**kw)
        assert "recover_orphans" in actions

    def test_validate_state(self) -> None:
        kw = self._base_kwargs()
        kw["invalid_task_id_count"] = 1
        actions, _ = compute_recommended_actions(**kw)
        assert "validate_state" in actions

    def test_full_pipeline(self) -> None:
        """Many actions at once — no wait."""
        kw = self._base_kwargs()
        kw["ready_count"] = 2
        kw["total_proposals"] = 1
        kw["building_count"] = 1
        kw["needs_work_generation"] = True
        actions, _ = compute_recommended_actions(**kw)
        assert "wait" not in actions
        assert "spawn_shepherds" in actions
        assert "promote_proposals" in actions
        assert "check_stuck" in actions

    def test_systematic_failure_suppresses_spawn(self) -> None:
        kw = self._base_kwargs()
        kw["ready_count"] = 2
        kw["systematic_failure_active"] = True
        actions, _ = compute_recommended_actions(**kw)
        assert "spawn_shepherds" not in actions

    def test_systematic_failure_probe_after_cooldown(self) -> None:
        """When systematic failure cooldown has elapsed, recommend probe."""
        kw = self._base_kwargs()
        kw["ready_count"] = 2
        kw["systematic_failure_active"] = True
        kw["systematic_failure_state"] = SystematicFailureState(
            active=True,
            pattern="api_error",
            probe_count=0,
            cooldown_elapsed=True,
            cooldown_remaining_seconds=0,
            probes_exhausted=False,
        )
        actions, _ = compute_recommended_actions(**kw)
        assert "probe_systematic_failure" in actions
        # When probing, spawning should be allowed
        assert "spawn_shepherds" in actions

    def test_systematic_failure_probes_exhausted(self) -> None:
        """When probes exhausted, require manual intervention."""
        kw = self._base_kwargs()
        kw["ready_count"] = 2
        kw["systematic_failure_active"] = True
        kw["systematic_failure_state"] = SystematicFailureState(
            active=True,
            pattern="api_error",
            probe_count=3,
            cooldown_elapsed=True,
            cooldown_remaining_seconds=0,
            probes_exhausted=True,
        )
        actions, _ = compute_recommended_actions(**kw)
        assert "systematic_failure_manual_intervention" in actions
        assert "spawn_shepherds" not in actions
        assert "probe_systematic_failure" not in actions

    def test_systematic_failure_within_cooldown(self) -> None:
        """When within cooldown, keep suppressing spawn."""
        kw = self._base_kwargs()
        kw["ready_count"] = 2
        kw["systematic_failure_active"] = True
        kw["systematic_failure_state"] = SystematicFailureState(
            active=True,
            pattern="api_error",
            probe_count=0,
            cooldown_elapsed=False,
            cooldown_remaining_seconds=600,
            probes_exhausted=False,
        )
        actions, _ = compute_recommended_actions(**kw)
        assert "spawn_shepherds" not in actions
        assert "probe_systematic_failure" not in actions

    def test_retry_blocked_issues_when_stalled(self) -> None:
        kw = self._base_kwargs()
        kw["blocked_count"] = 3
        kw["pipeline_health"] = PipelineHealth(
            status="stalled",
            stall_reason="all_issues_blocked",
            blocked_count=3,
            retryable_count=2,
        )
        actions, _ = compute_recommended_actions(**kw)
        assert "retry_blocked_issues" in actions

    def test_no_retry_when_healthy(self) -> None:
        kw = self._base_kwargs()
        kw["blocked_count"] = 1
        kw["ready_count"] = 2
        kw["pipeline_health"] = PipelineHealth(status="healthy", retryable_count=1)
        actions, _ = compute_recommended_actions(**kw)
        assert "retry_blocked_issues" not in actions


# ---------------------------------------------------------------------------
# Systematic failure state computation
# ---------------------------------------------------------------------------


class TestSystematicFailureState:
    def test_inactive_systematic_failure(self) -> None:
        """No systematic failure -> empty state."""
        ds = DaemonState()
        result = compute_systematic_failure_state(ds, _cfg(), _now=NOW)
        assert result.active is False
        assert result.cooldown_elapsed is False

    def test_cooldown_elapsed_with_cooldown_until(self) -> None:
        """Cooldown has passed using cooldown_until timestamp."""
        ds = DaemonState(systematic_failure=SystematicFailure(
            active=True,
            pattern="api_error",
            count=3,
            detected_at="2026-01-30T17:00:00Z",
            cooldown_until="2026-01-30T17:30:00Z",  # 30 min before NOW (18:00)
            probe_count=0,
        ))
        result = compute_systematic_failure_state(ds, _cfg(), _now=NOW)
        assert result.active is True
        assert result.cooldown_elapsed is True
        assert result.cooldown_remaining_seconds == 0
        assert result.probes_exhausted is False

    def test_cooldown_not_elapsed(self) -> None:
        """Cooldown has not passed yet."""
        ds = DaemonState(systematic_failure=SystematicFailure(
            active=True,
            pattern="api_error",
            count=3,
            detected_at="2026-01-30T17:50:00Z",
            cooldown_until="2026-01-30T18:20:00Z",  # 20 min after NOW
            probe_count=0,
        ))
        result = compute_systematic_failure_state(ds, _cfg(), _now=NOW)
        assert result.active is True
        assert result.cooldown_elapsed is False
        assert result.cooldown_remaining_seconds == 1200  # 20 minutes

    def test_probes_exhausted(self) -> None:
        """Max probes reached -> probes_exhausted is True."""
        cfg = _cfg(systematic_failure_max_probes=3)
        ds = DaemonState(systematic_failure=SystematicFailure(
            active=True,
            pattern="api_error",
            count=3,
            detected_at="2026-01-30T17:00:00Z",
            cooldown_until="2026-01-30T17:30:00Z",
            probe_count=3,
        ))
        result = compute_systematic_failure_state(ds, cfg, _now=NOW)
        assert result.probes_exhausted is True

    def test_fallback_to_detected_at_when_no_cooldown_until(self) -> None:
        """If no cooldown_until, use detected_at + cooldown."""
        cfg = _cfg(systematic_failure_cooldown=1800)  # 30 min
        ds = DaemonState(systematic_failure=SystematicFailure(
            active=True,
            pattern="api_error",
            count=3,
            detected_at="2026-01-30T17:00:00Z",  # 60 min before NOW
            cooldown_until=None,
            probe_count=0,
        ))
        result = compute_systematic_failure_state(ds, cfg, _now=NOW)
        assert result.cooldown_elapsed is True  # 60 min > 30 min cooldown

    def test_exponential_backoff(self) -> None:
        """Cooldown should double with each probe attempt."""
        cfg = _cfg(systematic_failure_cooldown=1800)  # 30 min base
        # After 1 probe: cooldown = 1800 * 2^1 = 3600s (60 min)
        # detected_at at 17:00, NOW at 18:00 -> 60 min elapsed
        # With probe_count=1, effective_cooldown=3600, should be exactly at cooldown
        ds = DaemonState(systematic_failure=SystematicFailure(
            active=True,
            pattern="api_error",
            count=3,
            detected_at="2026-01-30T17:00:00Z",  # 60 min before NOW
            cooldown_until=None,
            probe_count=1,
        ))
        result = compute_systematic_failure_state(ds, cfg, _now=NOW)
        # Elapsed = 3600, cooldown = 3600 -> cooldown_elapsed should be True
        assert result.cooldown_elapsed is True


# ---------------------------------------------------------------------------
# Pipeline health computation
# ---------------------------------------------------------------------------


class TestPipelineHealth:
    def test_healthy_pipeline(self) -> None:
        result = compute_pipeline_health(
            ready_count=3, building_count=1, blocked_count=0, total_in_flight=2,
            blocked_issues=[], daemon_state=DaemonState(), cfg=_cfg(), now=NOW,
        )
        assert result.status == "healthy"
        assert result.stall_reason is None

    def test_stalled_all_blocked(self) -> None:
        blocked = [{"number": 1}, {"number": 2}]
        result = compute_pipeline_health(
            ready_count=0, building_count=0, blocked_count=2, total_in_flight=0,
            blocked_issues=blocked, daemon_state=DaemonState(), cfg=_cfg(), now=NOW,
        )
        assert result.status == "stalled"
        assert result.stall_reason == "all_issues_blocked"
        assert result.retryable_count == 2

    def test_stalled_no_ready_issues(self) -> None:
        result = compute_pipeline_health(
            ready_count=0, building_count=0, blocked_count=0, total_in_flight=0,
            blocked_issues=[], daemon_state=DaemonState(), cfg=_cfg(), now=NOW,
        )
        assert result.status == "stalled"
        assert result.stall_reason == "no_ready_issues"

    def test_degraded_more_blocked_than_ready(self) -> None:
        blocked = [{"number": 1}, {"number": 2}, {"number": 3}]
        result = compute_pipeline_health(
            ready_count=1, building_count=0, blocked_count=3, total_in_flight=0,
            blocked_issues=blocked, daemon_state=DaemonState(), cfg=_cfg(), now=NOW,
        )
        assert result.status == "degraded"

    def test_retry_exhausted_is_permanent(self) -> None:
        ds = DaemonState(blocked_issue_retries={
            "100": BlockedIssueRetry(retry_count=3, retry_exhausted=True),
        })
        blocked = [{"number": 100}]
        result = compute_pipeline_health(
            ready_count=0, building_count=0, blocked_count=1, total_in_flight=0,
            blocked_issues=blocked, daemon_state=ds, cfg=_cfg(), now=NOW,
        )
        assert result.retryable_count == 0
        assert result.permanent_blocked_count == 1

    def test_cooldown_not_elapsed_is_permanent(self) -> None:
        """Issue retried 60s ago with 1800s cooldown — still in cooldown."""
        ds = DaemonState(blocked_issue_retries={
            "100": BlockedIssueRetry(
                retry_count=1,
                last_retry_at="2026-01-30T17:59:00Z",  # 60s before NOW
            ),
        })
        blocked = [{"number": 100}]
        result = compute_pipeline_health(
            ready_count=0, building_count=0, blocked_count=1, total_in_flight=0,
            blocked_issues=blocked, daemon_state=ds, cfg=_cfg(), now=NOW,
        )
        assert result.retryable_count == 0
        assert result.permanent_blocked_count == 1

    def test_cooldown_elapsed_is_retryable(self) -> None:
        """Issue retried 3601s ago, retry_count=1, effective cooldown = 1800*2 = 3600."""
        ds = DaemonState(blocked_issue_retries={
            "100": BlockedIssueRetry(
                retry_count=1,
                last_retry_at="2026-01-30T16:59:59Z",  # 3601s before NOW
            ),
        })
        blocked = [{"number": 100}]
        result = compute_pipeline_health(
            ready_count=0, building_count=0, blocked_count=1, total_in_flight=0,
            blocked_issues=blocked, daemon_state=ds, cfg=_cfg(), now=NOW,
        )
        assert result.retryable_count == 1
        assert result.retryable_issues[0]["number"] == 100
        assert result.retryable_issues[0]["retry_count"] == 1

    def test_backoff_multiplier(self) -> None:
        """After 2 retries, cooldown is 1800 * 2^2 = 7200s.

        With 7199s elapsed, still within cooldown — not retryable.
        """
        ds = DaemonState(blocked_issue_retries={
            "100": BlockedIssueRetry(
                retry_count=2,
                last_retry_at="2026-01-30T16:00:01Z",  # 7199s before NOW
            ),
        })
        blocked = [{"number": 100}]
        result = compute_pipeline_health(
            ready_count=0, building_count=0, blocked_count=1, total_in_flight=0,
            blocked_issues=blocked, daemon_state=ds, cfg=_cfg(), now=NOW,
        )
        assert result.retryable_count == 0  # within cooldown, not retryable

    def test_max_cooldown_cap(self) -> None:
        """Cooldown is capped at retry_max_cooldown (14400s = 4 hours)."""
        ds = DaemonState(blocked_issue_retries={
            "100": BlockedIssueRetry(
                retry_count=2,
                last_retry_at="2026-01-30T13:59:59Z",  # 14401s before NOW
            ),
        })
        blocked = [{"number": 100}]
        # effective_cooldown = 1800 * 2^2 = 7200, capped at 14400
        # elapsed = 14401 > 7200, so retryable
        result = compute_pipeline_health(
            ready_count=0, building_count=0, blocked_count=1, total_in_flight=0,
            blocked_issues=blocked, daemon_state=ds, cfg=_cfg(), now=NOW,
        )
        assert result.retryable_count == 1


# ---------------------------------------------------------------------------
# Health warnings
# ---------------------------------------------------------------------------


class TestHealth:
    def test_healthy(self) -> None:
        status, warnings = compute_health(
            ready_count=1, building_count=0, blocked_count=0,
            total_proposals=0, stale_heartbeat_count=0, orphaned_count=0,
            usage_healthy=True, session_percent=50.0,
        )
        assert status == "healthy"
        assert warnings == []

    def test_pipeline_stalled(self) -> None:
        status, warnings = compute_health(
            ready_count=0, building_count=0, blocked_count=3,
            total_proposals=0, stale_heartbeat_count=0, orphaned_count=0,
            usage_healthy=True, session_percent=50.0,
        )
        assert status == "stalled"
        assert any(w["code"] == "pipeline_stalled" for w in warnings)

    def test_proposal_backlog(self) -> None:
        status, warnings = compute_health(
            ready_count=0, building_count=0, blocked_count=0,
            total_proposals=3, stale_heartbeat_count=0, orphaned_count=0,
            usage_healthy=True, session_percent=50.0,
        )
        assert status == "degraded"
        assert any(w["code"] == "proposal_backlog" for w in warnings)

    def test_no_work_available(self) -> None:
        status, warnings = compute_health(
            ready_count=0, building_count=0, blocked_count=0,
            total_proposals=0, stale_heartbeat_count=0, orphaned_count=0,
            usage_healthy=True, session_percent=50.0,
        )
        assert status == "degraded"
        assert any(w["code"] == "no_work_available" for w in warnings)

    def test_stale_heartbeats(self) -> None:
        status, warnings = compute_health(
            ready_count=1, building_count=1, blocked_count=0,
            total_proposals=0, stale_heartbeat_count=2, orphaned_count=0,
            usage_healthy=True, session_percent=50.0,
        )
        assert status == "stalled"
        assert any(w["code"] == "stale_heartbeats" for w in warnings)

    def test_orphaned_issues(self) -> None:
        status, warnings = compute_health(
            ready_count=1, building_count=0, blocked_count=0,
            total_proposals=0, stale_heartbeat_count=0, orphaned_count=1,
            usage_healthy=True, session_percent=50.0,
        )
        assert status == "stalled"
        assert any(w["code"] == "orphaned_issues" for w in warnings)

    def test_session_budget_low(self) -> None:
        status, warnings = compute_health(
            ready_count=1, building_count=0, blocked_count=0,
            total_proposals=0, stale_heartbeat_count=0, orphaned_count=0,
            usage_healthy=False, session_percent=98.0,
        )
        assert status == "stalled"
        assert any(w["code"] == "session_budget_low" for w in warnings)

    def test_degraded_only_info(self) -> None:
        """Info-only warnings = degraded, not stalled."""
        status, warnings = compute_health(
            ready_count=0, building_count=0, blocked_count=0,
            total_proposals=2, stale_heartbeat_count=0, orphaned_count=0,
            usage_healthy=True, session_percent=50.0,
        )
        assert status == "degraded"
        assert all(w["level"] == "info" for w in warnings)


# ---------------------------------------------------------------------------
# Tmux pool detection
# ---------------------------------------------------------------------------


class TestTmuxPool:
    def test_tmux_not_available(self) -> None:
        with mock.patch("loom_tools.snapshot.subprocess.run", side_effect=FileNotFoundError):
            result = detect_tmux_pool("loom")
        assert result.available is False
        assert result.execution_mode == "direct"

    def test_tmux_has_sessions(self) -> None:
        mock_has = mock.MagicMock(returncode=0)
        mock_list = mock.MagicMock(
            returncode=0,
            stdout="loom-shepherd-1\nloom-shepherd-2\nloom-worker-1\n",
        )

        def side_effect(args, **kw):
            if "has-session" in args:
                return mock_has
            return mock_list

        with mock.patch("loom_tools.snapshot.subprocess.run", side_effect=side_effect):
            result = detect_tmux_pool("loom")
        assert result.available is True
        assert result.shepherd_count == 2
        assert result.total_count == 3
        assert result.execution_mode == "tmux"
        assert "loom-shepherd-1" in result.sessions

    def test_tmux_no_shepherds(self) -> None:
        mock_has = mock.MagicMock(returncode=0)
        mock_list = mock.MagicMock(returncode=0, stdout="loom-worker-1\n")

        def side_effect(args, **kw):
            if "has-session" in args:
                return mock_has
            return mock_list

        with mock.patch("loom_tools.snapshot.subprocess.run", side_effect=side_effect):
            result = detect_tmux_pool("loom")
        assert result.available is True
        assert result.shepherd_count == 0
        assert result.execution_mode == "direct"


# ---------------------------------------------------------------------------
# build_snapshot integration
# ---------------------------------------------------------------------------


class TestBuildSnapshot:
    def _mock_pipeline(self) -> dict:
        return {
            "ready_issues": [
                {"number": 1, "title": "Issue 1", "labels": [], "createdAt": "2026-01-01T00:00:00Z"},
                {"number": 2, "title": "Issue 2", "labels": [], "createdAt": "2026-01-02T00:00:00Z"},
            ],
            "building_issues": [{"number": 3, "title": "Building", "labels": []}],
            "blocked_issues": [],
            "architect_proposals": [{"number": 10, "title": "Proposal", "labels": []}],
            "hermit_proposals": [],
            "curated_issues": [],
            "review_requested": [{"number": 20, "title": "PR 20", "labels": [], "headRefName": "feature/20"}],
            "changes_requested": [],
            "ready_to_merge": [{"number": 30, "title": "PR 30", "labels": [], "headRefName": "feature/30"}],
            "usage": {"session_percent": 50, "total_cost": 10.0},
        }

    def test_snapshot_has_all_top_level_sections(self, tmp_path: pathlib.Path) -> None:
        # Create minimal .loom directory
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("{}")

        snapshot = build_snapshot(
            cfg=_cfg(),
            repo_root=tmp_path,
            _now=NOW,
            _pipeline_data=self._mock_pipeline(),
            _tmux_pool=TmuxPool(),
        )
        expected_sections = {
            "timestamp", "pipeline", "proposals", "prs",
            "shepherds", "validation", "support_roles",
            "pipeline_health", "systematic_failure",
            "usage", "tmux_pool", "computed", "config",
        }
        assert set(snapshot.keys()) == expected_sections

    def test_snapshot_computed_fields(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("{}")

        snapshot = build_snapshot(
            cfg=_cfg(),
            repo_root=tmp_path,
            _now=NOW,
            _pipeline_data=self._mock_pipeline(),
            _tmux_pool=TmuxPool(),
        )
        computed = snapshot["computed"]
        assert computed["total_ready"] == 2
        assert computed["total_building"] == 1
        assert computed["total_blocked"] == 0
        assert computed["total_proposals"] == 1  # 1 architect
        assert computed["needs_work_generation"] is True  # 2 < 3 threshold
        assert isinstance(computed["recommended_actions"], list)
        assert computed["health_status"] in ("healthy", "degraded", "stalled")

    def test_snapshot_pipeline_sorted(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("{}")

        snapshot = build_snapshot(
            cfg=_cfg(),
            repo_root=tmp_path,
            _now=NOW,
            _pipeline_data=self._mock_pipeline(),
            _tmux_pool=TmuxPool(),
        )
        issues = snapshot["pipeline"]["ready_issues"]
        # FIFO: oldest first
        assert issues[0]["number"] == 1
        assert issues[1]["number"] == 2

    def test_snapshot_support_roles_complete(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("{}")

        snapshot = build_snapshot(
            cfg=_cfg(),
            repo_root=tmp_path,
            _now=NOW,
            _pipeline_data=self._mock_pipeline(),
            _tmux_pool=TmuxPool(),
        )
        sr = snapshot["support_roles"]
        assert set(sr.keys()) == {"guide", "champion", "doctor", "auditor", "judge", "architect", "hermit"}
        # Demand roles have demand_trigger field
        assert "demand_trigger" in sr["champion"]
        assert "demand_trigger" in sr["doctor"]
        assert "demand_trigger" in sr["judge"]
        # Non-demand roles don't
        assert "demand_trigger" not in sr["guide"]

    def test_snapshot_usage_has_healthy(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("{}")

        snapshot = build_snapshot(
            cfg=_cfg(),
            repo_root=tmp_path,
            _now=NOW,
            _pipeline_data=self._mock_pipeline(),
            _tmux_pool=TmuxPool(),
        )
        assert "healthy" in snapshot["usage"]
        assert snapshot["usage"]["healthy"] is True

    def test_snapshot_promotable_proposals(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("{}")

        snapshot = build_snapshot(
            cfg=_cfg(),
            repo_root=tmp_path,
            _now=NOW,
            _pipeline_data=self._mock_pipeline(),
            _tmux_pool=TmuxPool(),
        )
        assert 10 in snapshot["computed"]["promotable_proposals"]

    def test_snapshot_json_serializable(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text("{}")

        snapshot = build_snapshot(
            cfg=_cfg(),
            repo_root=tmp_path,
            _now=NOW,
            _pipeline_data=self._mock_pipeline(),
            _tmux_pool=TmuxPool(),
        )
        # Must be JSON-serializable without errors
        output = json.dumps(snapshot)
        parsed = json.loads(output)
        assert parsed["timestamp"] == "2026-01-30T18:00:00Z"


# ---------------------------------------------------------------------------
# CLI interface
# ---------------------------------------------------------------------------


class TestCLI:
    def test_help_flag(self, capsys: pytest.CaptureFixture[str]) -> None:
        with pytest.raises(SystemExit) as exc:
            main(["--help"])
        assert exc.value.code == 0
        captured = capsys.readouterr()
        assert "daemon-snapshot.py" in captured.out
        assert "ENVIRONMENT VARIABLES" in captured.out

    def test_unknown_option(self, capsys: pytest.CaptureFixture[str]) -> None:
        with pytest.raises(SystemExit) as exc:
            main(["--bogus"])
        assert exc.value.code == 1
        captured = capsys.readouterr()
        assert "Unknown option" in captured.err

    def test_h_short_flag(self, capsys: pytest.CaptureFixture[str]) -> None:
        """Short -h flag should also display help and exit 0."""
        with pytest.raises(SystemExit) as exc:
            main(["-h"])
        assert exc.value.code == 0
        captured = capsys.readouterr()
        assert "daemon-snapshot.py" in captured.out


class TestEnvironmentVariables:
    """Tests for environment variable configuration."""

    def test_all_loom_env_vars_supported(self) -> None:
        """All documented LOOM_* environment variables should be read."""
        env = {
            "LOOM_ISSUE_THRESHOLD": "5",
            "LOOM_MAX_SHEPHERDS": "6",
            "LOOM_MAX_PROPOSALS": "10",
            "LOOM_ARCHITECT_COOLDOWN": "3600",
            "LOOM_HERMIT_COOLDOWN": "3600",
            "LOOM_GUIDE_INTERVAL": "1800",
            "LOOM_CHAMPION_INTERVAL": "1200",
            "LOOM_DOCTOR_INTERVAL": "600",
            "LOOM_AUDITOR_INTERVAL": "1200",
            "LOOM_JUDGE_INTERVAL": "600",
            "LOOM_ISSUE_STRATEGY": "lifo",
            "LOOM_HEARTBEAT_STALE_THRESHOLD": "240",
            "LOOM_TMUX_SOCKET": "custom",
            "LOOM_MAX_RETRY_COUNT": "5",
            "LOOM_RETRY_COOLDOWN": "3600",
            "LOOM_RETRY_BACKOFF_MULTIPLIER": "3",
            "LOOM_RETRY_MAX_COOLDOWN": "28800",
            "LOOM_SYSTEMATIC_FAILURE_THRESHOLD": "4",
            "LOOM_SYSTEMATIC_FAILURE_COOLDOWN": "3600",
            "LOOM_SYSTEMATIC_FAILURE_MAX_PROBES": "5",
        }
        with mock.patch.dict("os.environ", env, clear=False):
            cfg = SnapshotConfig.from_env()

        assert cfg.issue_threshold == 5
        assert cfg.max_shepherds == 6
        assert cfg.max_proposals == 10
        assert cfg.architect_cooldown == 3600
        assert cfg.hermit_cooldown == 3600
        assert cfg.guide_interval == 1800
        assert cfg.champion_interval == 1200
        assert cfg.doctor_interval == 600
        assert cfg.auditor_interval == 1200
        assert cfg.judge_interval == 600
        assert cfg.issue_strategy == "lifo"
        assert cfg.heartbeat_stale_threshold == 240
        assert cfg.tmux_socket == "custom"
        assert cfg.max_retry_count == 5
        assert cfg.retry_cooldown == 3600
        assert cfg.retry_backoff_multiplier == 3
        assert cfg.retry_max_cooldown == 28800
        assert cfg.systematic_failure_threshold == 4
        assert cfg.systematic_failure_cooldown == 3600
        assert cfg.systematic_failure_max_probes == 5
