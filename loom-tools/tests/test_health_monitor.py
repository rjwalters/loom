"""Tests for loom_tools.health_monitor - forge-derived spawn-loop health.

After the Phase 3.1.8 port (#3397), the health monitor is a point-in-time
composite score over forge queries + spawn-loop-state.json. The legacy tests
for the time-series / alert-persistence flows were trimmed; the recipe and
alert generation are still well-covered here.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

import pytest

from loom_tools.health_monitor import (
    Alert,
    HealthSnapshot,
    calculate_health_score,
    collect_snapshot,
    format_alerts_human,
    format_alerts_json,
    format_health_human,
    format_health_json,
    generate_alerts,
    get_health_status,
    main,
)
from loom_tools.models.spawn_loop_state import SpawnLoopState, SpawnLoopTask

NOW = datetime(2026, 6, 2, 12, 0, 0, tzinfo=timezone.utc)
TS = "2026-06-02T12:00:00Z"


def _snap(**kwargs) -> HealthSnapshot:
    return HealthSnapshot(timestamp=TS, **kwargs)


# ---------------------------------------------------------------------------
# Score recipe
# ---------------------------------------------------------------------------


class TestCalculateHealthScore:
    def test_perfect_score(self):
        assert calculate_health_score(_snap()) == 100

    def test_one_stuck_task(self):
        # -10 for 1 stuck task. No other deductions.
        assert calculate_health_score(_snap(stuck_tasks=1, running_tasks=1)) == 90

    def test_two_stuck_tasks(self):
        assert calculate_health_score(_snap(stuck_tasks=2, running_tasks=2)) == 85

    def test_three_stuck_tasks(self):
        assert calculate_health_score(_snap(stuck_tasks=3, running_tasks=3)) == 80

    def test_orphan_building_one(self):
        assert calculate_health_score(_snap(orphan_building=1)) == 95

    def test_orphan_building_three(self):
        assert calculate_health_score(_snap(orphan_building=3)) == 85

    def test_ready_queue_high(self):
        # ready >= 20 -> -15, running_tasks > 0 so no extra penalty
        assert calculate_health_score(_snap(ready_count=20, running_tasks=1)) == 85

    def test_ready_queue_medium(self):
        # ready >= 10 -> -10
        assert calculate_health_score(_snap(ready_count=10, running_tasks=1)) == 90

    def test_ready_queue_low_with_no_workers(self):
        # ready >= 1 but running_tasks == 0 -> -5 (pipeline stall warning)
        # Also triggers pipeline_stall alert but that doesn't affect score.
        assert calculate_health_score(_snap(ready_count=1, running_tasks=0)) == 95

    def test_ready_queue_low_with_workers(self):
        # ready >= 1 but running_tasks > 0 -> no penalty (normal queue)
        assert calculate_health_score(_snap(ready_count=2, running_tasks=2)) == 100

    def test_review_backlog_high(self):
        assert calculate_health_score(_snap(review_requested_count=10)) == 85

    def test_review_backlog_medium(self):
        assert calculate_health_score(_snap(review_requested_count=5)) == 90

    def test_review_backlog_low(self):
        # >=1 deducts 3 (mild)
        assert calculate_health_score(_snap(review_requested_count=1)) == 97

    def test_review_backlog_combined(self):
        # review + changes summed
        assert (
            calculate_health_score(
                _snap(review_requested_count=3, changes_requested_count=2)
            )
            == 90  # 5 total -> medium
        )

    def test_merge_conflict_high(self):
        assert calculate_health_score(_snap(merge_conflict_count=5)) == 85

    def test_merge_conflict_medium(self):
        assert calculate_health_score(_snap(merge_conflict_count=3)) == 90

    def test_merge_conflict_low(self):
        assert calculate_health_score(_snap(merge_conflict_count=1)) == 95

    def test_combined_factors(self):
        # 3 stuck (-20) + 2 orphan (-10) + ready 20 (-15, suppresses pipeline-stall path)
        # + 5 review (-10) + 3 merge-conflict (-10) = -65, score = 35
        score = calculate_health_score(
            _snap(
                stuck_tasks=3,
                running_tasks=3,
                orphan_building=2,
                ready_count=20,
                review_requested_count=5,
                merge_conflict_count=3,
            )
        )
        assert score == 35

    def test_floor_at_zero(self):
        # Max deduction = 80 -> score 20. Floor at 0 should be safe.
        score = calculate_health_score(
            _snap(
                stuck_tasks=3,  # -20
                orphan_building=3,  # -15
                ready_count=20,  # -15
                review_requested_count=10,  # -15
                merge_conflict_count=5,  # -15
            )
        )
        assert score == 20

    def test_score_never_negative(self):
        # Synthesize a deduction beyond what real factors can produce by
        # confirming the floor clamps.
        # max() in calculate_health_score guarantees non-negative; verify
        # the helper directly.
        from loom_tools.health_monitor import calculate_health_score as f

        # Build a snapshot with every penalty maxed.
        snap = _snap(
            stuck_tasks=100,
            orphan_building=100,
            ready_count=10_000,
            review_requested_count=10_000,
            changes_requested_count=10_000,
            merge_conflict_count=10_000,
        )
        assert 0 <= f(snap) <= 100


# ---------------------------------------------------------------------------
# Status mapping
# ---------------------------------------------------------------------------


class TestGetHealthStatus:
    @pytest.mark.parametrize(
        "score,expected",
        [
            (100, "excellent"),
            (95, "excellent"),
            (90, "excellent"),
            (85, "good"),
            (70, "good"),
            (65, "fair"),
            (50, "fair"),
            (40, "warning"),
            (30, "warning"),
            (20, "critical"),
            (0, "critical"),
        ],
    )
    def test_thresholds(self, score: int, expected: str):
        assert get_health_status(score) == expected


# ---------------------------------------------------------------------------
# Alert generation
# ---------------------------------------------------------------------------


class TestGenerateAlerts:
    def test_no_alerts_when_healthy(self):
        assert generate_alerts(_snap()) == []

    def test_stuck_tasks_warning(self):
        alerts = generate_alerts(_snap(stuck_tasks=1, running_tasks=1))
        assert len(alerts) == 1
        assert alerts[0].type == "stuck_tasks"
        assert alerts[0].severity == "warning"

    def test_stuck_tasks_critical(self):
        alerts = generate_alerts(_snap(stuck_tasks=3, running_tasks=3))
        assert any(a.type == "stuck_tasks" and a.severity == "critical" for a in alerts)

    def test_orphan_building_alert(self):
        alerts = generate_alerts(_snap(orphan_building=1))
        assert any(a.type == "orphan_building" for a in alerts)

    def test_orphan_building_critical(self):
        alerts = generate_alerts(_snap(orphan_building=3))
        assert any(
            a.type == "orphan_building" and a.severity == "critical" for a in alerts
        )

    def test_pipeline_stall(self):
        # ready >= 1, running_tasks == 0, no ready_to_merge -> stall alert
        alerts = generate_alerts(_snap(ready_count=3, running_tasks=0))
        assert any(a.type == "pipeline_stall" for a in alerts)

    def test_no_stall_when_workers_running(self):
        alerts = generate_alerts(_snap(ready_count=3, running_tasks=2))
        assert not any(a.type == "pipeline_stall" for a in alerts)

    def test_no_stall_when_ready_to_merge_present(self):
        # If there's a merge queue, the system isn't stalled even if no
        # tasks are running.
        alerts = generate_alerts(
            _snap(ready_count=3, running_tasks=0, ready_to_merge_count=1)
        )
        assert not any(a.type == "pipeline_stall" for a in alerts)

    def test_review_backlog_warning(self):
        alerts = generate_alerts(_snap(review_requested_count=5))
        assert any(
            a.type == "review_backlog" and a.severity == "warning" for a in alerts
        )

    def test_review_backlog_critical(self):
        alerts = generate_alerts(_snap(review_requested_count=10))
        assert any(
            a.type == "review_backlog" and a.severity == "critical" for a in alerts
        )

    def test_merge_conflict_alert(self):
        alerts = generate_alerts(_snap(merge_conflict_count=3))
        assert any(a.type == "merge_conflict_backlog" for a in alerts)

    def test_merge_conflict_critical(self):
        alerts = generate_alerts(_snap(merge_conflict_count=5))
        assert any(
            a.type == "merge_conflict_backlog" and a.severity == "critical"
            for a in alerts
        )


# ---------------------------------------------------------------------------
# Snapshot collection
# ---------------------------------------------------------------------------


class TestCollectSnapshot:
    def test_empty_pipeline_idle_loop(self, tmp_path: Path):
        sls = SpawnLoopState(started_at=TS, running=[], present=True)
        snap = collect_snapshot(
            tmp_path,
            _now=NOW,
            _pipeline_data={
                "ready_issues": [],
                "building_issues": [],
                "blocked_issues": [],
                "review_requested": [],
                "changes_requested": [],
                "ready_to_merge": [],
            },
            _spawn_loop_state=sls,
        )
        assert snap.ready_count == 0
        assert snap.running_tasks == 0
        assert snap.stuck_tasks == 0
        assert snap.orphan_building == 0
        assert snap.spawn_loop_present is True

    def test_counts_aggregated(self, tmp_path: Path):
        sls = SpawnLoopState(
            started_at=TS,
            running=[
                SpawnLoopTask(issue=42, pid=1, started_at=TS, last_heartbeat=TS),
            ],
            present=True,
        )
        snap = collect_snapshot(
            tmp_path,
            _now=NOW,
            _pipeline_data={
                "ready_issues": [{"number": 1}, {"number": 2}],
                "building_issues": [{"number": 42}, {"number": 99}],
                "blocked_issues": [{"number": 5}],
                "review_requested": [{"number": 10}],
                "changes_requested": [{"number": 11}],
                "ready_to_merge": [
                    {"number": 20, "labels": []},
                    {
                        "number": 21,
                        "labels": [{"name": "loom:pr"}, {"name": "loom:merge-conflict"}],
                    },
                ],
            },
            _spawn_loop_state=sls,
        )
        assert snap.ready_count == 2
        assert snap.building_count == 2
        assert snap.blocked_count == 1
        assert snap.review_requested_count == 1
        assert snap.changes_requested_count == 1
        assert snap.ready_to_merge_count == 2
        assert snap.merge_conflict_count == 1
        assert snap.running_tasks == 1
        # building_numbers = {42, 99}, running = {42} -> orphan = 1 (issue 99)
        assert snap.orphan_building == 1

    def test_stuck_heartbeat_counted(self, tmp_path: Path):
        # Heartbeat 3 minutes old; threshold default 120s -> stuck.
        stale_ts = "2026-06-02T11:57:00Z"
        sls = SpawnLoopState(
            started_at=TS,
            running=[
                SpawnLoopTask(issue=1, pid=1, started_at=stale_ts, last_heartbeat=stale_ts),
                SpawnLoopTask(issue=2, pid=2, started_at=TS, last_heartbeat=TS),  # fresh
            ],
            present=True,
        )
        snap = collect_snapshot(
            tmp_path,
            _now=NOW,
            _pipeline_data={},
            _spawn_loop_state=sls,
        )
        assert snap.stuck_tasks == 1

    def test_stuck_falls_back_to_started_at(self, tmp_path: Path):
        # Task with no last_heartbeat but old started_at -> stuck.
        stale_ts = "2026-06-02T11:50:00Z"  # 10 min ago
        sls = SpawnLoopState(
            started_at=TS,
            running=[
                SpawnLoopTask(issue=1, pid=1, started_at=stale_ts, last_heartbeat=None),
            ],
            present=True,
        )
        snap = collect_snapshot(tmp_path, _now=NOW, _pipeline_data={}, _spawn_loop_state=sls)
        assert snap.stuck_tasks == 1

    def test_no_orphan_when_spawn_loop_absent(self, tmp_path: Path):
        # If spawn loop isn't present we can't classify orphans.
        sls = SpawnLoopState.absent()
        snap = collect_snapshot(
            tmp_path,
            _now=NOW,
            _pipeline_data={"building_issues": [{"number": 1}, {"number": 2}]},
            _spawn_loop_state=sls,
        )
        assert snap.orphan_building == 0
        assert snap.spawn_loop_present is False

    def test_pipeline_query_failure_is_safe(self, tmp_path: Path):
        # When collect_pipeline_data raises, collect_snapshot returns zeros.
        sls = SpawnLoopState(started_at=TS, running=[], present=True)
        with patch(
            "loom_tools.forge_snapshot.collect_pipeline_data",
            side_effect=RuntimeError("forge unreachable"),
        ):
            snap = collect_snapshot(tmp_path, _now=NOW, _spawn_loop_state=sls)
        assert snap.ready_count == 0
        assert snap.running_tasks == 0


# ---------------------------------------------------------------------------
# Formatters
# ---------------------------------------------------------------------------


class TestFormatters:
    def test_json_output_shape(self):
        snap = _snap(ready_count=2, running_tasks=1)
        out = format_health_json(snap)
        data = json.loads(out)
        assert "health_score" in data
        assert "health_status" in data
        assert "snapshot" in data
        assert "alerts" in data
        assert data["snapshot"]["ready_count"] == 2

    def test_human_output_renders(self):
        snap = _snap(ready_count=2, running_tasks=1)
        out = format_health_human(snap)
        assert "LOOM HEALTH STATUS" in out
        assert "Health Score" in out
        assert "Issue Queues" in out

    def test_alerts_json(self):
        snap = _snap(stuck_tasks=1, running_tasks=1)
        out = format_alerts_json(snap)
        data = json.loads(out)
        assert "alerts" in data
        assert len(data["alerts"]) >= 1

    def test_alerts_human_empty(self):
        out = format_alerts_human(_snap())
        assert "No active alerts" in out

    def test_alerts_human_with_alerts(self):
        out = format_alerts_human(_snap(stuck_tasks=1, running_tasks=1))
        assert "stuck_tasks" in out


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


class TestCLI:
    def test_help(self):
        with pytest.raises(SystemExit) as exc:
            main(["--help"])
        assert exc.value.code == 0

    def test_default_run(self, tmp_path: Path, capsys):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()
        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.health_monitor.collect_snapshot",
            return_value=_snap(),
        ):
            rc = main([])
        assert rc == 0
        assert "LOOM HEALTH STATUS" in capsys.readouterr().out

    def test_json_flag(self, tmp_path: Path, capsys):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()
        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.health_monitor.collect_snapshot",
            return_value=_snap(),
        ):
            rc = main(["--json"])
        assert rc == 0
        data = json.loads(capsys.readouterr().out)
        assert data["health_score"] == 100

    def test_alerts_flag(self, tmp_path: Path, capsys):
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (tmp_path / ".git").mkdir()
        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ), patch(
            "loom_tools.health_monitor.collect_snapshot",
            return_value=_snap(stuck_tasks=1, running_tasks=1),
        ):
            rc = main(["--alerts"])
        assert rc == 0
        assert "stuck_tasks" in capsys.readouterr().out

    def test_retired_collect_flag(self, tmp_path: Path, capsys):
        # --collect was retired in #3397.
        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--collect"])
        assert rc == 2
        err = capsys.readouterr().err
        assert "retired" in err.lower()

    def test_retired_history_flag(self, tmp_path: Path, capsys):
        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--history", "4"])
        assert rc == 2

    def test_retired_acknowledge_flag(self, tmp_path: Path, capsys):
        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--acknowledge", "alert-1"])
        assert rc == 2

    def test_retired_clear_alerts_flag(self, tmp_path: Path, capsys):
        with patch(
            "loom_tools.health_monitor.find_repo_root", return_value=tmp_path
        ):
            rc = main(["--clear-alerts"])
        assert rc == 2

    def test_no_git_dir(self, tmp_path: Path, capsys):
        # find_repo_root raises FileNotFoundError -> exit 1
        with patch(
            "loom_tools.health_monitor.find_repo_root",
            side_effect=FileNotFoundError(),
        ):
            rc = main([])
        assert rc == 1
        assert "Not in a git repository" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# Alert dataclass
# ---------------------------------------------------------------------------


class TestAlert:
    def test_to_dict(self):
        a = Alert(
            type="t",
            severity="warning",
            message="m",
            context={"k": "v"},
        )
        d = a.to_dict()
        assert d == {
            "type": "t",
            "severity": "warning",
            "message": "m",
            "context": {"k": "v"},
        }
