"""Tests for the orphan_recovery module."""

from __future__ import annotations

import json
import pathlib
from datetime import datetime, timedelta, timezone
from unittest.mock import patch

import pytest

from loom_tools.common.time_utils import now_utc
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry
from loom_tools.models.progress import ShepherdProgress
from loom_tools.claim import claim_issue, has_valid_claim
from loom_tools.orphan_recovery import (
    DEFAULT_HEARTBEAT_STALE_THRESHOLD,
    OrphanEntry,
    OrphanRecoveryResult,
    RecoveryEntry,
    _check_task_exists,
    _cleanup_stale_worktree,
    _get_heartbeat_stale_threshold,
    _has_fresh_progress,
    _is_valid_task_id,
    check_daemon_state_tasks,
    check_stale_progress,
    check_untracked_building,
    format_result_human,
    format_result_json,
    main,
    recover_issue,
    recover_progress_file,
    recover_shepherd,
    run_orphan_recovery,
)


class TestTaskIdValidation:
    def test_valid_task_id(self) -> None:
        assert _is_valid_task_id("abc1234") is True
        assert _is_valid_task_id("0000000") is True
        assert _is_valid_task_id("a7dc1e0") is True

    def test_invalid_task_id_too_short(self) -> None:
        assert _is_valid_task_id("abc12") is False

    def test_invalid_task_id_too_long(self) -> None:
        assert _is_valid_task_id("abc12345") is False

    def test_invalid_task_id_uppercase(self) -> None:
        assert _is_valid_task_id("ABC1234") is False

    def test_invalid_task_id_non_hex(self) -> None:
        assert _is_valid_task_id("xyz1234") is False

    def test_empty_task_id(self) -> None:
        assert _is_valid_task_id("") is False


class TestCheckTaskExists:
    def test_output_file_exists(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "task.output"
        output_file.write_text("some output")
        assert _check_task_exists("abc1234", str(output_file)) is True

    def test_output_file_missing(self) -> None:
        assert _check_task_exists("abc1234", "/nonexistent/path.output") is False

    def test_no_output_file(self) -> None:
        assert _check_task_exists("abc1234", None) is False


class TestHeartbeatThreshold:
    def test_default_threshold(self) -> None:
        with patch.dict("os.environ", {}, clear=True):
            threshold = _get_heartbeat_stale_threshold()
            assert threshold == DEFAULT_HEARTBEAT_STALE_THRESHOLD

    def test_env_var_override(self) -> None:
        with patch.dict("os.environ", {"LOOM_HEARTBEAT_STALE_THRESHOLD": "600"}):
            threshold = _get_heartbeat_stale_threshold()
            assert threshold == 600

    def test_env_var_invalid(self) -> None:
        with patch.dict("os.environ", {"LOOM_HEARTBEAT_STALE_THRESHOLD": "invalid"}):
            threshold = _get_heartbeat_stale_threshold()
            assert threshold == DEFAULT_HEARTBEAT_STALE_THRESHOLD


class TestCheckDaemonStateTasks:
    def test_no_working_shepherds(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="idle"),
                "shepherd-2": ShepherdEntry(status="idle"),
            }
        )
        result = OrphanRecoveryResult()
        check_daemon_state_tasks(daemon_state, result)
        assert result.total_orphaned == 0

    def test_working_shepherd_no_task_id(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(status="working", issue=42),
            }
        )
        result = OrphanRecoveryResult()
        check_daemon_state_tasks(daemon_state, result)
        assert result.total_orphaned == 0

    def test_invalid_task_id_format(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working",
                    issue=42,
                    task_id="not-valid",
                ),
            }
        )
        result = OrphanRecoveryResult()
        check_daemon_state_tasks(daemon_state, result)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "invalid_task_id"
        assert result.orphaned[0].shepherd_id == "shepherd-1"
        assert result.orphaned[0].reason == "invalid_task_id_format"

    def test_stale_task_id(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working",
                    issue=42,
                    task_id="abc1234",
                    output_file="/nonexistent/output.txt",
                ),
            }
        )
        result = OrphanRecoveryResult()
        check_daemon_state_tasks(daemon_state, result)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "stale_task_id"
        assert result.orphaned[0].reason == "task_not_found"

    def test_valid_task_id_with_output(self, tmp_path: pathlib.Path) -> None:
        output_file = tmp_path / "task.output"
        output_file.write_text("some output")
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working",
                    issue=42,
                    task_id="abc1234",
                    output_file=str(output_file),
                ),
            }
        )
        result = OrphanRecoveryResult()
        check_daemon_state_tasks(daemon_state, result)
        assert result.total_orphaned == 0

    def test_multiple_shepherds_mixed(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working",
                    issue=42,
                    task_id="INVALID",
                ),
                "shepherd-2": ShepherdEntry(status="idle"),
                "shepherd-3": ShepherdEntry(
                    status="working",
                    issue=99,
                    task_id="abc1234",
                    output_file="/nonexistent/file",
                ),
            }
        )
        result = OrphanRecoveryResult()
        check_daemon_state_tasks(daemon_state, result)
        assert result.total_orphaned == 2
        types = {o.type for o in result.orphaned}
        assert "invalid_task_id" in types
        assert "stale_task_id" in types


class TestCheckUntrackedBuilding:
    def test_no_building_issues(self) -> None:
        daemon_state = DaemonState()
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ):
            check_untracked_building(daemon_state, [], result)
        assert result.total_orphaned == 0

    def test_tracked_building_issue(self) -> None:
        daemon_state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42, task_id="abc1234"
                ),
            }
        )
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(daemon_state, [], result)
        assert result.total_orphaned == 0

    def test_untracked_building_issue_no_progress(self) -> None:
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(daemon_state, [], result)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "untracked_building"
        assert result.orphaned[0].issue == 42
        assert result.orphaned[0].title == "Test issue"

    def test_untracked_with_fresh_progress(self) -> None:
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        fresh_hb = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="working",
                last_heartbeat=fresh_hb,
            )
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(daemon_state, progress, result)
        assert result.total_orphaned == 0

    def test_untracked_with_stale_progress(self) -> None:
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="working",
                last_heartbeat=stale_hb,
            )
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(
                daemon_state, progress, result, heartbeat_threshold=300
            )
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "untracked_building"

    def test_untracked_with_no_heartbeat_progress(self) -> None:
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="working",
                last_heartbeat=None,
            )
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(daemon_state, progress, result)
        assert result.total_orphaned == 1

    def test_gh_error_handled(self) -> None:
        daemon_state = DaemonState()
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            side_effect=Exception("gh failed"),
        ):
            check_untracked_building(daemon_state, [], result)
        assert result.total_orphaned == 0

    def test_untracked_with_valid_claim_skipped(self, tmp_path: pathlib.Path) -> None:
        """Issue with a valid file-based claim should NOT be flagged as orphaned."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True, exist_ok=True)
        (tmp_path / ".git").mkdir(exist_ok=True)
        claim_issue(tmp_path, 42, "cli-shepherd")

        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(
                daemon_state, [], result, repo_root=tmp_path
            )
        assert result.total_orphaned == 0

    def test_untracked_without_claim_still_orphaned(self, tmp_path: pathlib.Path) -> None:
        """Issue without a claim should still be flagged as orphaned."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True, exist_ok=True)
        (tmp_path / ".git").mkdir(exist_ok=True)

        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(
                daemon_state, [], result, repo_root=tmp_path
            )
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "untracked_building"


class TestCheckStaleProgress:
    def test_no_progress_files(self) -> None:
        result = OrphanRecoveryResult()
        check_stale_progress([], result)
        assert result.total_orphaned == 0

    def test_non_working_progress(self) -> None:
        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="completed",
                last_heartbeat=now_utc().strftime("%Y-%m-%dT%H:%M:%SZ"),
            )
        ]
        result = OrphanRecoveryResult()
        check_stale_progress(progress, result)
        assert result.total_orphaned == 0

    def test_fresh_heartbeat(self) -> None:
        fresh_hb = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="working",
                last_heartbeat=fresh_hb,
            )
        ]
        result = OrphanRecoveryResult()
        check_stale_progress(progress, result, heartbeat_threshold=300)
        assert result.total_orphaned == 0

    def test_stale_heartbeat(self) -> None:
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="working",
                last_heartbeat=stale_hb,
            )
        ]
        result = OrphanRecoveryResult()
        check_stale_progress(progress, result, heartbeat_threshold=300)
        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "stale_heartbeat"
        assert result.orphaned[0].task_id == "abc1234"
        assert result.orphaned[0].issue == 42
        assert result.orphaned[0].age_seconds is not None
        assert result.orphaned[0].age_seconds >= 600

    def test_no_heartbeat_field(self) -> None:
        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="working",
                last_heartbeat=None,
            )
        ]
        result = OrphanRecoveryResult()
        check_stale_progress(progress, result)
        assert result.total_orphaned == 0

    def test_multiple_progress_files(self) -> None:
        fresh_hb = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")

        progress = [
            ShepherdProgress(
                task_id="abc1234",
                issue=42,
                status="working",
                last_heartbeat=fresh_hb,
            ),
            ShepherdProgress(
                task_id="def5678",
                issue=99,
                status="working",
                last_heartbeat=stale_hb,
            ),
        ]
        result = OrphanRecoveryResult()
        check_stale_progress(progress, result, heartbeat_threshold=300)
        assert result.total_orphaned == 1
        assert result.orphaned[0].task_id == "def5678"


class TestRecoverShepherd:
    def test_reset_shepherd_in_daemon_state(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        daemon_state = {
            "running": True,
            "shepherds": {
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "output_file": "/tmp/output.txt",
                },
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.gh_run"):
            recover_shepherd(
                tmp_path, "shepherd-1", 42, "abc1234", "stale_task_id", result
            )

        updated = json.loads((loom_dir / "daemon-state.json").read_text())
        assert updated["shepherds"]["shepherd-1"]["status"] == "idle"
        assert updated["shepherds"]["shepherd-1"]["idle_reason"] == "orphan_recovery"
        assert updated["shepherds"]["shepherd-1"]["last_issue"] == 42

        # Should have recovery entries for both shepherd reset and issue reset
        assert result.total_recovered >= 1
        actions = {r.action for r in result.recovered}
        assert "reset_shepherd" in actions
        assert "reset_issue_label" in actions

    def test_reset_shepherd_no_issue(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        daemon_state = {
            "running": True,
            "shepherds": {
                "shepherd-1": {
                    "status": "working",
                    "task_id": "abc1234",
                },
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        result = OrphanRecoveryResult(recover_mode=True)
        recover_shepherd(
            tmp_path, "shepherd-1", None, "abc1234", "stale_task_id", result
        )

        # Should only have shepherd reset, no issue recovery
        assert result.total_recovered == 1
        assert result.recovered[0].action == "reset_shepherd"


class TestRecoverIssue:
    def test_label_swap(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "test_reason", result)

        # Should have called gh_run twice: once for label edit, once for comment
        assert mock_gh.call_count == 2

        # Check the label edit call
        label_call = mock_gh.call_args_list[0]
        args = label_call[0][0]
        assert "issue" in args
        assert "edit" in args
        assert "42" in args
        assert "--remove-label" in args
        assert "loom:building" in args
        assert "--add-label" in args
        assert "loom:issue" in args

        assert result.total_recovered == 1
        assert result.recovered[0].action == "reset_issue_label"

    def test_label_swap_failure(self) -> None:
        result = OrphanRecoveryResult(recover_mode=True)

        with patch(
            "loom_tools.orphan_recovery.gh_run",
            side_effect=Exception("gh failed"),
        ):
            recover_issue(42, "test_reason", result)

        # Failed to update labels, no recovery entry added
        assert result.total_recovered == 0

    def test_recover_skipped_with_valid_claim(self, tmp_path: pathlib.Path) -> None:
        """recover_issue should skip when a valid file-based claim exists."""
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True, exist_ok=True)
        (tmp_path / ".git").mkdir(exist_ok=True)
        claim_issue(tmp_path, 42, "cli-shepherd")

        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "test_reason", result, repo_root=tmp_path)

        # gh_run should never be called — recovery is skipped
        mock_gh.assert_not_called()
        assert result.total_recovered == 0


class TestRecoverProgressFile:
    def test_mark_progress_errored(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)

        progress_data = {
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
            "last_heartbeat": "2026-01-01T00:00:00Z",
            "milestones": [],
        }
        progress_path = progress_dir / "shepherd-abc1234.json"
        progress_path.write_text(json.dumps(progress_data))

        progress = ShepherdProgress(
            task_id="abc1234",
            issue=42,
            status="working",
            last_heartbeat="2026-01-01T00:00:00Z",
        )

        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.gh_run"):
            recover_progress_file(tmp_path, progress, result)

        updated = json.loads(progress_path.read_text())
        assert updated["status"] == "errored"
        assert len(updated["milestones"]) == 1
        assert updated["milestones"][0]["event"] == "error"
        assert updated["milestones"][0]["data"]["error"] == "orphan_recovery"
        assert updated["milestones"][0]["data"]["will_retry"] is False

        # Should have recovery entries for progress file and issue
        actions = {r.action for r in result.recovered}
        assert "mark_progress_errored" in actions
        assert "reset_issue_label" in actions

    def test_missing_progress_file(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        progress = ShepherdProgress(
            task_id="abc1234",
            issue=42,
            status="working",
        )

        result = OrphanRecoveryResult(recover_mode=True)
        recover_progress_file(tmp_path, progress, result)
        assert result.total_recovered == 0


class TestRunOrphanRecovery:
    @pytest.fixture
    def mock_repo(self, tmp_path: pathlib.Path) -> pathlib.Path:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir()

        daemon_state = {
            "running": True,
            "shepherds": {
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "INVALID",
                },
                "shepherd-2": {"status": "idle"},
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        return tmp_path

    def test_dry_run_detection(self, mock_repo: pathlib.Path) -> None:
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ):
            result = run_orphan_recovery(mock_repo, recover=False)

        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "invalid_task_id"
        assert result.total_recovered == 0

    def test_recovery_mode(self, mock_repo: pathlib.Path) -> None:
        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ), patch("loom_tools.orphan_recovery.gh_run"):
            result = run_orphan_recovery(mock_repo, recover=True)

        assert result.total_orphaned == 1
        assert result.total_recovered >= 1

    def test_no_orphans(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()

        daemon_state = {
            "running": True,
            "shepherds": {
                "shepherd-1": {"status": "idle"},
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list", return_value=[]
        ):
            result = run_orphan_recovery(tmp_path)

        assert result.total_orphaned == 0
        assert result.total_recovered == 0


class TestOrphanRecoveryResult:
    def test_to_dict(self) -> None:
        result = OrphanRecoveryResult(
            orphaned=[
                OrphanEntry(
                    type="stale_task_id",
                    shepherd_id="shepherd-1",
                    issue=42,
                    task_id="abc1234",
                    reason="task_not_found",
                ),
            ],
            recovered=[
                RecoveryEntry(
                    action="reset_shepherd",
                    shepherd_id="shepherd-1",
                    issue=42,
                    task_id="abc1234",
                    reason="stale_task_id",
                ),
            ],
            recover_mode=True,
        )
        d = result.to_dict()
        assert d["total_orphaned"] == 1
        assert d["total_recovered"] == 1
        assert d["recover_mode"] is True
        assert d["orphaned"][0]["type"] == "stale_task_id"
        assert d["recovered"][0]["action"] == "reset_shepherd"


class TestFormatResultJson:
    def test_json_format(self) -> None:
        result = OrphanRecoveryResult(
            orphaned=[
                OrphanEntry(
                    type="stale_task_id",
                    shepherd_id="shepherd-1",
                    issue=42,
                    task_id="abc1234",
                    reason="task_not_found",
                ),
            ],
            recover_mode=False,
        )
        output = format_result_json(result)
        data = json.loads(output)

        assert data["total_orphaned"] == 1
        assert data["total_recovered"] == 0
        assert data["recover_mode"] is False
        assert len(data["orphaned"]) == 1
        assert data["orphaned"][0]["type"] == "stale_task_id"
        assert data["orphaned"][0]["shepherd_id"] == "shepherd-1"

    def test_empty_json(self) -> None:
        result = OrphanRecoveryResult()
        output = format_result_json(result)
        data = json.loads(output)

        assert data["total_orphaned"] == 0
        assert data["total_recovered"] == 0
        assert data["orphaned"] == []
        assert data["recovered"] == []


class TestFormatResultHuman:
    def test_no_orphans(self) -> None:
        result = OrphanRecoveryResult()
        output = format_result_human(result)
        assert "No orphaned shepherds found" in output

    def test_with_orphans_dry_run(self) -> None:
        result = OrphanRecoveryResult(
            orphaned=[
                OrphanEntry(
                    type="stale_task_id",
                    shepherd_id="shepherd-1",
                    issue=42,
                    task_id="abc1234",
                    reason="task_not_found",
                ),
                OrphanEntry(
                    type="untracked_building",
                    issue=99,
                    title="Fix bug",
                    reason="no_daemon_entry",
                ),
            ],
            recover_mode=False,
        )
        output = format_result_human(result)
        assert "Found 2 orphaned shepherd(s)" in output
        assert "shepherd-1" in output
        assert "#99" in output
        assert "--recover" in output

    def test_with_orphans_recovered(self) -> None:
        result = OrphanRecoveryResult(
            orphaned=[
                OrphanEntry(
                    type="stale_heartbeat",
                    task_id="abc1234",
                    issue=42,
                    age_seconds=600,
                    reason="heartbeat_stale",
                ),
            ],
            recovered=[
                RecoveryEntry(
                    action="mark_progress_errored",
                    task_id="abc1234",
                    issue=42,
                    reason="stale_heartbeat",
                ),
            ],
            recover_mode=True,
        )
        output = format_result_human(result)
        assert "Found 1 orphaned shepherd(s)" in output
        assert "Recovered 1 item(s)" in output
        assert "stale_heartbeat" in output


class TestMainCli:
    def test_help_flag(self) -> None:
        with pytest.raises(SystemExit) as exc_info:
            main(["--help"])
        assert exc_info.value.code == 0

    def test_no_repo(self) -> None:
        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            side_effect=FileNotFoundError("no repo"),
        ):
            exit_code = main([])
        assert exit_code == 1

    def test_dry_run_no_orphans(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        daemon_state = {"running": True, "shepherds": {}}
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            return_value=tmp_path,
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=[],
        ):
            exit_code = main([])
        assert exit_code == 0

    def test_dry_run_with_orphans_returns_2(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        daemon_state = {
            "running": True,
            "shepherds": {
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "INVALID",
                },
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            return_value=tmp_path,
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=[],
        ):
            exit_code = main([])
        assert exit_code == 2

    def test_json_output(self, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        daemon_state = {"running": True, "shepherds": {}}
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            return_value=tmp_path,
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=[],
        ):
            exit_code = main(["--json"])

        assert exit_code == 0
        captured = capsys.readouterr()
        data = json.loads(captured.out)
        assert data["total_orphaned"] == 0

    def test_recover_mode(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        daemon_state = {
            "running": True,
            "shepherds": {
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "INVALID",
                },
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(daemon_state))

        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            return_value=tmp_path,
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=[],
        ), patch(
            "loom_tools.orphan_recovery.gh_run",
        ):
            exit_code = main(["--recover"])
        assert exit_code == 0


class TestCleanupStaleWorktree:
    """Tests for _cleanup_stale_worktree function."""

    def test_no_worktree_returns_false(self, tmp_path: pathlib.Path) -> None:
        result = _cleanup_stale_worktree(tmp_path, 42)
        assert result is False

    def test_stale_worktree_cleaned_up(self, tmp_path: pathlib.Path) -> None:
        """Worktree with 0 commits ahead and no changes should be cleaned."""
        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            # Simulate: worktree exists, 0 commits ahead, no changes, branch name
            worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
            worktree_path.mkdir(parents=True)

            def side_effect(cmd, **kwargs):
                from unittest.mock import MagicMock

                m = MagicMock()
                m.returncode = 0
                if "log" in cmd:
                    m.stdout = ""  # No commits ahead
                elif "status" in cmd:
                    m.stdout = ""  # No changes
                elif "rev-parse" in cmd:
                    m.stdout = "feature/issue-42\n"
                elif "worktree" in cmd:
                    m.stdout = ""
                else:
                    m.stdout = ""
                return m

            mock_run.side_effect = side_effect
            cleaned = _cleanup_stale_worktree(tmp_path, 42)

        assert cleaned is True

    def test_worktree_with_commits_not_cleaned(self, tmp_path: pathlib.Path) -> None:
        """Worktree with commits ahead of main should NOT be cleaned."""
        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
            worktree_path.mkdir(parents=True)

            def side_effect(cmd, **kwargs):
                from unittest.mock import MagicMock

                m = MagicMock()
                m.returncode = 0
                if "log" in cmd:
                    m.stdout = "abc1234 some commit\n"  # Has commits ahead
                else:
                    m.stdout = ""
                return m

            mock_run.side_effect = side_effect
            cleaned = _cleanup_stale_worktree(tmp_path, 42)

        assert cleaned is False

    def test_worktree_with_meaningful_changes_not_cleaned(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Worktree with uncommitted source changes should NOT be cleaned."""
        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
            worktree_path.mkdir(parents=True)

            def side_effect(cmd, **kwargs):
                from unittest.mock import MagicMock

                m = MagicMock()
                m.returncode = 0
                if "log" in cmd:
                    m.stdout = ""  # No commits
                elif "status" in cmd:
                    m.stdout = " M src/main.py\n"  # Meaningful change
                else:
                    m.stdout = ""
                return m

            mock_run.side_effect = side_effect
            cleaned = _cleanup_stale_worktree(tmp_path, 42)

        assert cleaned is False

    def test_worktree_with_only_build_artifacts_cleaned(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Worktree with only build artifact changes should be cleaned."""
        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
            worktree_path.mkdir(parents=True)

            def side_effect(cmd, **kwargs):
                from unittest.mock import MagicMock

                m = MagicMock()
                m.returncode = 0
                if "log" in cmd:
                    m.stdout = ""  # No commits
                elif "status" in cmd:
                    m.stdout = "?? node_modules/foo\n M Cargo.lock\n"
                elif "rev-parse" in cmd:
                    m.stdout = "feature/issue-42\n"
                elif "worktree" in cmd:
                    m.stdout = ""
                else:
                    m.stdout = ""
                return m

            mock_run.side_effect = side_effect
            cleaned = _cleanup_stale_worktree(tmp_path, 42)

        assert cleaned is True

    def test_git_log_failure_returns_false(self, tmp_path: pathlib.Path) -> None:
        """If git log fails, we can't determine status so don't clean."""
        with patch("loom_tools.orphan_recovery.subprocess.run") as mock_run:
            worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
            worktree_path.mkdir(parents=True)

            def side_effect(cmd, **kwargs):
                from unittest.mock import MagicMock

                m = MagicMock()
                if "log" in cmd:
                    m.returncode = 128  # git error
                    m.stdout = ""
                else:
                    m.returncode = 0
                    m.stdout = ""
                return m

            mock_run.side_effect = side_effect
            cleaned = _cleanup_stale_worktree(tmp_path, 42)

        assert cleaned is False


class TestRecoverIssueClaimsAndWorktree:
    """Tests for claim-check and worktree-cleanup behavior in recover_issue."""

    def test_recover_skipped_with_valid_claim(self, tmp_path: pathlib.Path) -> None:
        """recover_issue should skip when a valid file claim exists."""
        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.has_valid_claim", return_value=True):
            recover_issue(42, "test_reason", result, repo_root=tmp_path)

        assert result.total_recovered == 0

    def test_recover_proceeds_without_claim(self) -> None:
        """recover_issue should proceed when no valid claim exists."""
        result = OrphanRecoveryResult(recover_mode=True)

        with patch(
            "loom_tools.orphan_recovery.has_valid_claim", return_value=False
        ), patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "test_reason", result, repo_root=pathlib.Path("/fake"))

        assert mock_gh.call_count == 2  # label edit + comment
        assert result.total_recovered == 1
        assert result.recovered[0].action == "reset_issue_label"

    def test_recover_cleans_stale_worktree(self, tmp_path: pathlib.Path) -> None:
        """recover_issue should clean stale worktree and add recovery entry."""
        result = OrphanRecoveryResult(recover_mode=True)

        with patch(
            "loom_tools.orphan_recovery.has_valid_claim", return_value=False
        ), patch(
            "loom_tools.orphan_recovery._cleanup_stale_worktree", return_value=True
        ) as mock_cleanup, patch(
            "loom_tools.orphan_recovery.gh_run"
        ):
            recover_issue(42, "test_reason", result, repo_root=tmp_path)

        mock_cleanup.assert_called_once_with(tmp_path, 42)
        # Should have cleanup_stale_worktree + reset_issue_label
        actions = {r.action for r in result.recovered}
        assert "cleanup_stale_worktree" in actions
        assert "reset_issue_label" in actions

    def test_recover_no_repo_root_skips_claim_and_worktree(self) -> None:
        """When repo_root is None, skip claim check and worktree cleanup."""
        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "test_reason", result)

        assert mock_gh.call_count == 2
        assert result.total_recovered == 1
        # No cleanup_stale_worktree entry
        actions = {r.action for r in result.recovered}
        assert "cleanup_stale_worktree" not in actions

    def test_recover_comment_includes_worktree_cleanup(
        self, tmp_path: pathlib.Path
    ) -> None:
        """When worktree is cleaned, the comment should mention it."""
        result = OrphanRecoveryResult(recover_mode=True)

        with patch(
            "loom_tools.orphan_recovery.has_valid_claim", return_value=False
        ), patch(
            "loom_tools.orphan_recovery._cleanup_stale_worktree", return_value=True
        ), patch(
            "loom_tools.orphan_recovery.gh_run"
        ) as mock_gh:
            recover_issue(42, "test_reason", result, repo_root=tmp_path)

        # The second gh_run call is the comment
        comment_call = mock_gh.call_args_list[1]
        comment_body = comment_call[0][0][-1]  # last arg is the body
        assert "stale worktree" in comment_body.lower()


class TestCheckUntrackedBuildingClaims:
    """Tests for claim-check behavior in check_untracked_building."""

    def test_untracked_with_valid_claim_skipped(self) -> None:
        """Issue with valid file claim should not be flagged as orphaned."""
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ), patch(
            "loom_tools.orphan_recovery.has_valid_claim", return_value=True
        ):
            check_untracked_building(
                daemon_state, [], result, repo_root=pathlib.Path("/fake")
            )

        assert result.total_orphaned == 0

    def test_untracked_without_claim_still_orphaned(self) -> None:
        """Issue without valid claim should still be flagged as orphaned."""
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ), patch(
            "loom_tools.orphan_recovery.has_valid_claim", return_value=False
        ):
            check_untracked_building(
                daemon_state, [], result, repo_root=pathlib.Path("/fake")
            )

        assert result.total_orphaned == 1
        assert result.orphaned[0].type == "untracked_building"

    def test_untracked_no_repo_root_still_orphaned(self) -> None:
        """Without repo_root, claim check is skipped and issue is orphaned."""
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ):
            check_untracked_building(daemon_state, [], result)

        assert result.total_orphaned == 1


class TestHasFreshProgress:
    """Tests for _has_fresh_progress re-read helper."""

    def test_fresh_heartbeat_returns_true(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)

        fresh_hb = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        progress_data = {
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
            "last_heartbeat": fresh_hb,
            "milestones": [],
        }
        (progress_dir / "shepherd-abc1234.json").write_text(
            json.dumps(progress_data)
        )

        assert _has_fresh_progress(tmp_path, 42) is True

    def test_stale_heartbeat_returns_false(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)

        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        progress_data = {
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
            "last_heartbeat": stale_hb,
            "milestones": [],
        }
        (progress_dir / "shepherd-abc1234.json").write_text(
            json.dumps(progress_data)
        )

        assert _has_fresh_progress(tmp_path, 42, heartbeat_threshold=300) is False

    def test_no_progress_file_returns_false(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)

        assert _has_fresh_progress(tmp_path, 42) is False

    def test_non_working_status_returns_false(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)

        fresh_hb = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        progress_data = {
            "task_id": "abc1234",
            "issue": 42,
            "status": "completed",
            "last_heartbeat": fresh_hb,
            "milestones": [],
        }
        (progress_dir / "shepherd-abc1234.json").write_text(
            json.dumps(progress_data)
        )

        assert _has_fresh_progress(tmp_path, 42) is False

    def test_no_heartbeat_returns_false(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)

        progress_data = {
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
            "milestones": [],
        }
        (progress_dir / "shepherd-abc1234.json").write_text(
            json.dumps(progress_data)
        )

        assert _has_fresh_progress(tmp_path, 42) is False


class TestRecoverIssueProgressRecheck:
    """Tests that recover_issue re-reads progress files before acting."""

    def test_recover_skipped_with_fresh_progress_on_reread(
        self, tmp_path: pathlib.Path
    ) -> None:
        """recover_issue should skip when a fresh progress heartbeat exists on disk."""
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)
        (tmp_path / ".git").mkdir(exist_ok=True)

        fresh_hb = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        progress_data = {
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
            "last_heartbeat": fresh_hb,
            "milestones": [],
        }
        (progress_dir / "shepherd-abc1234.json").write_text(
            json.dumps(progress_data)
        )

        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.gh_run") as mock_gh:
            recover_issue(42, "no_daemon_entry", result, repo_root=tmp_path)

        # gh_run should NOT be called — recovery is skipped
        mock_gh.assert_not_called()
        assert result.total_recovered == 0

    def test_recover_proceeds_with_stale_progress_on_reread(
        self, tmp_path: pathlib.Path
    ) -> None:
        """recover_issue should proceed when progress heartbeat is stale on disk."""
        loom_dir = tmp_path / ".loom"
        progress_dir = loom_dir / "progress"
        progress_dir.mkdir(parents=True)
        (tmp_path / ".git").mkdir(exist_ok=True)

        old_time = datetime.now(timezone.utc) - timedelta(seconds=600)
        stale_hb = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        progress_data = {
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
            "last_heartbeat": stale_hb,
            "milestones": [],
        }
        (progress_dir / "shepherd-abc1234.json").write_text(
            json.dumps(progress_data)
        )

        result = OrphanRecoveryResult(recover_mode=True)

        with patch("loom_tools.orphan_recovery.gh_run"):
            recover_issue(
                42, "no_daemon_entry", result,
                repo_root=tmp_path, heartbeat_threshold=300,
            )

        assert result.total_recovered == 1
        assert result.recovered[0].action == "reset_issue_label"

    def test_recover_proceeds_without_progress_file(self) -> None:
        """recover_issue should proceed when no progress file exists on disk."""
        result = OrphanRecoveryResult(recover_mode=True)

        with patch(
            "loom_tools.orphan_recovery.has_valid_claim", return_value=False
        ), patch(
            "loom_tools.orphan_recovery._has_fresh_progress", return_value=False
        ), patch(
            "loom_tools.orphan_recovery.gh_run",
        ):
            recover_issue(
                42, "no_daemon_entry", result,
                repo_root=pathlib.Path("/fake"),
            )

        assert result.total_recovered == 1
        assert result.recovered[0].action == "reset_issue_label"


class TestCheckUntrackedBuildingLogging:
    """Tests for logging behavior when repo_root is None."""

    def test_no_repo_root_logs_warning(self) -> None:
        """When repo_root is None, a warning should be logged about skipping claim check."""
        daemon_state = DaemonState()
        building_issues = [
            {"number": 42, "title": "Test issue", "labels": [], "state": "OPEN"}
        ]
        result = OrphanRecoveryResult()

        with patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=building_issues,
        ), patch(
            "loom_tools.orphan_recovery.log_warning"
        ) as mock_warn:
            check_untracked_building(daemon_state, [], result)

        # Should have warned about repo_root being None
        assert any(
            "repo_root is None" in str(call) for call in mock_warn.call_args_list
        )
        assert result.total_orphaned == 1


class TestExitCodes:
    """Verify exit code convention matches stuck_detection.py."""

    def test_exit_0_no_orphans(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text(
            json.dumps({"running": True, "shepherds": {}})
        )

        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            return_value=tmp_path,
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=[],
        ):
            assert main([]) == 0

    def test_exit_1_error(self) -> None:
        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            side_effect=FileNotFoundError("no repo"),
        ):
            assert main([]) == 1

    def test_exit_2_orphans_detected(self, tmp_path: pathlib.Path) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        (loom_dir / "daemon-state.json").write_text(
            json.dumps({
                "running": True,
                "shepherds": {
                    "shepherd-1": {
                        "status": "working",
                        "issue": 42,
                        "task_id": "INVALID",
                    },
                },
            })
        )

        with patch(
            "loom_tools.orphan_recovery.find_repo_root",
            return_value=tmp_path,
        ), patch(
            "loom_tools.orphan_recovery.gh_issue_list",
            return_value=[],
        ):
            assert main([]) == 2
