"""Tests for daemon_v2 spinning issue escalation."""

from __future__ import annotations

from unittest import mock

from loom_tools.daemon_v2.actions.spinning import escalate_spinning_issues


class TestEscalateSpinningIssues:
    """Tests for escalate_spinning_issues action."""

    def test_empty_list(self) -> None:
        assert escalate_spinning_issues([]) == 0

    @mock.patch("loom_tools.daemon_v2.actions.spinning.gh_run")
    def test_escalates_with_linked_issue(self, mock_gh: mock.MagicMock) -> None:
        mock_gh.return_value = mock.MagicMock(returncode=0)
        spinning = [
            {"pr_number": 100, "review_cycles": 5, "linked_issue": 42},
        ]
        result = escalate_spinning_issues(spinning)
        assert result == 1

        # Verify gh calls: comment on PR, close PR, edit issue labels, comment on issue
        calls = mock_gh.call_args_list
        assert len(calls) == 4

        # PR comment
        assert calls[0][0][0][:3] == ["pr", "comment", "100"]
        # PR close
        assert calls[1][0][0] == ["pr", "close", "100"]
        # Issue label edit
        assert calls[2][0][0][:3] == ["issue", "edit", "42"]
        assert "--add-label" in calls[2][0][0]
        assert "loom:blocked" in calls[2][0][0]
        # Issue comment
        assert calls[3][0][0][:3] == ["issue", "comment", "42"]

    @mock.patch("loom_tools.daemon_v2.actions.spinning.gh_run")
    def test_no_linked_issue_still_closes_pr(self, mock_gh: mock.MagicMock) -> None:
        mock_gh.return_value = mock.MagicMock(returncode=0)
        spinning = [
            {"pr_number": 100, "review_cycles": 5, "linked_issue": None},
        ]
        result = escalate_spinning_issues(spinning)
        assert result == 0  # No issue to escalate

        # Still comments and closes the PR
        calls = mock_gh.call_args_list
        assert len(calls) == 2  # comment + close (no issue operations)

    @mock.patch("loom_tools.daemon_v2.actions.spinning.gh_run")
    def test_multiple_spinning_prs(self, mock_gh: mock.MagicMock) -> None:
        mock_gh.return_value = mock.MagicMock(returncode=0)
        spinning = [
            {"pr_number": 100, "review_cycles": 5, "linked_issue": 42},
            {"pr_number": 200, "review_cycles": 3, "linked_issue": 50},
        ]
        result = escalate_spinning_issues(spinning)
        assert result == 2

    @mock.patch("loom_tools.daemon_v2.actions.spinning.gh_run")
    def test_gh_failure_handled_gracefully(self, mock_gh: mock.MagicMock) -> None:
        mock_gh.side_effect = Exception("API error")
        spinning = [
            {"pr_number": 100, "review_cycles": 5, "linked_issue": 42},
        ]
        # Should not raise
        result = escalate_spinning_issues(spinning)
        assert result == 0  # Failed to block the issue

    def test_missing_pr_number_skipped(self) -> None:
        spinning = [
            {"review_cycles": 5, "linked_issue": 42},
        ]
        result = escalate_spinning_issues(spinning)
        assert result == 0
