"""``loom-forge`` CLI -- forge-agnostic wrapper for shell scripts.

Provides a CLI interface that mirrors common ``gh issue/pr`` subcommands
but dispatches through the ``ForgeClient`` protocol, supporting both
GitHub and Gitea backends transparently.

Shell scripts replace ``gh issue/pr`` calls with ``loom-forge`` equivalents:

    # Before:
    gh issue list --label "loom:building" --state open --json number,title
    gh issue view 42 --json labels --jq '.labels[].name'
    gh issue edit 42 --remove-label "loom:building" --add-label "loom:issue"
    gh issue comment 42 --body "Recovery message"
    gh issue create --title "..." --body "..." --label "..."
    gh pr list --state open --json number,headRefName,body,labels
    gh auth status

    # After:
    loom-forge issue list --label "loom:building" --state open --json number,title
    loom-forge issue view 42 --json labels --jq '.labels[].name'
    loom-forge issue edit 42 --remove-label "loom:building" --add-label "loom:issue"
    loom-forge issue comment 42 --body "Recovery message"
    loom-forge issue create --title "..." --body "..." --label "..."
    loom-forge pr list --state open --json number,headRefName,body,labels
    loom-forge pr edit 123 --remove-label "loom:reviewing" --add-label "loom:review-requested"
    loom-forge auth status

For GitHub: dispatches to ``gh`` CLI (identical behavior).
For Gitea: translates to Gitea REST API via ``GiteaForge``.

Output format matches ``gh --json`` for backward compatibility.
"""

from __future__ import annotations

import json
import subprocess
import sys
from typing import Any, Sequence

from loom_tools.common.forge import (
    ForgeClient,
    ForgeIssue,
    ForgePullRequest,
    detect_forge,
    get_forge,
)


# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------


def _issue_to_dict(issue: ForgeIssue, fields: Sequence[str] | None = None) -> dict[str, Any]:
    """Convert a ForgeIssue to a dict matching ``gh --json`` output format."""
    data: dict[str, Any] = {
        "number": issue.number,
        "state": issue.state,
        "title": issue.title,
        "url": issue.url,
        "labels": [{"name": name} for name in issue.labels],
        "body": issue.body or "",
        "createdAt": "",  # Not available from ForgeIssue
        "updatedAt": "",  # Not available from ForgeIssue
    }
    if fields:
        data = {k: v for k, v in data.items() if k in fields}
    return data


def _pr_to_dict(pr: ForgePullRequest, fields: Sequence[str] | None = None) -> dict[str, Any]:
    """Convert a ForgePullRequest to a dict matching ``gh --json`` output format."""
    data: dict[str, Any] = {
        "number": pr.number,
        "state": pr.state,
        "title": pr.title,
        "url": pr.url,
        "labels": [{"name": name} for name in pr.labels],
        "headRefName": pr.head_branch or "",
        "body": pr.body or "",
    }
    if fields:
        data = {k: v for k, v in data.items() if k in fields}
    return data


def _jq_extract(data: Any, jq_expr: str) -> str:
    """Apply a simple jq-like expression to data.

    Supports common patterns used by shell scripts:
    - ``.labels[].name`` -> newline-separated label names
    - ``.state`` -> state string
    - ``.labels`` -> JSON array of labels

    For complex expressions, falls back to printing JSON.
    """
    if jq_expr == ".labels[].name":
        if isinstance(data, dict):
            labels = data.get("labels", [])
            if isinstance(labels, list):
                names = []
                for label in labels:
                    if isinstance(label, dict):
                        names.append(label.get("name", ""))
                    elif isinstance(label, str):
                        names.append(label)
                return "\n".join(names)
        return ""

    if jq_expr == ".state":
        if isinstance(data, dict):
            return str(data.get("state", ""))
        return ""

    # Generic single-field extraction: .fieldName
    if jq_expr.startswith(".") and jq_expr[1:].isidentifier():
        field = jq_expr[1:]
        if isinstance(data, dict) and field in data:
            val = data[field]
            if isinstance(val, (str, int, float, bool)):
                return str(val)
            return json.dumps(val)
        return ""

    # Fallback: return JSON
    return json.dumps(data)


def _output_json(data: Any) -> None:
    """Print data as JSON to stdout."""
    json.dump(data, sys.stdout)
    sys.stdout.write("\n")


# ---------------------------------------------------------------------------
# Argument parsing helpers
# ---------------------------------------------------------------------------


def _pop_flag(args: list[str], flag: str) -> str | None:
    """Pop a --flag value pair from args list, returning the value."""
    try:
        idx = args.index(flag)
    except ValueError:
        return None
    if idx + 1 >= len(args):
        return None
    value = args[idx + 1]
    del args[idx : idx + 2]
    return value


def _pop_all_flags(args: list[str], flag: str) -> list[str]:
    """Pop all occurrences of --flag value pairs, returning list of values."""
    values: list[str] = []
    while True:
        val = _pop_flag(args, flag)
        if val is None:
            break
        values.append(val)
    return values


def _pop_bool_flag(args: list[str], flag: str) -> bool:
    """Pop a boolean flag (no value) from args list."""
    try:
        idx = args.index(flag)
        del args[idx]
        return True
    except ValueError:
        return False


def _parse_json_fields(field_str: str) -> list[str]:
    """Parse comma-separated JSON field names."""
    return [f.strip() for f in field_str.split(",") if f.strip()]


# ---------------------------------------------------------------------------
# Command handlers
# ---------------------------------------------------------------------------


def _cmd_issue_list(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge issue list [--label X] [--state X] [--json fields] [--limit N]"""
    label = _pop_flag(args, "--label")
    state = _pop_flag(args, "--state") or "open"
    json_fields_str = _pop_flag(args, "--json")
    limit_str = _pop_flag(args, "--limit")

    labels = label.split(",") if label else None
    limit = int(limit_str) if limit_str else None
    json_fields = _parse_json_fields(json_fields_str) if json_fields_str else None

    issues = forge.list_issues(labels=labels, state=state, limit=limit)
    result = [_issue_to_dict(i, json_fields) for i in issues]
    _output_json(result)
    return 0


def _cmd_issue_view(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge issue view <number> [--json fields] [--jq expr]"""
    if not args:
        print("Error: issue number required", file=sys.stderr)
        return 1

    number = int(args.pop(0))
    json_fields_str = _pop_flag(args, "--json")
    jq_expr = _pop_flag(args, "--jq")

    json_fields = _parse_json_fields(json_fields_str) if json_fields_str else None

    issue = forge.get_issue(number)
    if issue is None:
        print(f"Error: issue #{number} not found", file=sys.stderr)
        return 1

    data = _issue_to_dict(issue, json_fields)

    if jq_expr:
        result = _jq_extract(data, jq_expr)
        if result:
            print(result)
    else:
        _output_json(data)

    return 0


def _cmd_issue_edit(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge issue edit <number> [--add-label X]... [--remove-label X]..."""
    if not args:
        print("Error: issue number required", file=sys.stderr)
        return 1

    number = int(args.pop(0))
    add_labels = _pop_all_flags(args, "--add-label")
    remove_labels = _pop_all_flags(args, "--remove-label")

    success = forge.transition_labels("issue", number, add=add_labels, remove=remove_labels)
    return 0 if success else 1


def _cmd_issue_comment(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge issue comment <number> --body <text>"""
    if not args:
        print("Error: issue number required", file=sys.stderr)
        return 1

    number = int(args.pop(0))
    body = _pop_flag(args, "--body")
    if body is None:
        print("Error: --body is required", file=sys.stderr)
        return 1

    success = forge.comment_on_issue(number, body)
    return 0 if success else 1


def _cmd_issue_create(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge issue create --title X --body X [--label X]... [--repo R]"""
    title = _pop_flag(args, "--title")
    body = _pop_flag(args, "--body")
    labels = _pop_all_flags(args, "--label")
    # --repo is consumed but ignored (forge is already bound to a repo)
    _pop_flag(args, "--repo")

    if not title:
        print("Error: --title is required", file=sys.stderr)
        return 1
    if body is None:
        body = ""

    issue = forge.create_issue(title, body, labels=labels or None)
    if issue is None:
        print("Error: failed to create issue", file=sys.stderr)
        return 1

    # Output the issue URL (matches gh behavior)
    print(issue.url)
    return 0


def _cmd_pr_list(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge pr list [--label X] [--state X] [--json fields] [--limit N]"""
    label = _pop_flag(args, "--label")
    state = _pop_flag(args, "--state") or "open"
    json_fields_str = _pop_flag(args, "--json")
    limit_str = _pop_flag(args, "--limit")

    labels = label.split(",") if label else None
    limit = int(limit_str) if limit_str else None
    json_fields = _parse_json_fields(json_fields_str) if json_fields_str else None

    prs = forge.list_pull_requests(labels=labels, state=state, limit=limit)
    result = [_pr_to_dict(pr, json_fields) for pr in prs]
    _output_json(result)
    return 0


def _cmd_pr_edit(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge pr edit <number> [--add-label X]... [--remove-label X]..."""
    if not args:
        print("Error: PR number required", file=sys.stderr)
        return 1

    number = int(args.pop(0))
    add_labels = _pop_all_flags(args, "--add-label")
    remove_labels = _pop_all_flags(args, "--remove-label")

    success = forge.transition_labels("pr", number, add=add_labels, remove=remove_labels)
    return 0 if success else 1


def _cmd_auth_status(forge: ForgeClient, args: list[str]) -> int:
    """Handle: loom-forge auth status

    For GitHub: runs ``gh auth status``.
    For Gitea: validates token by fetching repo metadata.
    """
    if forge.forge_type == "github":
        # Pass through to gh auth status
        result = subprocess.run(["gh", "auth", "status"], capture_output=True, text=True)
        if result.stdout:
            sys.stdout.write(result.stdout)
        if result.stderr:
            sys.stderr.write(result.stderr)
        return result.returncode

    # Gitea: try to get repo info as auth check
    nwo = forge.get_repo_nwo()
    if nwo is None:
        print("Error: cannot determine repository", file=sys.stderr)
        return 1

    # Try fetching the default branch as a lightweight auth check
    branch = forge.get_repo_default_branch()
    if branch is None:
        print(f"Error: authentication failed or repository {nwo} not accessible", file=sys.stderr)
        return 1

    print(f"Authenticated to {forge.forge_type} as configured user")
    print(f"Repository: {nwo}")
    return 0


# ---------------------------------------------------------------------------
# Command dispatch
# ---------------------------------------------------------------------------


_ISSUE_COMMANDS = {
    "list": _cmd_issue_list,
    "view": _cmd_issue_view,
    "edit": _cmd_issue_edit,
    "comment": _cmd_issue_comment,
    "create": _cmd_issue_create,
}

_PR_COMMANDS = {
    "list": _cmd_pr_list,
    "edit": _cmd_pr_edit,
}

_TOP_COMMANDS = {
    "issue": _ISSUE_COMMANDS,
    "pr": _PR_COMMANDS,
}


def _print_usage() -> None:
    """Print usage help."""
    print(
        "Usage: loom-forge <entity> <command> [args...]\n"
        "\n"
        "Forge-agnostic CLI for issue/PR operations.\n"
        "Detects GitHub or Gitea from git remote and dispatches accordingly.\n"
        "\n"
        "Entities:\n"
        "  issue    Issue operations (list, view, edit, comment, create)\n"
        "  pr       Pull request operations (list, edit)\n"
        "  auth     Authentication (status)\n"
        "\n"
        "Examples:\n"
        "  loom-forge issue list --label loom:building --state open --json number,title\n"
        "  loom-forge issue view 42 --json labels --jq '.labels[].name'\n"
        "  loom-forge issue edit 42 --remove-label loom:building --add-label loom:issue\n"
        "  loom-forge issue comment 42 --body 'Recovery message'\n"
        "  loom-forge issue create --title 'Title' --body 'Body' --label bug\n"
        "  loom-forge pr list --state open --json number,headRefName,body,labels\n"
        "  loom-forge pr edit 123 --remove-label loom:reviewing --add-label loom:review-requested\n"
        "  loom-forge auth status\n",
        file=sys.stderr,
    )


def main(argv: list[str] | None = None) -> int:
    """Entry point for the ``loom-forge`` CLI."""
    args = list(argv if argv is not None else sys.argv[1:])

    if args and args[0] == "--version":
        from importlib.metadata import version

        print(f"loom-forge {version('loom-tools')}")
        return 0

    if not args or args[0] in ("-h", "--help"):
        _print_usage()
        return 0 if args and args[0] in ("-h", "--help") else 1

    entity = args.pop(0)

    # Special case: auth status
    if entity == "auth":
        if args and args[0] == "status":
            args.pop(0)
            forge = get_forge()
            return _cmd_auth_status(forge, args)
        print("Error: unknown auth subcommand", file=sys.stderr)
        return 1

    commands = _TOP_COMMANDS.get(entity)
    if commands is None:
        print(f"Error: unknown entity '{entity}'", file=sys.stderr)
        _print_usage()
        return 1

    if not args:
        print(f"Error: subcommand required for '{entity}'", file=sys.stderr)
        return 1

    subcommand = args.pop(0)
    handler = commands.get(subcommand)
    if handler is None:
        print(f"Error: unknown {entity} subcommand '{subcommand}'", file=sys.stderr)
        return 1

    forge = get_forge()
    return handler(forge, args)


def cli_main() -> None:
    """Console script entry point."""
    sys.exit(main())
