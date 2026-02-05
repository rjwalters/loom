"""Tests for shepherd configuration."""

from __future__ import annotations

import os
from unittest.mock import patch

import pytest

from loom_tools.common.config import env_int
from loom_tools.shepherd.config import (
    ExecutionMode,
    Phase,
    QualityGateLevel,
    QualityGates,
    ShepherdConfig,
    _generate_task_id,
    _parse_quality_gate_level,
)


class TestTaskIdGeneration:
    """Test task ID generation."""

    def test_task_id_length(self) -> None:
        """Task ID should be 7 characters."""
        task_id = _generate_task_id()
        assert len(task_id) == 7

    def test_task_id_is_hex(self) -> None:
        """Task ID should be lowercase hex."""
        task_id = _generate_task_id()
        assert all(c in "0123456789abcdef" for c in task_id)

    def test_task_id_uniqueness(self) -> None:
        """Task IDs should be unique."""
        ids = [_generate_task_id() for _ in range(100)]
        assert len(set(ids)) == 100


class TestEnvInt:
    """Test environment variable integer parsing (via common.config)."""

    def test_returns_default_when_not_set(self) -> None:
        """Should return default when env var not set."""
        assert env_int("NONEXISTENT_VAR_12345", 42) == 42

    def test_returns_env_value_when_set(self) -> None:
        """Should return env value when set."""
        with patch.dict(os.environ, {"TEST_VAR": "100"}):
            assert env_int("TEST_VAR", 42) == 100

    def test_returns_default_on_invalid(self) -> None:
        """Should return default when env var is not a valid int."""
        with patch.dict(os.environ, {"TEST_VAR": "not_an_int"}):
            assert env_int("TEST_VAR", 42) == 42


class TestPhase:
    """Test Phase enum."""

    def test_phase_values(self) -> None:
        """Phases should have expected string values."""
        assert Phase.CURATOR.value == "curator"
        assert Phase.APPROVAL.value == "approval"
        assert Phase.BUILDER.value == "builder"
        assert Phase.JUDGE.value == "judge"
        assert Phase.DOCTOR.value == "doctor"
        assert Phase.MERGE.value == "merge"


class TestExecutionMode:
    """Test ExecutionMode enum."""

    def test_mode_values(self) -> None:
        """Modes should have expected string values."""
        assert ExecutionMode.DEFAULT.value == "default"
        assert ExecutionMode.FORCE_MERGE.value == "force-merge"
        assert ExecutionMode.NORMAL.value == "normal"


class TestShepherdConfig:
    """Test ShepherdConfig dataclass."""

    def test_required_issue(self) -> None:
        """Config should require issue number."""
        config = ShepherdConfig(issue=42)
        assert config.issue == 42

    def test_default_mode(self) -> None:
        """Default mode should be DEFAULT."""
        config = ShepherdConfig(issue=42)
        assert config.mode == ExecutionMode.DEFAULT

    def test_is_force_mode_false_by_default(self) -> None:
        """is_force_mode should be False by default."""
        config = ShepherdConfig(issue=42)
        assert config.is_force_mode is False

    def test_is_force_mode_true_when_force_merge(self) -> None:
        """is_force_mode should be True for FORCE_MERGE."""
        config = ShepherdConfig(issue=42, mode=ExecutionMode.FORCE_MERGE)
        assert config.is_force_mode is True

    def test_should_auto_approve_true_for_default(self) -> None:
        """should_auto_approve should be True for DEFAULT mode."""
        config = ShepherdConfig(issue=42)
        assert config.should_auto_approve is True

    def test_should_auto_approve_true_for_force_merge(self) -> None:
        """should_auto_approve should be True for FORCE_MERGE mode."""
        config = ShepherdConfig(issue=42, mode=ExecutionMode.FORCE_MERGE)
        assert config.should_auto_approve is True

    def test_should_auto_approve_false_for_normal(self) -> None:
        """should_auto_approve should be False for NORMAL (deprecated) mode."""
        config = ShepherdConfig(issue=42, mode=ExecutionMode.NORMAL)
        assert config.should_auto_approve is False

    def test_auto_generated_task_id(self) -> None:
        """Task ID should be auto-generated."""
        config = ShepherdConfig(issue=42)
        assert len(config.task_id) == 7

    def test_explicit_task_id(self) -> None:
        """Task ID can be explicitly set."""
        config = ShepherdConfig(issue=42, task_id="abc1234")
        assert config.task_id == "abc1234"

    def test_should_skip_phase_no_start_from(self) -> None:
        """No phases should be skipped when start_from is None."""
        config = ShepherdConfig(issue=42)
        assert config.should_skip_phase(Phase.CURATOR) is False
        assert config.should_skip_phase(Phase.BUILDER) is False
        assert config.should_skip_phase(Phase.JUDGE) is False
        assert config.should_skip_phase(Phase.MERGE) is False

    def test_should_skip_phase_from_builder(self) -> None:
        """Curator should be skipped when starting from builder."""
        config = ShepherdConfig(issue=42, start_from=Phase.BUILDER)
        assert config.should_skip_phase(Phase.CURATOR) is True
        assert config.should_skip_phase(Phase.BUILDER) is False
        assert config.should_skip_phase(Phase.JUDGE) is False
        assert config.should_skip_phase(Phase.MERGE) is False

    def test_should_skip_phase_from_judge(self) -> None:
        """Curator and builder should be skipped when starting from judge."""
        config = ShepherdConfig(issue=42, start_from=Phase.JUDGE)
        assert config.should_skip_phase(Phase.CURATOR) is True
        assert config.should_skip_phase(Phase.BUILDER) is True
        assert config.should_skip_phase(Phase.JUDGE) is False
        assert config.should_skip_phase(Phase.MERGE) is False

    def test_should_skip_phase_from_merge(self) -> None:
        """All phases before merge should be skipped when starting from merge."""
        config = ShepherdConfig(issue=42, start_from=Phase.MERGE)
        assert config.should_skip_phase(Phase.CURATOR) is True
        assert config.should_skip_phase(Phase.BUILDER) is True
        assert config.should_skip_phase(Phase.JUDGE) is True
        assert config.should_skip_phase(Phase.MERGE) is False

    def test_get_phase_timeout(self) -> None:
        """Phase timeouts should be returned correctly."""
        config = ShepherdConfig(
            issue=42,
            curator_timeout=100,
            approval_timeout=150,
            builder_timeout=200,
            judge_timeout=300,
            doctor_timeout=400,
        )
        assert config.get_phase_timeout(Phase.CURATOR) == 100
        assert config.get_phase_timeout(Phase.APPROVAL) == 150
        assert config.get_phase_timeout(Phase.BUILDER) == 200
        assert config.get_phase_timeout(Phase.JUDGE) == 300
        assert config.get_phase_timeout(Phase.DOCTOR) == 400

    def test_default_timeouts(self) -> None:
        """Default timeouts should be set from environment or defaults."""
        config = ShepherdConfig(issue=42)
        # Note: Timeouts set high to avoid killing agents mid-work (see issue #2001)
        assert config.curator_timeout == 3600
        assert config.approval_timeout == 1800
        assert config.builder_timeout == 14400
        assert config.judge_timeout == 3600
        assert config.doctor_timeout == 3600
        assert config.poll_interval == 5

    def test_default_retry_limits(self) -> None:
        """Default retry limits should be set."""
        config = ShepherdConfig(issue=42)
        assert config.doctor_max_retries == 3
        assert config.judge_max_retries == 1
        assert config.stuck_max_retries == 1

    def test_judge_max_retries_env_override(self) -> None:
        """LOOM_JUDGE_MAX_RETRIES env var should override default."""
        with patch.dict(os.environ, {"LOOM_JUDGE_MAX_RETRIES": "3"}):
            config = ShepherdConfig(issue=42)
            assert config.judge_max_retries == 3

    def test_judge_max_retries_explicit(self) -> None:
        """judge_max_retries can be explicitly set."""
        config = ShepherdConfig(issue=42, judge_max_retries=5)
        assert config.judge_max_retries == 5

    def test_test_fix_max_retries_default(self) -> None:
        """test_fix_max_retries should default to 2."""
        config = ShepherdConfig(issue=42)
        assert config.test_fix_max_retries == 2

    def test_test_fix_max_retries_env_override(self) -> None:
        """LOOM_TEST_FIX_MAX_RETRIES env var should override default."""
        with patch.dict(os.environ, {"LOOM_TEST_FIX_MAX_RETRIES": "5"}):
            config = ShepherdConfig(issue=42)
            assert config.test_fix_max_retries == 5

    def test_worktree_marker_file(self) -> None:
        """Worktree marker file should have default value."""
        config = ShepherdConfig(issue=42)
        assert config.worktree_marker_file == ".loom-in-use"
