"""Tests for budget exhaustion handling (issue #2201).

Covers:
- Per-error-class block thresholds in issue_failures.py
- Budget exhaustion detection in completions.py
- WIP preservation and failure recording in shepherds.py
- Decomposition trigger creation
- Cross-shepherd checkpoint continuity
- Exit code additions
"""

from __future__ import annotations

import json
import pathlib
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.common.issue_failures import (
    BACKOFF_BASE,
    ERROR_CLASS_BLOCK_THRESHOLDS,
    MAX_FAILURES_BEFORE_BLOCK,
    IssueFailureEntry,
    IssueFailureLog,
    load_failure_log,
    merge_into_daemon_state,
    record_failure,
    record_success,
)
from loom_tools.daemon_v2.actions.completions import (
    BUDGET_EXHAUSTION_PATTERNS,
    CompletionEntry,
    _check_budget_exhaustion,
    _record_persistent_failure,
    _trigger_decomposition,
    check_completions,
)
from loom_tools.daemon_v2.actions.shepherds import (
    _has_budget_exhaustion_warning,
    _has_existing_branch,
    _has_existing_checkpoint,
    _preserve_wip,
)
from loom_tools.shepherd.exit_codes import (
    EXIT_CODE_DESCRIPTIONS,
    ShepherdExitCode,
    describe_exit_code,
)


@pytest.fixture
def repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo with .loom directory."""
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    (tmp_path / ".loom" / "worktrees").mkdir()
    (tmp_path / ".loom" / "progress").mkdir()
    return tmp_path


# ── Exit Code ─────────────────────────────────────────────────


class TestBudgetExhaustedExitCode:
    def test_exit_code_value(self) -> None:
        assert ShepherdExitCode.BUDGET_EXHAUSTED == 8

    def test_exit_code_in_descriptions(self) -> None:
        assert ShepherdExitCode.BUDGET_EXHAUSTED in EXIT_CODE_DESCRIPTIONS

    def test_describe_exit_code(self) -> None:
        desc = describe_exit_code(8)
        assert "budget" in desc.lower() or "decomposition" in desc.lower()


# ── Per-Error-Class Thresholds ────────────────────────────────


class TestPerErrorClassThresholds:
    def test_budget_exhausted_threshold_is_2(self) -> None:
        assert ERROR_CLASS_BLOCK_THRESHOLDS["budget_exhausted"] == 2

    def test_default_threshold_unchanged(self) -> None:
        assert MAX_FAILURES_BEFORE_BLOCK == 5

    def test_budget_exhausted_blocks_at_2(self) -> None:
        entry = IssueFailureEntry(
            total_failures=2, error_class="budget_exhausted"
        )
        assert entry.should_auto_block is True
        assert entry.block_threshold == 2

    def test_budget_exhausted_not_blocked_at_1(self) -> None:
        entry = IssueFailureEntry(
            total_failures=1, error_class="budget_exhausted"
        )
        assert entry.should_auto_block is False

    def test_generic_failure_not_blocked_at_2(self) -> None:
        entry = IssueFailureEntry(
            total_failures=2, error_class="shepherd_failure"
        )
        assert entry.should_auto_block is False
        assert entry.block_threshold == MAX_FAILURES_BEFORE_BLOCK

    def test_generic_failure_blocks_at_5(self) -> None:
        entry = IssueFailureEntry(
            total_failures=5, error_class="shepherd_failure"
        )
        assert entry.should_auto_block is True

    def test_unknown_error_class_uses_default(self) -> None:
        entry = IssueFailureEntry(
            total_failures=4, error_class="some_new_error"
        )
        assert entry.block_threshold == MAX_FAILURES_BEFORE_BLOCK
        assert entry.should_auto_block is False

    def test_backoff_returns_negative_at_threshold(self) -> None:
        entry = IssueFailureEntry(
            total_failures=2, error_class="budget_exhausted"
        )
        assert entry.backoff_iterations() == -1

    def test_backoff_normal_for_first_budget_failure(self) -> None:
        entry = IssueFailureEntry(
            total_failures=1, error_class="budget_exhausted"
        )
        assert entry.backoff_iterations() == 0

    def test_record_failure_with_budget_exhausted(self, repo: pathlib.Path) -> None:
        entry = record_failure(
            repo, 42,
            error_class="budget_exhausted",
            phase="builder",
            details="Session ran out of budget",
        )
        assert entry.total_failures == 1
        assert entry.error_class == "budget_exhausted"

        entry = record_failure(
            repo, 42,
            error_class="budget_exhausted",
            phase="builder",
        )
        assert entry.total_failures == 2
        assert entry.should_auto_block is True

    def test_merge_uses_per_class_threshold(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="budget_exhausted")
        record_failure(repo, 42, error_class="budget_exhausted")

        retries: dict = {}
        result = merge_into_daemon_state(repo, retries)
        assert result["42"]["retry_exhausted"] is True

    def test_merge_generic_not_exhausted_at_2(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="shepherd_failure")
        record_failure(repo, 42, error_class="shepherd_failure")

        retries: dict = {}
        result = merge_into_daemon_state(repo, retries)
        assert "retry_exhausted" not in result.get("42", {})


# ── Budget Exhaustion Detection ───────────────────────────────


class TestBudgetExhaustionDetection:
    def test_detect_budget_milestone(self) -> None:
        milestones = [
            {"event": "budget_exhausted", "data": {}},
        ]
        assert _check_budget_exhaustion(milestones) is True

    def test_detect_budget_error_pattern(self) -> None:
        milestones = [
            {"event": "error", "data": {"error": "Session budget exceeded"}},
        ]
        assert _check_budget_exhaustion(milestones) is True

    def test_detect_token_limit_pattern(self) -> None:
        milestones = [
            {"event": "error", "data": {"error": "Token limit reached for this session"}},
        ]
        assert _check_budget_exhaustion(milestones) is True

    def test_detect_max_turns_pattern(self) -> None:
        milestones = [
            {"event": "error", "data": {"error": "Reached max turns in conversation"}},
        ]
        assert _check_budget_exhaustion(milestones) is True

    def test_no_budget_exhaustion(self) -> None:
        milestones = [
            {"event": "error", "data": {"error": "Build failed with exit code 1"}},
        ]
        assert _check_budget_exhaustion(milestones) is False

    def test_empty_milestones(self) -> None:
        assert _check_budget_exhaustion([]) is False

    def test_completion_entry_has_error_class(self) -> None:
        entry = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            error_class="budget_exhausted",
        )
        assert entry.error_class == "budget_exhausted"

    def test_completion_entry_default_error_class(self) -> None:
        entry = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
        )
        assert entry.error_class == "shepherd_failure"


# ── Snapshot Health Warning Detection ─────────────────────────


class TestSnapshotBudgetDetection:
    def test_has_budget_warning(self) -> None:
        snapshot = {
            "computed": {
                "health_warnings": [
                    {"code": "session_budget_low", "level": "warning", "message": "..."},
                ],
            },
        }
        assert _has_budget_exhaustion_warning(snapshot) is True

    def test_no_budget_warning(self) -> None:
        snapshot = {
            "computed": {
                "health_warnings": [
                    {"code": "stale_heartbeats", "level": "warning", "message": "..."},
                ],
            },
        }
        assert _has_budget_exhaustion_warning(snapshot) is False

    def test_empty_warnings(self) -> None:
        snapshot = {"computed": {"health_warnings": []}}
        assert _has_budget_exhaustion_warning(snapshot) is False

    def test_missing_computed(self) -> None:
        snapshot = {}
        assert _has_budget_exhaustion_warning(snapshot) is False


# ── WIP Preservation ──────────────────────────────────────────


class TestWipPreservation:
    def test_no_worktree_returns_false(self, repo: pathlib.Path) -> None:
        assert _preserve_wip(repo, 999) is False

    @patch("loom_tools.daemon_v2.actions.shepherds.subprocess")
    def test_preserves_uncommitted_changes(
        self, mock_subprocess: MagicMock, repo: pathlib.Path
    ) -> None:
        # Create worktree directory
        worktree = repo / ".loom" / "worktrees" / "issue-42"
        worktree.mkdir(parents=True)

        # Mock git status showing changes
        mock_status = MagicMock()
        mock_status.stdout = " M src/main.py\n"
        mock_status.returncode = 0

        # Mock git add, commit
        mock_add = MagicMock()
        mock_add.returncode = 0
        mock_commit = MagicMock()
        mock_commit.returncode = 0

        # Mock git rev-parse for branch name
        mock_branch = MagicMock()
        mock_branch.stdout = "feature/issue-42\n"
        mock_branch.returncode = 0

        # Mock git push
        mock_push = MagicMock()
        mock_push.returncode = 0

        mock_subprocess.run.side_effect = [
            mock_status, mock_add, mock_commit, mock_branch, mock_push
        ]

        result = _preserve_wip(repo, 42)
        assert result is True
        assert mock_subprocess.run.call_count == 5

    @patch("loom_tools.daemon_v2.actions.shepherds.subprocess")
    def test_no_uncommitted_changes_pushes_existing_commits(
        self, mock_subprocess: MagicMock, repo: pathlib.Path
    ) -> None:
        worktree = repo / ".loom" / "worktrees" / "issue-42"
        worktree.mkdir(parents=True)

        # Mock git status showing no changes
        mock_status = MagicMock()
        mock_status.stdout = ""
        mock_status.returncode = 0

        # Mock git rev-parse for branch name
        mock_branch = MagicMock()
        mock_branch.stdout = "feature/issue-42\n"
        mock_branch.returncode = 0

        # Mock git push succeeds (existing commits pushed)
        mock_push = MagicMock()
        mock_push.returncode = 0

        mock_subprocess.run.side_effect = [mock_status, mock_branch, mock_push]

        result = _preserve_wip(repo, 42)
        # Push succeeded, so WIP was preserved (existing commits pushed)
        assert result is True
        # Should not have called git add or git commit (no uncommitted changes)
        assert mock_subprocess.run.call_count == 3


# ── Cross-Shepherd Checkpoint Continuity ──────────────────────


class TestCheckpointContinuity:
    def test_has_existing_checkpoint(self, repo: pathlib.Path) -> None:
        worktree = repo / ".loom" / "worktrees" / "issue-42"
        worktree.mkdir(parents=True)
        (worktree / ".loom-checkpoint").write_text(
            json.dumps({"stage": "implementing", "issue": 42})
        )
        assert _has_existing_checkpoint(repo, 42) is True

    def test_no_existing_checkpoint(self, repo: pathlib.Path) -> None:
        assert _has_existing_checkpoint(repo, 42) is False

    def test_checkpoint_without_worktree(self, repo: pathlib.Path) -> None:
        assert _has_existing_checkpoint(repo, 999) is False

    @patch("loom_tools.daemon_v2.actions.shepherds.subprocess")
    def test_has_existing_remote_branch(
        self, mock_subprocess: MagicMock, repo: pathlib.Path
    ) -> None:
        mock_result = MagicMock()
        mock_result.stdout = "abc1234\trefs/heads/feature/issue-42\n"
        mock_result.returncode = 0
        mock_subprocess.run.return_value = mock_result

        assert _has_existing_branch(repo, 42) is True

    @patch("loom_tools.daemon_v2.actions.shepherds.subprocess")
    def test_no_existing_remote_branch(
        self, mock_subprocess: MagicMock, repo: pathlib.Path
    ) -> None:
        mock_result = MagicMock()
        mock_result.stdout = ""
        mock_result.returncode = 0
        mock_subprocess.run.return_value = mock_result

        assert _has_existing_branch(repo, 42) is False


# ── Decomposition Trigger ─────────────────────────────────────


class TestDecompositionTrigger:
    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_creates_architect_issue(self, mock_gh: MagicMock) -> None:
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_gh.return_value = mock_result

        entry = IssueFailureEntry(
            issue=42,
            total_failures=2,
            error_class="budget_exhausted",
            phase="builder",
            first_failure_at="2026-01-01T00:00:00Z",
            last_failure_at="2026-01-02T00:00:00Z",
        )

        _trigger_decomposition(42, entry)

        mock_gh.assert_called_once()
        call_args = mock_gh.call_args[0][0]
        assert "issue" in call_args
        assert "create" in call_args
        assert "--label" in call_args
        assert "loom:architect" in call_args

    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_decomposition_body_references_issue(self, mock_gh: MagicMock) -> None:
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_gh.return_value = mock_result

        entry = IssueFailureEntry(
            issue=42,
            total_failures=2,
            error_class="budget_exhausted",
            phase="builder",
        )

        _trigger_decomposition(42, entry)

        call_args = mock_gh.call_args[0][0]
        # Find the --body arg
        body_idx = call_args.index("--body") + 1
        body = call_args[body_idx]
        assert "#42" in body
        assert "budget exhaustion" in body.lower() or "budget" in body.lower()

    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_decomposition_handles_failure(self, mock_gh: MagicMock) -> None:
        mock_gh.side_effect = Exception("Network error")

        entry = IssueFailureEntry(
            issue=42,
            total_failures=2,
            error_class="budget_exhausted",
        )

        # Should not raise
        _trigger_decomposition(42, entry)


# ── Integration: Record Persistent Failure with Budget Exhaustion ──


class TestRecordPersistentFailureIntegration:
    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_budget_exhausted_triggers_block_and_decomposition(
        self, mock_gh: MagicMock, repo: pathlib.Path
    ) -> None:
        """After 2 budget_exhausted failures, issue is blocked and decomposition triggered."""
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_gh.return_value = mock_result

        from loom_tools.daemon_v2.config import DaemonConfig
        from loom_tools.daemon_v2.context import DaemonContext
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        # Record first failure
        record_failure(repo, 42, error_class="budget_exhausted", phase="builder")

        ctx = DaemonContext(
            config=DaemonConfig(),
            repo_root=repo,
            state=DaemonState(
                shepherds={
                    "shepherd-1": ShepherdEntry(
                        status="idle", last_phase="builder"
                    )
                }
            ),
            snapshot={},
        )

        # Second failure via completion handler
        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            success=False,
            error_class="budget_exhausted",
        )
        _record_persistent_failure(ctx, completion)

        # Verify auto-block was triggered (gh issue edit call)
        block_calls = [
            c for c in mock_gh.call_args_list
            if "loom:blocked" in str(c)
        ]
        assert len(block_calls) >= 1

        # Verify decomposition issue was created
        create_calls = [
            c for c in mock_gh.call_args_list
            if "create" in str(c) and "loom:architect" in str(c)
        ]
        assert len(create_calls) >= 1

    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_generic_failure_no_decomposition_at_2(
        self, mock_gh: MagicMock, repo: pathlib.Path
    ) -> None:
        """Generic shepherd_failure at 2 failures does NOT trigger decomposition."""
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_gh.return_value = mock_result

        from loom_tools.daemon_v2.config import DaemonConfig
        from loom_tools.daemon_v2.context import DaemonContext
        from loom_tools.models.daemon_state import DaemonState, ShepherdEntry

        record_failure(repo, 42, error_class="shepherd_failure", phase="builder")

        ctx = DaemonContext(
            config=DaemonConfig(),
            repo_root=repo,
            state=DaemonState(
                shepherds={
                    "shepherd-1": ShepherdEntry(
                        status="idle", last_phase="builder"
                    )
                }
            ),
            snapshot={},
        )

        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            success=False,
            error_class="shepherd_failure",
        )
        _record_persistent_failure(ctx, completion)

        # Should NOT have block or decomposition calls (only 2 failures, threshold is 5)
        block_calls = [
            c for c in mock_gh.call_args_list
            if "loom:blocked" in str(c)
        ]
        assert len(block_calls) == 0

        create_calls = [
            c for c in mock_gh.call_args_list
            if "create" in str(c) and "loom:architect" in str(c)
        ]
        assert len(create_calls) == 0
