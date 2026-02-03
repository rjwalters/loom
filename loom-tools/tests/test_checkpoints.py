"""Tests for loom_tools.checkpoints."""

from __future__ import annotations

import json
import pathlib

import pytest

from loom_tools.checkpoints import (
    CHECKPOINT_FILENAME,
    CHECKPOINT_STAGES,
    RECOVERY_PATHS,
    Checkpoint,
    CheckpointDetails,
    clear_checkpoint,
    get_checkpoint_path,
    get_recovery_recommendation,
    main,
    read_checkpoint,
    write_checkpoint,
)


@pytest.fixture
def worktree(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal worktree directory."""
    worktree_path = tmp_path / ".loom" / "worktrees" / "issue-42"
    worktree_path.mkdir(parents=True)
    return worktree_path


# ── Checkpoint stages and paths ─────────────────────────────────


class TestCheckpointStages:
    def test_stages_in_order(self) -> None:
        expected = ("planning", "implementing", "tested", "committed", "pushed", "pr_created")
        assert CHECKPOINT_STAGES == expected

    def test_each_stage_has_recovery_path(self) -> None:
        for stage in CHECKPOINT_STAGES:
            assert stage in RECOVERY_PATHS
            assert isinstance(RECOVERY_PATHS[stage], str)

    def test_recovery_paths(self) -> None:
        assert RECOVERY_PATHS["planning"] == "retry_from_scratch"
        assert RECOVERY_PATHS["implementing"] == "check_changes"
        assert RECOVERY_PATHS["tested"] == "route_to_commit"
        assert RECOVERY_PATHS["committed"] == "push_and_pr"
        assert RECOVERY_PATHS["pushed"] == "create_pr"
        assert RECOVERY_PATHS["pr_created"] == "verify_labels"


# ── CheckpointDetails ───────────────────────────────────────────


class TestCheckpointDetails:
    def test_empty_to_dict(self) -> None:
        details = CheckpointDetails()
        assert details.to_dict() == {}

    def test_with_all_fields(self) -> None:
        details = CheckpointDetails(
            files_changed=5,
            test_command="pnpm check:ci",
            test_result="pass",
            test_output_summary="45 tests passed",
            commit_sha="abc123",
            pr_number=100,
        )
        d = details.to_dict()
        assert d["files_changed"] == 5
        assert d["test_command"] == "pnpm check:ci"
        assert d["test_result"] == "pass"
        assert d["test_output_summary"] == "45 tests passed"
        assert d["commit_sha"] == "abc123"
        assert d["pr_number"] == 100

    def test_from_dict(self) -> None:
        data = {
            "files_changed": 3,
            "test_result": "fail",
            "unknown_field": "value",
        }
        details = CheckpointDetails.from_dict(data)
        assert details.files_changed == 3
        assert details.test_result == "fail"
        assert details.extra == {"unknown_field": "value"}


# ── Checkpoint ──────────────────────────────────────────────────


class TestCheckpoint:
    def test_to_dict_minimal(self) -> None:
        checkpoint = Checkpoint(stage="planning", timestamp="2026-01-25T10:00:00Z")
        d = checkpoint.to_dict()
        assert d["stage"] == "planning"
        assert d["timestamp"] == "2026-01-25T10:00:00Z"
        assert "issue" not in d  # Zero value not included
        assert "details" not in d  # Empty details not included

    def test_to_dict_full(self) -> None:
        details = CheckpointDetails(files_changed=5, test_result="pass")
        checkpoint = Checkpoint(
            stage="tested",
            timestamp="2026-01-25T10:00:00Z",
            issue=42,
            details=details,
        )
        d = checkpoint.to_dict()
        assert d["stage"] == "tested"
        assert d["issue"] == 42
        assert d["details"]["files_changed"] == 5
        assert d["details"]["test_result"] == "pass"

    def test_from_dict(self) -> None:
        data = {
            "stage": "committed",
            "timestamp": "2026-01-25T10:00:00Z",
            "issue": 100,
            "details": {
                "commit_sha": "def456",
            },
        }
        checkpoint = Checkpoint.from_dict(data)
        assert checkpoint.stage == "committed"
        assert checkpoint.issue == 100
        assert checkpoint.details.commit_sha == "def456"

    def test_recovery_path(self) -> None:
        assert Checkpoint(stage="planning", timestamp="").recovery_path == "retry_from_scratch"
        assert Checkpoint(stage="tested", timestamp="").recovery_path == "route_to_commit"
        assert Checkpoint(stage="pushed", timestamp="").recovery_path == "create_pr"
        assert Checkpoint(stage="unknown", timestamp="").recovery_path == "retry_from_scratch"

    def test_stage_index(self) -> None:
        assert Checkpoint(stage="planning", timestamp="").stage_index == 0
        assert Checkpoint(stage="implementing", timestamp="").stage_index == 1
        assert Checkpoint(stage="tested", timestamp="").stage_index == 2
        assert Checkpoint(stage="committed", timestamp="").stage_index == 3
        assert Checkpoint(stage="pushed", timestamp="").stage_index == 4
        assert Checkpoint(stage="pr_created", timestamp="").stage_index == 5
        assert Checkpoint(stage="unknown", timestamp="").stage_index == -1

    def test_is_after(self) -> None:
        checkpoint = Checkpoint(stage="tested", timestamp="")
        assert checkpoint.is_after("planning")
        assert checkpoint.is_after("implementing")
        assert not checkpoint.is_after("tested")
        assert not checkpoint.is_after("committed")
        assert not checkpoint.is_after("unknown")


# ── write_checkpoint ────────────────────────────────────────────


class TestWriteCheckpoint:
    def test_writes_checkpoint_file(self, worktree: pathlib.Path) -> None:
        ok = write_checkpoint(worktree, "implementing", issue=42, quiet=True)
        assert ok

        checkpoint_path = worktree / CHECKPOINT_FILENAME
        assert checkpoint_path.is_file()

        data = json.loads(checkpoint_path.read_text())
        assert data["stage"] == "implementing"
        assert data["issue"] == 42
        assert "timestamp" in data

    def test_writes_with_details(self, worktree: pathlib.Path) -> None:
        ok = write_checkpoint(
            worktree,
            "tested",
            issue=42,
            files_changed=5,
            test_command="pnpm check:ci",
            test_result="pass",
            quiet=True,
        )
        assert ok

        data = json.loads((worktree / CHECKPOINT_FILENAME).read_text())
        assert data["stage"] == "tested"
        assert data["details"]["files_changed"] == 5
        assert data["details"]["test_command"] == "pnpm check:ci"
        assert data["details"]["test_result"] == "pass"

    def test_invalid_stage_fails(self, worktree: pathlib.Path) -> None:
        ok = write_checkpoint(worktree, "invalid_stage", quiet=True)
        assert not ok
        assert not (worktree / CHECKPOINT_FILENAME).exists()

    def test_nonexistent_worktree_fails(self, tmp_path: pathlib.Path) -> None:
        nonexistent = tmp_path / "does_not_exist"
        ok = write_checkpoint(nonexistent, "planning", quiet=True)
        assert not ok

    def test_overwrites_previous_checkpoint(self, worktree: pathlib.Path) -> None:
        write_checkpoint(worktree, "planning", quiet=True)
        write_checkpoint(worktree, "implementing", quiet=True)

        data = json.loads((worktree / CHECKPOINT_FILENAME).read_text())
        assert data["stage"] == "implementing"


# ── read_checkpoint ─────────────────────────────────────────────


class TestReadCheckpoint:
    def test_reads_written_checkpoint(self, worktree: pathlib.Path) -> None:
        write_checkpoint(
            worktree,
            "committed",
            issue=100,
            commit_sha="abc123",
            quiet=True,
        )

        checkpoint = read_checkpoint(worktree)
        assert checkpoint is not None
        assert checkpoint.stage == "committed"
        assert checkpoint.issue == 100
        assert checkpoint.details.commit_sha == "abc123"

    def test_returns_none_for_missing_file(self, worktree: pathlib.Path) -> None:
        checkpoint = read_checkpoint(worktree)
        assert checkpoint is None

    def test_returns_none_for_invalid_json(self, worktree: pathlib.Path) -> None:
        (worktree / CHECKPOINT_FILENAME).write_text("not json")
        checkpoint = read_checkpoint(worktree)
        assert checkpoint is None

    def test_returns_none_for_non_dict_json(self, worktree: pathlib.Path) -> None:
        (worktree / CHECKPOINT_FILENAME).write_text("[]")
        checkpoint = read_checkpoint(worktree)
        assert checkpoint is None


# ── clear_checkpoint ────────────────────────────────────────────


class TestClearCheckpoint:
    def test_removes_existing_checkpoint(self, worktree: pathlib.Path) -> None:
        write_checkpoint(worktree, "planning", quiet=True)
        assert (worktree / CHECKPOINT_FILENAME).exists()

        ok = clear_checkpoint(worktree, quiet=True)
        assert ok
        assert not (worktree / CHECKPOINT_FILENAME).exists()

    def test_succeeds_when_no_checkpoint(self, worktree: pathlib.Path) -> None:
        ok = clear_checkpoint(worktree, quiet=True)
        assert ok


# ── get_checkpoint_path ─────────────────────────────────────────


class TestGetCheckpointPath:
    def test_returns_correct_path(self, worktree: pathlib.Path) -> None:
        path = get_checkpoint_path(worktree)
        assert path == worktree / CHECKPOINT_FILENAME


# ── get_recovery_recommendation ─────────────────────────────────


class TestGetRecoveryRecommendation:
    def test_no_checkpoint(self) -> None:
        rec = get_recovery_recommendation(None)
        assert rec["recovery_path"] == "retry_from_scratch"
        assert rec["skip_stages"] == []
        assert "No checkpoint found" in rec["details"]

    def test_planning_checkpoint(self) -> None:
        checkpoint = Checkpoint(stage="planning", timestamp="2026-01-25T10:00:00Z")
        rec = get_recovery_recommendation(checkpoint)
        assert rec["recovery_path"] == "retry_from_scratch"
        assert rec["skip_stages"] == ["planning"]

    def test_tested_checkpoint(self) -> None:
        details = CheckpointDetails(test_result="pass", files_changed=5)
        checkpoint = Checkpoint(
            stage="tested",
            timestamp="2026-01-25T10:00:00Z",
            details=details,
        )
        rec = get_recovery_recommendation(checkpoint)
        assert rec["recovery_path"] == "route_to_commit"
        assert rec["skip_stages"] == ["planning", "implementing", "tested"]
        assert "test_result=pass" in rec["details"]
        assert "files_changed=5" in rec["details"]

    def test_committed_checkpoint(self) -> None:
        checkpoint = Checkpoint(stage="committed", timestamp="2026-01-25T10:00:00Z")
        rec = get_recovery_recommendation(checkpoint)
        assert rec["recovery_path"] == "push_and_pr"
        assert rec["skip_stages"] == ["planning", "implementing", "tested", "committed"]

    def test_pushed_checkpoint(self) -> None:
        checkpoint = Checkpoint(stage="pushed", timestamp="2026-01-25T10:00:00Z")
        rec = get_recovery_recommendation(checkpoint)
        assert rec["recovery_path"] == "create_pr"
        assert rec["skip_stages"] == ["planning", "implementing", "tested", "committed", "pushed"]

    def test_pr_created_checkpoint(self) -> None:
        details = CheckpointDetails(pr_number=123)
        checkpoint = Checkpoint(
            stage="pr_created",
            timestamp="2026-01-25T10:00:00Z",
            details=details,
        )
        rec = get_recovery_recommendation(checkpoint)
        assert rec["recovery_path"] == "verify_labels"
        assert "pr_number=123" in rec["details"]
        assert "checkpoint" in rec  # Full checkpoint included


# ── CLI ─────────────────────────────────────────────────────────


class TestCLI:
    def test_write_and_read(self, worktree: pathlib.Path) -> None:
        # Write
        exit_code = main([
            "write",
            "--worktree", str(worktree),
            "--stage", "implementing",
            "--issue", "42",
            "--quiet",
        ])
        assert exit_code == 0
        assert (worktree / CHECKPOINT_FILENAME).exists()

        # Read
        exit_code = main([
            "read",
            "--worktree", str(worktree),
            "--json",
        ])
        assert exit_code == 0

    def test_write_with_test_result(self, worktree: pathlib.Path) -> None:
        exit_code = main([
            "write",
            "--worktree", str(worktree),
            "--stage", "tested",
            "--test-result", "pass",
            "--test-command", "pnpm check:ci",
            "--quiet",
        ])
        assert exit_code == 0

        checkpoint = read_checkpoint(worktree)
        assert checkpoint is not None
        assert checkpoint.details.test_result == "pass"
        assert checkpoint.details.test_command == "pnpm check:ci"

    def test_clear(self, worktree: pathlib.Path) -> None:
        write_checkpoint(worktree, "planning", quiet=True)

        exit_code = main([
            "clear",
            "--worktree", str(worktree),
            "--quiet",
        ])
        assert exit_code == 0
        assert not (worktree / CHECKPOINT_FILENAME).exists()

    def test_stages_command(self) -> None:
        exit_code = main(["stages"])
        assert exit_code == 0

    def test_stages_json(self) -> None:
        exit_code = main(["stages", "--json"])
        assert exit_code == 0

    def test_help(self) -> None:
        exit_code = main([])
        assert exit_code == 0

    def test_invalid_stage(self, worktree: pathlib.Path) -> None:
        # argparse exits with SystemExit for invalid choices
        with pytest.raises(SystemExit) as exc_info:
            main([
                "write",
                "--worktree", str(worktree),
                "--stage", "invalid",
                "--quiet",
            ])
        # argparse returns exit code 2 for argument errors
        assert exc_info.value.code == 2

    def test_write_missing_stage(self, worktree: pathlib.Path) -> None:
        exit_code = main([
            "write",
            "--worktree", str(worktree),
        ])
        assert exit_code == 1  # --stage is required
