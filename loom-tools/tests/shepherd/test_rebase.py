"""Tests for the rebase phase."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.shepherd.config import ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import PhaseStatus
from loom_tools.shepherd.phases.rebase import RebasePhase, _is_pr_mergeable, _is_pr_merged


@pytest.fixture
def mock_context(tmp_path: Path) -> MagicMock:
    """Create a mock ShepherdContext with a real worktree directory."""
    ctx = MagicMock(spec=ShepherdContext)
    ctx.config = ShepherdConfig(issue=42)
    ctx.repo_root = Path("/fake/repo")
    ctx.worktree_path = tmp_path / "worktree"
    ctx.worktree_path.mkdir()
    ctx.pr_number = 100
    ctx.label_cache = MagicMock()
    ctx.check_shutdown.return_value = False
    ctx.report_milestone = MagicMock(return_value=True)
    return ctx


class TestRebasePhase:
    """Tests for RebasePhase."""

    def test_branch_up_to_date_skips(self, mock_context: MagicMock) -> None:
        """When branch is not behind origin/main, phase should be SKIPPED."""
        phase = RebasePhase()

        with patch(
            "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=False
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SKIPPED
        assert "up to date" in result.message

    def test_rebase_succeeds_and_pushes(self, mock_context: MagicMock) -> None:
        """When rebase succeeds, should force-push and return SUCCESS."""
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(True, ""),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.force_push_branch",
                return_value=True,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.remove_pr_label"
            ) as mock_remove,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "rebased" in result.message
        mock_remove.assert_called_once_with(100, "loom:merge-conflict", mock_context.repo_root)
        mock_context.label_cache.invalidate_pr.assert_called_with(100)

    def test_rebase_succeeds_removes_merge_conflict_label(
        self, mock_context: MagicMock
    ) -> None:
        """Successful rebase should remove loom:merge-conflict label from PR."""
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(True, ""),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.force_push_branch",
                return_value=True,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.remove_pr_label"
            ) as mock_remove,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        mock_remove.assert_called_once_with(100, "loom:merge-conflict", mock_context.repo_root)

    def test_rebase_conflict_fails_and_labels(self, mock_context: MagicMock) -> None:
        """When rebase has conflicts, should FAIL and apply loom:merge-conflict."""
        phase = RebasePhase()
        conflict_detail = "Conflicting files:\nsrc/main.py\nsrc/utils.py"

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(False, conflict_detail),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
                return_value=False,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.add_pr_label"
            ) as mock_add,
            patch("subprocess.run") as mock_subprocess,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "merge_conflict" in result.data.get("reason", "")
        mock_add.assert_called_once_with(100, "loom:merge-conflict", mock_context.repo_root)
        # Should also post a diagnostic comment
        mock_subprocess.assert_called_once()
        call_args = mock_subprocess.call_args
        assert "gh" in call_args[0][0][0]
        assert "pr" in call_args[0][0][1]
        assert "comment" in call_args[0][0][2]

    def test_no_worktree_fails_gracefully(self, mock_context: MagicMock) -> None:
        """When no worktree path is available and PR is not CLEAN, should FAIL gracefully."""
        mock_context.worktree_path = None
        phase = RebasePhase()

        with patch(
            "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
            return_value=False,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "no worktree" in result.message
        assert result.data.get("reason") == "no_worktree"

    def test_nonexistent_worktree_fails_gracefully(
        self, mock_context: MagicMock
    ) -> None:
        """When worktree path doesn't exist on disk and PR is not CLEAN, should FAIL gracefully."""
        mock_context.worktree_path = Path("/nonexistent/path")
        phase = RebasePhase()

        with patch(
            "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
            return_value=False,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "no worktree" in result.message

    def test_no_worktree_but_pr_is_clean_on_github_succeeds(
        self, mock_context: MagicMock
    ) -> None:
        """When no worktree is available but GitHub says PR is CLEAN, should SUCCESS."""
        mock_context.worktree_path = None
        phase = RebasePhase()

        with patch(
            "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
            return_value=True,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "mergeable on GitHub" in result.message
        assert result.data.get("reason") == "github_mergeable_fallback"

    def test_nonexistent_worktree_but_pr_is_clean_on_github_succeeds(
        self, mock_context: MagicMock
    ) -> None:
        """When worktree dir is missing but GitHub says PR is CLEAN, should SUCCESS."""
        mock_context.worktree_path = Path("/nonexistent/path")
        phase = RebasePhase()

        with patch(
            "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
            return_value=True,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "mergeable on GitHub" in result.message
        assert result.data.get("reason") == "github_mergeable_fallback"

    def test_no_worktree_no_pr_number_fails_without_github_check(
        self, mock_context: MagicMock
    ) -> None:
        """When no worktree and no PR number, should FAIL without calling _is_pr_mergeable."""
        mock_context.worktree_path = None
        mock_context.pr_number = None
        phase = RebasePhase()

        with patch(
            "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
        ) as mock_mergeable:
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("reason") == "no_worktree"
        mock_mergeable.assert_not_called()

    def test_shutdown_signal(self, mock_context: MagicMock) -> None:
        """When shutdown is signaled, should return SHUTDOWN."""
        mock_context.check_shutdown.return_value = True
        phase = RebasePhase()

        result = phase.run(mock_context)

        assert result.status == PhaseStatus.SHUTDOWN
        assert result.is_shutdown

    def test_force_push_failure(self, mock_context: MagicMock) -> None:
        """When rebase succeeds but force-push fails, should return FAILED."""
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(True, ""),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.force_push_branch",
                return_value=False,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase._is_pr_merged",
                return_value=False,
            ),
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert "push_failed" in result.data.get("reason", "")

    def test_force_push_failure_but_pr_already_merged(
        self, mock_context: MagicMock
    ) -> None:
        """When force-push fails but PR is already merged, should return SUCCESS."""
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(True, ""),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.force_push_branch",
                return_value=False,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase._is_pr_merged",
                return_value=True,
            ),
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "already merged" in result.message

    def test_force_push_failure_no_pr_number_skips_merged_check(
        self, mock_context: MagicMock
    ) -> None:
        """When force-push fails with no PR number, should FAIL without merged check."""
        mock_context.pr_number = None
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(True, ""),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.force_push_branch",
                return_value=False,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase._is_pr_merged",
            ) as mock_merged,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        mock_merged.assert_not_called()

    def test_no_pr_number_skips_label_operations(
        self, mock_context: MagicMock
    ) -> None:
        """When pr_number is None, should skip label add/remove but still rebase."""
        mock_context.pr_number = None
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(True, ""),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.force_push_branch",
                return_value=True,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.remove_pr_label"
            ) as mock_remove,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        mock_remove.assert_not_called()

    def test_conflict_no_pr_number_skips_label_and_comment(
        self, mock_context: MagicMock
    ) -> None:
        """When rebase fails and pr_number is None, should not add labels or comment."""
        mock_context.pr_number = None
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(False, "conflicts"),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.add_pr_label"
            ) as mock_add,
            patch("subprocess.run") as mock_subprocess,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        mock_add.assert_not_called()
        mock_subprocess.assert_not_called()

    def test_phase_name_is_rebase(self) -> None:
        """Phase name should be 'rebase'."""
        phase = RebasePhase()
        assert phase.phase_name == "rebase"

    def test_reports_heartbeat_before_rebase(self, mock_context: MagicMock) -> None:
        """Should report a heartbeat milestone before attempting the rebase."""
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(True, ""),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.force_push_branch",
                return_value=True,
            ),
            patch("loom_tools.shepherd.phases.rebase.remove_pr_label"),
        ):
            phase.run(mock_context)

        mock_context.report_milestone.assert_called_with(
            "heartbeat", action="rebasing onto origin/main"
        )


class TestRebasePhaseValidate:
    """Tests for RebasePhase.validate()."""

    def test_validate_passes_when_not_behind(self, mock_context: MagicMock) -> None:
        """validate() should return True when branch is not behind."""
        phase = RebasePhase()
        with patch(
            "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=False
        ):
            assert phase.validate(mock_context) is True

    def test_validate_fails_when_behind(self, mock_context: MagicMock) -> None:
        """validate() should return False when branch is still behind."""
        phase = RebasePhase()
        with patch(
            "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
        ):
            assert phase.validate(mock_context) is False

    def test_validate_fails_without_worktree(self, mock_context: MagicMock) -> None:
        """validate() should return False when no worktree is available."""
        mock_context.worktree_path = None
        phase = RebasePhase()
        assert phase.validate(mock_context) is False


class TestIsPrMerged:
    """Tests for the _is_pr_merged helper."""

    def test_returns_true_when_merged(self) -> None:
        """Should return True when gh reports MERGED."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(stdout="MERGED\n")
            assert _is_pr_merged(100, "/fake/repo") is True

    def test_returns_false_when_open(self) -> None:
        """Should return False when gh reports OPEN."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(stdout="OPEN\n")
            assert _is_pr_merged(100, "/fake/repo") is False

    def test_returns_false_when_closed(self) -> None:
        """Should return False when gh reports CLOSED."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(stdout="CLOSED\n")
            assert _is_pr_merged(100, "/fake/repo") is False

    def test_returns_false_on_empty_output(self) -> None:
        """Should return False when gh returns empty output."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(stdout="")
            assert _is_pr_merged(100, "/fake/repo") is False


class TestIsPrMergeable:
    """Tests for the _is_pr_mergeable helper."""

    def test_returns_true_when_mergeable_and_clean(self) -> None:
        """Should return True when GitHub reports MERGEABLE + CLEAN."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout='{"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}',
            )
            assert _is_pr_mergeable(100, "/fake/repo") is True

    def test_returns_false_when_conflicting(self) -> None:
        """Should return False when GitHub reports CONFLICTING."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout='{"mergeable":"CONFLICTING","mergeStateStatus":"DIRTY"}',
            )
            assert _is_pr_mergeable(100, "/fake/repo") is False

    def test_returns_false_when_mergeable_but_not_clean(self) -> None:
        """Should return False when mergeable but merge state is not CLEAN."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout='{"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED"}',
            )
            assert _is_pr_mergeable(100, "/fake/repo") is False

    def test_returns_false_when_unknown(self) -> None:
        """Should return False when GitHub reports UNKNOWN merge status."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout='{"mergeable":"UNKNOWN","mergeStateStatus":"UNKNOWN"}',
            )
            assert _is_pr_mergeable(100, "/fake/repo") is False

    def test_returns_false_on_gh_failure(self) -> None:
        """Should return False when gh command fails."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=1, stdout="")
            assert _is_pr_mergeable(100, "/fake/repo") is False

    def test_returns_false_on_invalid_json(self) -> None:
        """Should return False when gh returns invalid JSON."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout="not json")
            assert _is_pr_mergeable(100, "/fake/repo") is False

    def test_returns_false_on_empty_output(self) -> None:
        """Should return False when gh returns empty output."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout="")
            assert _is_pr_mergeable(100, "/fake/repo") is False


class TestRebaseGitHubFallback:
    """Tests for the GitHub mergeable fallback when local rebase fails."""

    def test_rebase_fails_but_github_says_mergeable(
        self, mock_context: MagicMock
    ) -> None:
        """When local rebase fails but GitHub says MERGEABLE/CLEAN, should SUCCESS."""
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(False, "Rebase failed (unknown conflicts)"),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
                return_value=True,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.add_pr_label"
            ) as mock_add_label,
            patch("subprocess.run") as mock_subprocess,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.SUCCESS
        assert "mergeable on GitHub" in result.message
        assert result.data.get("reason") == "github_mergeable_fallback"
        # Should NOT apply merge-conflict label or post comment
        mock_add_label.assert_not_called()
        mock_subprocess.assert_not_called()

    def test_rebase_fails_and_github_says_conflicting(
        self, mock_context: MagicMock
    ) -> None:
        """When both local rebase and GitHub agree on conflicts, should FAIL."""
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(False, "Conflicting files:\nsrc/main.py"),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
                return_value=False,
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.add_pr_label"
            ) as mock_add_label,
            patch("subprocess.run"),
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        assert result.data.get("reason") == "merge_conflict"
        mock_add_label.assert_called_once_with(
            100, "loom:merge-conflict", mock_context.repo_root
        )

    def test_rebase_fails_no_pr_number_skips_github_check(
        self, mock_context: MagicMock
    ) -> None:
        """When rebase fails with no PR number, should skip GitHub check and FAIL."""
        mock_context.pr_number = None
        phase = RebasePhase()

        with (
            patch(
                "loom_tools.shepherd.phases.rebase.is_branch_behind", return_value=True
            ),
            patch(
                "loom_tools.shepherd.phases.rebase.attempt_rebase",
                return_value=(False, "conflicts"),
            ),
            patch(
                "loom_tools.shepherd.phases.rebase._is_pr_mergeable",
            ) as mock_mergeable,
        ):
            result = phase.run(mock_context)

        assert result.status == PhaseStatus.FAILED
        mock_mergeable.assert_not_called()
