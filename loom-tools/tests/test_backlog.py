"""Tests for the loom-backlog command (issue-failures.json port).

After Phase 3.1.2 (#3391), ``loom-backlog`` reads exclusively from
``.loom/issue-failures.json`` and uses the ``loom:blocked`` label as the
escalation deduplication signal. These tests exercise that contract:

- No fixture writes ``daemon-state.json`` — its absence is the point.
- The forge ``gh issue list --label loom:blocked`` call is patched out
  so tests do not require network access.
"""

from __future__ import annotations

import json
import pathlib
from typing import Any
from unittest import mock

import pytest

from loom_tools.backlog import cmd_list, cmd_prune, get_retry_policy, main


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _write_failures(
    loom_dir: pathlib.Path,
    entries: dict[str, dict],
) -> None:
    """Write a minimal ``issue-failures.json`` with the given entries."""
    payload = {
        "entries": entries,
        "updated_at": "2026-06-02T12:00:00Z",
    }
    loom_dir.mkdir(parents=True, exist_ok=True)
    (loom_dir / "issue-failures.json").write_text(json.dumps(payload))


def _failure_entry(
    issue: int,
    *,
    total_failures: int,
    error_class: str,
    phase: str = "builder",
    details: str = "",
) -> dict[str, Any]:
    """Build a single ``IssueFailureEntry``-shaped dict."""
    return {
        "issue": issue,
        "total_failures": total_failures,
        "error_class": error_class,
        "phase": phase,
        "details": details,
        "first_failure_at": "2026-01-20T10:00:00Z",
        "last_failure_at": "2026-01-22T10:00:00Z",
    }


def _patch_escalated(monkeypatch: pytest.MonkeyPatch, numbers: set[int]) -> mock.MagicMock:
    """Patch ``gh issue list --label loom:blocked`` to return *numbers*.

    Returns the underlying ``gh_run`` mock so tests can assert on
    follow-up calls (label apply, comment).
    """
    payload = json.dumps([{"number": n} for n in sorted(numbers)])
    list_result = mock.MagicMock(returncode=0, stdout=payload)
    edit_result = mock.MagicMock(returncode=0, stdout="")
    comment_result = mock.MagicMock(returncode=0, stdout="")

    def fake_gh(cmd, **kwargs):  # noqa: ANN001 - test helper
        if len(cmd) >= 2 and cmd[0] == "issue":
            if cmd[1] == "list":
                return list_result
            if cmd[1] == "edit":
                return edit_result
            if cmd[1] == "comment":
                return comment_result
        return mock.MagicMock(returncode=0, stdout="")

    m = mock.MagicMock(side_effect=fake_gh)
    monkeypatch.setattr("loom_tools.backlog.gh_run", m)
    return m


# ---------------------------------------------------------------------------
# get_retry_policy()
# ---------------------------------------------------------------------------


class TestRetryPolicy:
    def test_known_class_uses_table(self) -> None:
        p = get_retry_policy("builder_test_failure")
        assert p.max_retries == 2
        assert p.cooldown == 21600
        assert p.escalate is True

    def test_transient_class_does_not_escalate(self) -> None:
        p = get_retry_policy("mcp_infrastructure_failure")
        assert p.escalate is False

    def test_doctor_class_immediate_escalation(self) -> None:
        p = get_retry_policy("doctor_exhausted")
        assert p.max_retries == 0
        assert p.escalate is True

    def test_unknown_class_uses_default(self) -> None:
        p = get_retry_policy("totally_unknown_class")
        assert p.max_retries == 3
        assert p.cooldown == 1800
        assert p.escalate is True


# ---------------------------------------------------------------------------
# cmd_list()
# ---------------------------------------------------------------------------


class TestCmdList:
    def test_no_failures_file(
        self, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
    ) -> None:
        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir()
        result = cmd_list(tmp_path)
        out = capsys.readouterr().out
        assert result == 1
        assert "No issue-failures.json" in out

    def test_empty_entries(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _write_failures(tmp_path / ".loom", {})
        _patch_escalated(monkeypatch, set())

        result = cmd_list(tmp_path)
        out = capsys.readouterr().out
        assert result == 0
        assert "No tracked issue failures" in out

    def test_lists_tracked_failures(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _write_failures(tmp_path / ".loom", {
            "42": _failure_entry(42, total_failures=2, error_class="builder_test_failure"),
            "99": _failure_entry(99, total_failures=1, error_class="mcp_infrastructure_failure"),
        })
        _patch_escalated(monkeypatch, set())

        result = cmd_list(tmp_path)
        out = capsys.readouterr().out
        assert result == 0
        assert "#42" in out
        assert "#99" in out
        assert "builder_test_failure" in out
        assert "mcp_infrastructure_failure" in out
        # #42: 2 failures vs max_retries=2 -> exhausted
        # #99: 1 failure vs max_retries=5 -> retryable
        assert "exhausted" in out
        assert "retryable" in out

    def test_escalated_label_overrides_exhausted(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _write_failures(tmp_path / ".loom", {
            "42": _failure_entry(42, total_failures=5, error_class="builder_test_failure"),
        })
        _patch_escalated(monkeypatch, {42})

        result = cmd_list(tmp_path)
        out = capsys.readouterr().out
        assert result == 0
        assert "escalated" in out

    def test_does_not_read_daemon_state(
        self,
        tmp_path: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """The ported CLI must not pull data from daemon-state.json at all."""
        loom_dir = tmp_path / ".loom"
        _write_failures(loom_dir, {
            "42": _failure_entry(42, total_failures=2, error_class="builder_test_failure"),
        })
        # Write a daemon-state.json that, if read, would yield different data.
        (loom_dir / "daemon-state.json").write_text(json.dumps({
            "running": True,
            "blocked_issue_retries": {
                "9999": {
                    "retry_count": 99,
                    "error_class": "builder_unknown_failure",
                    "retry_exhausted": True,
                    "escalated_to_human": False,
                    "last_blocked_phase": "builder",
                    "last_blocked_details": "",
                },
            },
            "needs_human_input": [],
        }))
        _patch_escalated(monkeypatch, set())

        import io
        buf = io.StringIO()
        with mock.patch("sys.stdout", buf):
            result = cmd_list(tmp_path)
        out = buf.getvalue()

        assert result == 0
        assert "#42" in out
        assert "#9999" not in out


# ---------------------------------------------------------------------------
# cmd_prune()
# ---------------------------------------------------------------------------


class TestCmdPrune:
    def test_no_failures_file(
        self, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
    ) -> None:
        (tmp_path / ".loom").mkdir()
        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        out = capsys.readouterr().out
        assert result == 1
        assert "No issue-failures.json" in out

    def test_dry_run_makes_no_mutations(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _write_failures(tmp_path / ".loom", {
            "42": _failure_entry(42, total_failures=2, error_class="builder_test_failure"),
        })
        m = _patch_escalated(monkeypatch, set())

        result = cmd_prune(tmp_path, dry_run=True, add_comment=False)
        out = capsys.readouterr().out
        assert result == 0
        assert "dry-run" in out

        # Only the read-side 'issue list' query should have happened.
        invoked_subcommands = [call.args[0][1] for call in m.call_args_list]
        assert invoked_subcommands == ["list"]

    def test_prune_escalates_exhausted_issue(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _write_failures(tmp_path / ".loom", {
            "42": _failure_entry(42, total_failures=2, error_class="builder_test_failure"),
        })
        m = _patch_escalated(monkeypatch, set())

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        out = capsys.readouterr().out
        assert result == 0
        assert "Escalated 1 issue" in out

        label_calls = [
            call for call in m.call_args_list
            if call.args[0][:2] == ["issue", "edit"]
        ]
        assert len(label_calls) == 1
        assert "loom:blocked" in label_calls[0].args[0]
        assert "42" in label_calls[0].args[0]

    def test_prune_skips_already_escalated(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _write_failures(tmp_path / ".loom", {
            "42": _failure_entry(42, total_failures=2, error_class="builder_test_failure"),
        })
        m = _patch_escalated(monkeypatch, {42})

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        out = capsys.readouterr().out
        assert result == 0
        assert "Nothing to escalate" in out
        edit_calls = [
            call for call in m.call_args_list
            if call.args[0][:2] == ["issue", "edit"]
        ]
        assert edit_calls == []

    def test_prune_does_not_escalate_transient_errors(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """mcp_infrastructure_failure exhausted but escalate=False -> no escalation."""
        _write_failures(tmp_path / ".loom", {
            "200": _failure_entry(200, total_failures=5, error_class="mcp_infrastructure_failure"),
        })
        m = _patch_escalated(monkeypatch, set())

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        out = capsys.readouterr().out
        assert result == 0
        assert "Nothing to escalate" in out
        edit_calls = [
            call for call in m.call_args_list
            if call.args[0][:2] == ["issue", "edit"]
        ]
        assert edit_calls == []

    def test_prune_with_comment_posts_comment(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _write_failures(tmp_path / ".loom", {
            "42": _failure_entry(42, total_failures=3, error_class="builder_unknown_failure"),
        })
        m = _patch_escalated(monkeypatch, set())

        result = cmd_prune(tmp_path, dry_run=False, add_comment=True)
        assert result == 0

        comment_calls = [
            call for call in m.call_args_list
            if call.args[0][:2] == ["issue", "comment"]
        ]
        assert len(comment_calls) == 1
        assert "42" in comment_calls[0].args[0]

    def test_doctor_exhausted_immediate_escalation(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """doctor_exhausted with 0 retries is immediately escalated (max_retries=0)."""
        _write_failures(tmp_path / ".loom", {
            "300": _failure_entry(300, total_failures=0, error_class="doctor_exhausted"),
        })
        m = _patch_escalated(monkeypatch, set())

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        assert result == 0

        edit_calls = [
            call for call in m.call_args_list
            if call.args[0][:2] == ["issue", "edit"]
        ]
        assert len(edit_calls) == 1
        assert "300" in edit_calls[0].args[0]

    def test_prune_does_not_read_daemon_state(
        self,
        tmp_path: pathlib.Path,
        capsys: pytest.CaptureFixture[str],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        loom_dir = tmp_path / ".loom"
        _write_failures(loom_dir, {})
        # Write a daemon-state.json that, if read, would queue an escalation.
        (loom_dir / "daemon-state.json").write_text(json.dumps({
            "running": True,
            "blocked_issue_retries": {
                "9999": {
                    "retry_count": 99,
                    "error_class": "builder_unknown_failure",
                    "retry_exhausted": True,
                    "escalated_to_human": False,
                    "last_blocked_phase": "builder",
                    "last_blocked_details": "",
                },
            },
            "needs_human_input": [],
        }))
        m = _patch_escalated(monkeypatch, set())

        result = cmd_prune(tmp_path, dry_run=False, add_comment=False)
        assert result == 0
        edit_calls = [
            call for call in m.call_args_list
            if call.args[0][:2] == ["issue", "edit"]
        ]
        assert edit_calls == []


# ---------------------------------------------------------------------------
# main()
# ---------------------------------------------------------------------------


class TestMain:
    def test_no_subcommand_returns_1(self) -> None:
        result = main([])
        assert result == 1

    def test_unknown_subcommand_raises_system_exit(self) -> None:
        with pytest.raises(SystemExit):
            main(["unknown"])
