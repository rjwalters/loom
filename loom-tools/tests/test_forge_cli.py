"""Tests for loom_tools.forge_cli module.

Tests cover the CLI argument parsing, command dispatch, output formatting,
and jq expression handling.
"""

from __future__ import annotations

import json
from typing import Any, Sequence
from unittest import mock

import pytest

from loom_tools.common.forge import (
    EntityType,
    ForgeCIStatus,
    ForgeIssue,
    ForgePullRequest,
)
from loom_tools.forge_cli import (
    _issue_to_dict,
    _jq_extract,
    _parse_json_fields,
    _pop_all_flags,
    _pop_bool_flag,
    _pop_flag,
    _pr_to_dict,
    main,
)


# ===========================================================================
# Conversion helper tests
# ===========================================================================


class TestIssueToDictConversion:
    """Tests for _issue_to_dict."""

    def test_full_conversion(self) -> None:
        issue = ForgeIssue(
            number=42, state="OPEN", title="Test issue",
            url="https://example.com/42", labels=["bug", "loom:issue"],
            body="Issue body",
        )
        result = _issue_to_dict(issue)
        assert result["number"] == 42
        assert result["state"] == "OPEN"
        assert result["title"] == "Test issue"
        assert result["labels"] == [{"name": "bug"}, {"name": "loom:issue"}]
        assert result["body"] == "Issue body"

    def test_field_filtering(self) -> None:
        issue = ForgeIssue(
            number=42, state="OPEN", title="Test",
            url="https://example.com/42",
        )
        result = _issue_to_dict(issue, fields=["number", "title"])
        assert set(result.keys()) == {"number", "title"}
        assert result["number"] == 42


class TestPrToDictConversion:
    """Tests for _pr_to_dict."""

    def test_full_conversion(self) -> None:
        pr = ForgePullRequest(
            number=10, state="OPEN", title="Test PR",
            url="https://example.com/pull/10", labels=["loom:pr"],
            head_branch="feature/issue-42", body="PR body",
        )
        result = _pr_to_dict(pr)
        assert result["number"] == 10
        assert result["headRefName"] == "feature/issue-42"
        assert result["labels"] == [{"name": "loom:pr"}]

    def test_field_filtering(self) -> None:
        pr = ForgePullRequest(
            number=10, state="OPEN", title="Test",
            url="https://example.com/pull/10",
        )
        result = _pr_to_dict(pr, fields=["number", "headRefName"])
        assert set(result.keys()) == {"number", "headRefName"}


# ===========================================================================
# jq expression tests
# ===========================================================================


class TestJqExtract:
    """Tests for _jq_extract."""

    def test_labels_name_extraction(self) -> None:
        data = {"labels": [{"name": "bug"}, {"name": "loom:issue"}]}
        result = _jq_extract(data, ".labels[].name")
        assert result == "bug\nloom:issue"

    def test_labels_name_empty(self) -> None:
        data = {"labels": []}
        result = _jq_extract(data, ".labels[].name")
        assert result == ""

    def test_state_extraction(self) -> None:
        data = {"state": "CLOSED"}
        result = _jq_extract(data, ".state")
        assert result == "CLOSED"

    def test_generic_field(self) -> None:
        data = {"title": "My Issue"}
        result = _jq_extract(data, ".title")
        assert result == "My Issue"

    def test_generic_field_missing(self) -> None:
        data = {"number": 1}
        result = _jq_extract(data, ".title")
        assert result == ""


# ===========================================================================
# Argument parsing tests
# ===========================================================================


class TestArgParsing:
    """Tests for argument parsing helpers."""

    def test_pop_flag(self) -> None:
        args = ["--label", "bug", "--state", "open"]
        val = _pop_flag(args, "--label")
        assert val == "bug"
        assert args == ["--state", "open"]

    def test_pop_flag_missing(self) -> None:
        args = ["--state", "open"]
        val = _pop_flag(args, "--label")
        assert val is None
        assert args == ["--state", "open"]

    def test_pop_all_flags(self) -> None:
        args = ["--add-label", "a", "--add-label", "b", "--other"]
        vals = _pop_all_flags(args, "--add-label")
        assert vals == ["a", "b"]
        assert args == ["--other"]

    def test_pop_bool_flag(self) -> None:
        args = ["--verbose", "--state", "open"]
        val = _pop_bool_flag(args, "--verbose")
        assert val is True
        assert args == ["--state", "open"]

    def test_parse_json_fields(self) -> None:
        assert _parse_json_fields("number,title,labels") == ["number", "title", "labels"]
        assert _parse_json_fields("number") == ["number"]


# ===========================================================================
# FakeForge for integration tests
# ===========================================================================


class FakeForge:
    """Minimal ForgeClient implementation for testing."""

    def __init__(self) -> None:
        self.issues: dict[int, ForgeIssue] = {
            1: ForgeIssue(
                number=1, state="OPEN", title="Test issue",
                url="https://example.com/1", labels=["bug", "loom:building"],
                body="Test body",
            ),
            2: ForgeIssue(
                number=2, state="CLOSED", title="Done issue",
                url="https://example.com/2", labels=["loom:issue"],
            ),
        }
        self.prs: dict[int, ForgePullRequest] = {
            10: ForgePullRequest(
                number=10, state="OPEN", title="Test PR",
                url="https://example.com/pull/10", labels=["loom:review-requested"],
                head_branch="feature/issue-1", body="Closes #1",
            ),
        }
        self.comments: list[tuple[int, str]] = []
        self.label_ops: list[tuple[str, int, list[str], list[str]]] = []
        self.created_issues: list[dict[str, Any]] = []

    @property
    def forge_type(self) -> str:
        return "test"

    def get_issue(self, number: int) -> ForgeIssue | None:
        return self.issues.get(number)

    def list_issues(
        self, *, labels: Sequence[str] | None = None,
        state: str = "open", limit: int | None = None,
    ) -> list[ForgeIssue]:
        results = list(self.issues.values())
        if state != "all":
            results = [i for i in results if i.state.lower() == state.lower()]
        if labels:
            results = [
                i for i in results
                if all(l in i.labels for l in labels)
            ]
        if limit:
            results = results[:limit]
        return results

    def create_issue(
        self, title: str, body: str,
        labels: Sequence[str] | None = None,
    ) -> ForgeIssue | None:
        num = max(self.issues.keys()) + 1
        issue = ForgeIssue(
            number=num, state="OPEN", title=title,
            url=f"https://example.com/{num}",
            labels=list(labels or []), body=body,
        )
        self.issues[num] = issue
        self.created_issues.append({"title": title, "body": body, "labels": labels})
        return issue

    def close_issue(self, number: int) -> bool:
        return True

    def comment_on_issue(self, number: int, body: str) -> bool:
        self.comments.append((number, body))
        return True

    def list_pull_requests(
        self, *, labels: Sequence[str] | None = None,
        state: str = "open", head: str | None = None,
        search: str | None = None, limit: int | None = None,
    ) -> list[ForgePullRequest]:
        results = list(self.prs.values())
        if state != "all":
            results = [p for p in results if p.state.lower() == state.lower()]
        return results

    def transition_labels(
        self, entity_type: EntityType, number: int,
        add: Sequence[str] | None = None,
        remove: Sequence[str] | None = None,
    ) -> bool:
        self.label_ops.append((entity_type, number, list(add or []), list(remove or [])))
        return True

    def get_repo_nwo(self) -> str | None:
        return "test/repo"

    def get_repo_default_branch(self) -> str | None:
        return "main"


# ===========================================================================
# Integration tests via main()
# ===========================================================================


class TestMainIssueList:
    """Tests for 'loom-forge issue list'."""

    def test_issue_list_basic(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "list", "--state", "open", "--json", "number,title"])
        assert rc == 0
        output = json.loads(capsys.readouterr().out)
        assert isinstance(output, list)
        assert len(output) == 1  # Only issue #1 is OPEN
        assert output[0]["number"] == 1

    def test_issue_list_with_label_filter(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "list", "--label", "bug", "--json", "number"])
        assert rc == 0
        output = json.loads(capsys.readouterr().out)
        assert len(output) == 1
        assert output[0]["number"] == 1


class TestMainIssueView:
    """Tests for 'loom-forge issue view'."""

    def test_issue_view_json(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "view", "1", "--json", "number,state"])
        assert rc == 0
        output = json.loads(capsys.readouterr().out)
        assert output["number"] == 1
        assert output["state"] == "OPEN"

    def test_issue_view_jq_labels(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "view", "1", "--json", "labels", "--jq", ".labels[].name"])
        assert rc == 0
        output = capsys.readouterr().out.strip()
        assert "bug" in output
        assert "loom:building" in output

    def test_issue_view_jq_state(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "view", "2", "--json", "state", "--jq", ".state"])
        assert rc == 0
        output = capsys.readouterr().out.strip()
        assert output == "CLOSED"

    def test_issue_view_not_found(self) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "view", "999"])
        assert rc == 1


class TestMainIssueEdit:
    """Tests for 'loom-forge issue edit'."""

    def test_issue_edit_labels(self) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main([
                "issue", "edit", "1",
                "--remove-label", "loom:building",
                "--add-label", "loom:issue",
            ])
        assert rc == 0
        assert len(forge.label_ops) == 1
        entity, num, add, remove = forge.label_ops[0]
        assert entity == "issue"
        assert num == 1
        assert add == ["loom:issue"]
        assert remove == ["loom:building"]


class TestMainIssueComment:
    """Tests for 'loom-forge issue comment'."""

    def test_issue_comment(self) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "comment", "1", "--body", "Recovery message"])
        assert rc == 0
        assert forge.comments == [(1, "Recovery message")]

    def test_issue_comment_no_body(self) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "comment", "1"])
        assert rc == 1


class TestMainIssueCreate:
    """Tests for 'loom-forge issue create'."""

    def test_issue_create(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main([
                "issue", "create",
                "--title", "New issue",
                "--body", "Description",
                "--label", "improvement",
            ])
        assert rc == 0
        assert len(forge.created_issues) == 1
        assert forge.created_issues[0]["title"] == "New issue"
        output = capsys.readouterr().out.strip()
        assert "example.com" in output


class TestMainPrList:
    """Tests for 'loom-forge pr list'."""

    def test_pr_list(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["pr", "list", "--state", "open", "--json", "number,headRefName,body,labels"])
        assert rc == 0
        output = json.loads(capsys.readouterr().out)
        assert len(output) == 1
        assert output[0]["number"] == 10
        assert output[0]["headRefName"] == "feature/issue-1"


class TestMainPrEdit:
    """Tests for 'loom-forge pr edit'."""

    def test_pr_edit_labels(self) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main([
                "pr", "edit", "10",
                "--remove-label", "loom:reviewing",
                "--add-label", "loom:review-requested",
            ])
        assert rc == 0
        assert len(forge.label_ops) == 1
        entity, num, add, remove = forge.label_ops[0]
        assert entity == "pr"
        assert num == 10
        assert add == ["loom:review-requested"]
        assert remove == ["loom:reviewing"]

    def test_pr_edit_no_number(self) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["pr", "edit"])
        assert rc == 1


class TestMainAuthStatus:
    """Tests for 'loom-forge auth status'."""

    def test_auth_status_non_github(self, capsys: pytest.CaptureFixture[str]) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["auth", "status"])
        assert rc == 0
        output = capsys.readouterr().out
        assert "Authenticated" in output


class TestMainHelp:
    """Tests for help and error handling."""

    def test_no_args(self) -> None:
        rc = main([])
        assert rc == 1

    def test_help_flag(self) -> None:
        rc = main(["--help"])
        assert rc == 0

    def test_version_flag(self, capsys: pytest.CaptureFixture[str]) -> None:
        rc = main(["--version"])
        assert rc == 0
        out = capsys.readouterr().out
        assert out.startswith("loom-forge ")

    def test_unknown_entity(self) -> None:
        rc = main(["unknown", "list"])
        assert rc == 1

    def test_unknown_subcommand(self) -> None:
        forge = FakeForge()
        with mock.patch("loom_tools.forge_cli.get_forge", return_value=forge):
            rc = main(["issue", "unknown"])
        assert rc == 1
