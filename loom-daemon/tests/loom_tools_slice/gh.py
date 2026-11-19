#!/usr/bin/python3
"""Stateful gh fixture for the workflow commands. Reject every unexpected mutation."""

import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
root = Path(__file__).parent
repo = "acme/role-probe"
prefix = f"repos/{repo}"
with (root / "calls").open("a") as log:
    print(json.dumps(args), file=log)


def reply(value, next_page=None):
    if "--include" in args or "-i" in args:
        sys.stdout.write("HTTP/2.0 200 OK\r\nContent-Type: application/json\r\n")
        if next_page:
            sys.stdout.write(f'Link: <{next_page}>; rel="next"\r\n')
        sys.stdout.write("\r\n")
    print(json.dumps(value))
    sys.exit(0)


def labels():
    return json.loads((root / "labels").read_text())


def save(names):
    (root / "labels").write_text(json.dumps(sorted(set(names))))


def objects(names):
    return [{"name": name} for name in names]


if args[:2] == ["pr", "list"]:
    reply([])
if args[:2] in (["issue", "edit"], ["issue", "comment"], ["pr", "comment"]):
    assert args[2] == "12" and args[args.index("--repo") + 1] == repo
    if args[1] == "edit":
        assert args[3:7] == [
            "--remove-label",
            "loom:blocked",
            "--add-label",
            "loom:curated",
        ]
        if (root / "fail-edit").exists():
            sys.exit(1)
        save([n for n in labels() if n != "loom:blocked"] + ["loom:curated"])
    else:
        with (root / "comments").open("a") as f:
            print(json.dumps(args[args.index("--body") + 1]), file=f)
    sys.exit(0)

assert args[0] == "api"
if "rate_limit" in args or any(a.startswith("installation/repositories") for a in args):
    sys.exit(1)
if "graphql" in args:
    reply(
        {
            "data": {
                "rateLimit": {"limit": 5000, "remaining": 4999, "cost": 1},
                "repository": {
                    "i12": {
                        "closedByPullRequestsReferences": {"totalCount": 0, "nodes": []}
                    }
                },
            }
        }
    )
endpoint = next(
    (a.removeprefix("https://api.github.com/") for a in args
     if a.startswith(("repos/", "https://api.github.com/repos/"))),
    "",
)
path, _, query = endpoint.partition("?")
paginated_ratification = (root / "paginated-ratification").exists()


def ratification_pages(second_page):
    assert "--jq" not in args
    if "--paginate" in args:
        reply(second_page)
    assert "--include" in args and "per_page=100" in query
    if "page=2" in query.split("&"):
        if (root / f"fail-{path.rsplit('/', 1)[1]}").exists():
            sys.exit(1)
        reply(second_page)
    reply([], f"https://api.github.com/{path}?per_page=100&page=2")

if path == prefix:
    if "--jq" in args:
        if os.getenv("PERMISSION") == "unknown":
            sys.exit(1)
        reply({"push": True} if os.getenv("PERMISSION") != "denied" else {"pull": True})
    reply({"archived": False})
comments = [json.loads(line) for line in (root / "comments").read_text().splitlines()]
if path == f"{prefix}/issues":
    reply(
        [
            {
                "number": 12,
                "title": "Dependent",
                "state": "open",
                "body": (root / "body").read_text(),
                "labels": objects(labels()),
                "user": {"login": "bot"},
                "comments": len(comments),
            }
        ]
        if "loom:blocked" in labels()
        else []
    )
if path == f"{prefix}/issues/10":
    reply({"number": 10, "state": "closed", "labels": []})
if path == f"{prefix}/issues/12":
    reply(
        {
            "number": 12,
            "body": (root / "body").read_text(),
            "user": {"login": "maintainer"},
            "author_association": "OWNER",
            "labels": objects(labels()),
        }
    )
if path == f"{prefix}/issues/12/comments":
    if paginated_ratification:
        ratification_pages([
            {
                "body": "<!-- champion:ratification-hold -->\n<!-- champion:hold-state head=abc -->",
                "user": {"login": "maintainer"},
                "author_association": "OWNER",
            }
        ])
    reply(
        [
            {"body": b, "user": {"login": "maintainer"}, "author_association": "OWNER"}
            for b in comments
        ]
    )
if path == f"{prefix}/issues/12/labels":
    if "-X" in args:
        assert args[args.index("-X") + 1] == "POST"
        save(
            labels()
            + [a.removeprefix("labels[]=") for a in args if a.startswith("labels[]=")]
        )
    elif (root / "unreadable").exists():
        sys.exit(1)
    reply(objects(labels()))
if path == f"{prefix}/issues/12/labels/points%3A2":
    assert "DELETE" in args
    if (root / "fail-edit").exists():
        sys.exit(1)
    save([n for n in labels() if n != "points:2"])
    sys.exit(0)
if path == f"{prefix}/pulls/12":
    reply({"head": {"sha": "abc"}, "base": {"ref": "main"}})
if path == f"{prefix}/pulls/12/files":
    if paginated_ratification:
        ratification_pages([{"filename": "spec/workflow.md", "status": "modified"}])
    reply([])
if path == f"{prefix}/contents/spec/workflow.md":
    assert query == "ref=abc"
    print("Ratified only by the operator.")
    sys.exit(0)
sys.exit(2)
