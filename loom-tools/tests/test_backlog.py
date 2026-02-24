"""Tests for the loom-backlog command."""

from __future__ import annotations

import json
import pathlib
from unittest import mock

import pytest

from loom_tools.backlog import cmd_list, cmd_prune, main
from loom_tools.models.daemon_state import BlockedIssueRetry, DaemonState


def _write_state(loom_dir: pathlib.Path, blocked_retries: dict) -> None:
    """Write a minimal daemon-state.json with the given blocked_issue_retries."""
    state = {
        "running": False,
        "blocked_issue_retries": blocked_retries,
        "needs_human_input": [],
    }
    loom_dir.mkdir(parents=True, exist_ok=True)
    (loom_dir / "daemon-state.json").write_text(json.dumps(state))


class TestCmdList:
    def test_no_state_file(self, tmp_path: pathlib.Path, capsys) -> None:
        repo = tmp_path / "repo"
        repo.mkdir()
        (repo / ".git").mkdir()
        (repo / ".loom").mkdir()

        with mock.patch("loom_tools.backlog.find_repo_root", return_value=repo):
            result = cmd_list(repo)

        assert result == 1

    def test_empty_retries(self, tmp_path: pathlib.Path, capsys) -> None:
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {})

        result = cmd_list(tmp_path)
        out = capsys.readouterr().out
        assert result == 0
        assert "No blocked issues" in out

    def test_lists_blocked_issues(self, tmp_path: pathlib.Path, capsys) -> None:
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {
            "42": {
                "retry_count": 2,
                "error_class": "builder_test_failure",
                "retry_exhausted": False,
                "last_blocked_phase": "builder",
                "last_blocked_details": "",
            },
            "99": {
                "retry_count": 1,
                "error_class": "mcp_infrastructure_failure",
                "retry_exhausted": False,
                "last_blocked_phase": "builder",
                "last_blocked_details": "",
            },
        })

        result = cmd_list(tmp_path)
        out = capsys.readouterr().out
        assert result == 0
        assert "#42" in out
        assert "#99" in out
        assert "builder_test_failure" in out
        assert "mcp_infrastructure_failure" in out


class TestCmdPrune:
    def test_dry_run_makes_no_changes(self, tmp_path: pathlib.Path, capsys) -> None:
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {
            "42": {
                "retry_count": 2,
                "error_class": "builder_test_failure",
                "retry_exhausted": False,
                "escalated_to_human": False,
                "last_blocked_phase": "builder",
                "last_blocked_details": "",
            },
        })

        result = cmd_prune(tmp_path, dry_run=True, add_comment=False)
        out = capsys.readouterr().out
        assert result == 0
        assert "dry-run" in out

        # State should be unchanged
        state = json.loads((loom_dir / "daemon-state.json").read_text())
        entry = state["blocked_issue_retries"]["42"]
        assert entry.get("escalated_to_human", False) is False

    def test_prune_escalates_exhausted_issue(self, tmp_path: pathlib.Path, capsys) -> None:
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {
            "42": {
                "retry_count": 2,
                "error_class": "builder_test_failure",
                "retry_exhausted": False,
                "escalated_to_human": False,
                "last_blocked_phase": "builder",
                "last_blocked_details": "",
            },
        })

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        assert result == 0

        state = json.loads((loom_dir / "daemon-state.json").read_text())
        entry = state["blocked_issue_retries"]["42"]
        assert entry.get("escalated_to_human") is True
        assert len(state["needs_human_input"]) == 1
        assert state["needs_human_input"][0]["issue"] == 42

    def test_prune_skips_already_escalated(self, tmp_path: pathlib.Path, capsys) -> None:
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {
            "42": {
                "retry_count": 2,
                "error_class": "builder_test_failure",
                "retry_exhausted": False,
                "escalated_to_human": True,  # already escalated
                "last_blocked_phase": "builder",
                "last_blocked_details": "",
            },
        })

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        out = capsys.readouterr().out
        assert result == 0
        assert "Nothing to escalate" in out

    def test_prune_does_not_escalate_transient_errors(self, tmp_path: pathlib.Path, capsys) -> None:
        """mcp_infrastructure_failure exhausted but escalate=False → no escalation."""
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {
            "200": {
                "retry_count": 5,
                "error_class": "mcp_infrastructure_failure",
                "retry_exhausted": False,
                "escalated_to_human": False,
                "last_blocked_phase": "builder",
                "last_blocked_details": "",
            },
        })

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        out = capsys.readouterr().out
        assert result == 0
        assert "Nothing to escalate" in out

        state = json.loads((loom_dir / "daemon-state.json").read_text())
        assert len(state["needs_human_input"]) == 0

    @mock.patch("loom_tools.backlog.gh_run")
    def test_prune_with_comment(
        self, mock_gh: mock.MagicMock, tmp_path: pathlib.Path, capsys
    ) -> None:
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {
            "42": {
                "retry_count": 3,
                "error_class": "builder_unknown_failure",
                "retry_exhausted": False,
                "escalated_to_human": False,
                "last_blocked_phase": "builder",
                "last_blocked_details": "",
            },
        })
        mock_gh.return_value = mock.MagicMock(returncode=0)

        result = cmd_prune(tmp_path, dry_run=False, add_comment=True)
        assert result == 0

        # Verify gh comment was called
        mock_gh.assert_called_once()
        call_args = mock_gh.call_args[0][0]
        assert "comment" in call_args
        assert "42" in call_args

    def test_doctor_exhausted_immediate_escalation(self, tmp_path: pathlib.Path, capsys) -> None:
        """doctor_exhausted with 0 retries is immediately escalated."""
        loom_dir = tmp_path / ".loom"
        _write_state(loom_dir, {
            "300": {
                "retry_count": 0,
                "error_class": "doctor_exhausted",
                "retry_exhausted": False,
                "escalated_to_human": False,
                "last_blocked_phase": "doctor",
                "last_blocked_details": "",
            },
        })

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        assert result == 0

        state = json.loads((loom_dir / "daemon-state.json").read_text())
        assert state["blocked_issue_retries"]["300"].get("escalated_to_human") is True
        assert len(state["needs_human_input"]) == 1


class TestMain:
    def test_no_subcommand_returns_1(self) -> None:
        result = main([])
        assert result == 1

    def test_unknown_subcommand_raises_system_exit(self) -> None:
        with pytest.raises(SystemExit):
            main(["unknown"])
