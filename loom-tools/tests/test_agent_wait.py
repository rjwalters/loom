"""Tests for agent_wait models."""

from __future__ import annotations

import os
from unittest import mock

import pytest

from loom_tools.models.agent_wait import (
    CompletionReason,
    ContractCheckResult,
    MonitorConfig,
    SignalType,
    StuckAction,
    StuckConfig,
    WaitResult,
    WaitStatus,
)


class TestWaitStatus:
    def test_enum_values(self) -> None:
        assert WaitStatus.COMPLETED.value == "completed"
        assert WaitStatus.TIMEOUT.value == "timeout"
        assert WaitStatus.SESSION_NOT_FOUND.value == "session_not_found"
        assert WaitStatus.SIGNAL.value == "signal"
        assert WaitStatus.STUCK.value == "stuck"
        assert WaitStatus.ERRORED.value == "errored"


class TestSignalType:
    def test_enum_values(self) -> None:
        assert SignalType.SHUTDOWN.value == "shutdown"
        assert SignalType.ABORT.value == "abort"


class TestCompletionReason:
    def test_enum_values(self) -> None:
        assert CompletionReason.EXPLICIT_EXIT.value == "explicit_exit"
        assert CompletionReason.PHASE_CONTRACT_SATISFIED.value == "phase_contract_satisfied"
        assert CompletionReason.BUILDER_PR_CREATED.value == "builder_pr_created"
        assert CompletionReason.JUDGE_REVIEW_COMPLETE.value == "judge_review_complete"
        assert CompletionReason.DOCTOR_FIXES_COMPLETE.value == "doctor_fixes_complete"
        assert CompletionReason.CURATOR_CURATION_COMPLETE.value == "curator_curation_complete"


class TestStuckAction:
    def test_enum_values(self) -> None:
        assert StuckAction.WARN.value == "warn"
        assert StuckAction.PAUSE.value == "pause"
        assert StuckAction.RESTART.value == "restart"
        assert StuckAction.RETRY.value == "retry"


class TestStuckConfig:
    def test_defaults(self) -> None:
        config = StuckConfig()
        assert config.warning_threshold == 300
        assert config.critical_threshold == 600
        assert config.prompt_stuck_check_interval == 10
        assert config.prompt_stuck_age_threshold == 30
        assert config.prompt_stuck_recovery_cooldown == 60
        assert config.action == StuckAction.WARN

    def test_from_env_defaults(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True):
            # Remove our env vars if they exist
            for key in [
                "LOOM_STUCK_WARNING",
                "LOOM_STUCK_CRITICAL",
                "LOOM_STUCK_ACTION",
                "LOOM_PROMPT_STUCK_CHECK_INTERVAL",
                "LOOM_PROMPT_STUCK_AGE_THRESHOLD",
                "LOOM_PROMPT_STUCK_RECOVERY_COOLDOWN",
            ]:
                os.environ.pop(key, None)
            config = StuckConfig.from_env()
        assert config.warning_threshold == 300
        assert config.critical_threshold == 600
        assert config.prompt_stuck_check_interval == 10
        assert config.prompt_stuck_age_threshold == 30
        assert config.prompt_stuck_recovery_cooldown == 60
        assert config.action == StuckAction.WARN

    def test_from_env_custom(self) -> None:
        with mock.patch.dict(os.environ, {
            "LOOM_STUCK_WARNING": "180",
            "LOOM_STUCK_CRITICAL": "360",
            "LOOM_STUCK_ACTION": "pause",
            "LOOM_PROMPT_STUCK_CHECK_INTERVAL": "5",
            "LOOM_PROMPT_STUCK_AGE_THRESHOLD": "15",
            "LOOM_PROMPT_STUCK_RECOVERY_COOLDOWN": "30",
        }):
            config = StuckConfig.from_env()
        assert config.warning_threshold == 180
        assert config.critical_threshold == 360
        assert config.prompt_stuck_check_interval == 5
        assert config.prompt_stuck_age_threshold == 15
        assert config.prompt_stuck_recovery_cooldown == 30
        assert config.action == StuckAction.PAUSE

    def test_from_env_invalid_action(self) -> None:
        with mock.patch.dict(os.environ, {"LOOM_STUCK_ACTION": "invalid"}):
            config = StuckConfig.from_env()
        assert config.action == StuckAction.WARN  # Falls back to default


class TestWaitResult:
    def test_basic_result(self) -> None:
        result = WaitResult(
            status=WaitStatus.COMPLETED,
            name="builder-issue-42",
            elapsed=120,
            reason=CompletionReason.BUILDER_PR_CREATED,
        )
        assert result.status == WaitStatus.COMPLETED
        assert result.name == "builder-issue-42"
        assert result.elapsed == 120
        assert result.reason == CompletionReason.BUILDER_PR_CREATED

    def test_to_dict_minimal(self) -> None:
        result = WaitResult(
            status=WaitStatus.TIMEOUT,
            name="test-agent",
            elapsed=3600,
        )
        d = result.to_dict()
        assert d == {
            "status": "timeout",
            "name": "test-agent",
            "elapsed": 3600,
        }
        # None values should not appear
        assert "reason" not in d
        assert "signal_type" not in d
        assert "stuck_status" not in d

    def test_to_dict_full(self) -> None:
        result = WaitResult(
            status=WaitStatus.STUCK,
            name="test-agent",
            elapsed=600,
            stuck_status="CRITICAL",
            stuck_action="paused",
            idle_time=600,
        )
        d = result.to_dict()
        assert d["status"] == "stuck"
        assert d["stuck_status"] == "CRITICAL"
        assert d["action"] == "paused"
        assert d["idle_time"] == 600

    def test_to_dict_with_signal(self) -> None:
        result = WaitResult(
            status=WaitStatus.SIGNAL,
            name="test-agent",
            elapsed=50,
            signal_type=SignalType.ABORT,
        )
        d = result.to_dict()
        assert d["signal_type"] == "abort"

    def test_to_dict_with_error(self) -> None:
        result = WaitResult(
            status=WaitStatus.ERRORED,
            name="test-agent",
            elapsed=30,
            error_message="progress_file_errored",
        )
        d = result.to_dict()
        assert d["error"] == "progress_file_errored"


class TestContractCheckResult:
    def test_defaults(self) -> None:
        result = ContractCheckResult(satisfied=False)
        assert result.satisfied is False
        assert result.status == "not_satisfied"
        assert result.message == ""
        assert result.recovery_action == "none"

    def test_from_json_satisfied(self) -> None:
        data = {
            "phase": "builder",
            "issue": 42,
            "status": "satisfied",
            "message": "PR #100 exists with loom:review-requested",
            "recovery_action": "none",
        }
        result = ContractCheckResult.from_json(data)
        assert result.satisfied is True
        assert result.status == "satisfied"
        assert "PR #100" in result.message

    def test_from_json_recovered(self) -> None:
        data = {
            "status": "recovered",
            "message": "Applied loom:curated label",
            "recovery_action": "apply_label",
        }
        result = ContractCheckResult.from_json(data)
        assert result.satisfied is True
        assert result.status == "recovered"
        assert result.recovery_action == "apply_label"

    def test_from_json_failed(self) -> None:
        data = {
            "status": "failed",
            "message": "No PR found",
        }
        result = ContractCheckResult.from_json(data)
        assert result.satisfied is False
        assert result.status == "failed"


class TestMonitorConfig:
    def test_defaults(self) -> None:
        config = MonitorConfig(name="test-agent")
        assert config.name == "test-agent"
        assert config.timeout == 3600
        assert config.poll_interval == 5
        assert config.issue is None
        assert config.task_id is None
        assert config.phase is None
        assert config.worktree is None
        assert config.pr_number is None
        assert config.idle_timeout == 60
        assert config.contract_interval == 90
        assert config.min_session_age == 10
        assert config.heartbeat_interval == 60

    def test_from_args_minimal(self) -> None:
        config = MonitorConfig.from_args(name="builder-issue-42")
        assert config.name == "builder-issue-42"
        assert config.timeout == 3600
        assert isinstance(config.stuck_config, StuckConfig)

    def test_from_args_full(self) -> None:
        config = MonitorConfig.from_args(
            name="builder-issue-42",
            timeout=1800,
            poll_interval=10,
            issue=42,
            task_id="abc123",
            phase="builder",
            worktree=".loom/worktrees/issue-42",
            pr_number=100,
            idle_timeout=30,
            contract_interval=60,
            min_session_age=5,
        )
        assert config.name == "builder-issue-42"
        assert config.timeout == 1800
        assert config.poll_interval == 10
        assert config.issue == 42
        assert config.task_id == "abc123"
        assert config.phase == "builder"
        assert config.worktree == ".loom/worktrees/issue-42"
        assert config.pr_number == 100
        assert config.idle_timeout == 30
        assert config.contract_interval == 60
        assert config.min_session_age == 5
