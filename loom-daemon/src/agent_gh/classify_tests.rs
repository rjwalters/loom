//! The routing table (#10331). Rows are real agent call shapes: the top
//! `gh.caller=claude` operations named in the issue (`issue view`, `pr view`,
//! `pr list`, `api …/comments`, `repo view`), the read samples 2am's
//! `appread` classifier is tested on, and the `gh` invocations the role
//! prompts under `defaults/.claude/commands/loom/` issue most. Every
//! mutation, every `api` call and every unrecognised shape must be
//! Passthrough.

use super::{classify, is_slug, Entity, Route};

/// Minimal quote-aware split, enough for the table below.
fn argv(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, ' ') => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

const VIEWS: &[(&str, Entity)] = &[
    ("issue view 10331 --json title,body", Entity::Issue),
    ("issue view 42 --json labels --jq '.labels[].name'", Entity::Issue),
    ("issue view 42 --json labels -q '.labels[].name'", Entity::Issue),
    ("issue view 42 --json labels,state", Entity::Issue),
    ("issue view --json state 42", Entity::Issue),
    ("issue view 42 --json=state,updatedAt", Entity::Issue),
    ("issue view 7 -R rjwalters/loom --json state", Entity::Issue),
    ("issue view 7 --repo=rjwalters/loom --json number,title,url", Entity::Issue),
    ("issue view 9 --json body --jq .body", Entity::Issue),
    ("pr view 42 --json labels,state,isDraft", Entity::Pr),
    ("pr view 10051 --json headRefName,baseRefName", Entity::Pr),
    ("pr view 7 -R rjwalters/loom --json state", Entity::Pr),
    ("pr view 7 --json mergedAt --jq '.mergedAt != null'", Entity::Pr),
];

/// `(argv, entity, served argv tail)`.
const LISTS: &[(&str, Entity, &str)] = &[
    (
        "issue list --label loom:issue --state open --json number,title",
        Entity::Issue,
        "list --label loom:issue --state open --json number,title --limit 30",
    ),
    (
        "issue list --label=loom:curated --limit 50 --json number",
        Entity::Issue,
        "list --label=loom:curated --limit 50 --json number",
    ),
    (
        "issue list -l loom:building --json number,updatedAt --jq '.[].number'",
        Entity::Issue,
        "list -l loom:building --json number,updatedAt --jq .[].number --limit 30",
    ),
    (
        "issue list --state closed -L 5 --json number,title,state",
        Entity::Issue,
        "list --state closed -L 5 --json number,state,title",
    ),
    (
        "issue list --label loom:issue --json=title,number,title",
        Entity::Issue,
        "list --label loom:issue --json=number,title --limit 30",
    ),
    (
        "pr list --label loom:review-requested --json number,title",
        Entity::Pr,
        "list --label loom:review-requested --json number,title --limit 30",
    ),
    (
        "pr list --label=loom:pr --state open --json number,createdAt -q '.[].number'",
        Entity::Pr,
        "list --label=loom:pr --state open --json createdAt,number -q .[].number --limit 30",
    ),
];

const PASSTHROUGH: &[&str] = &[
    // Mutations.
    "issue edit 42 --remove-label loom:issue --add-label loom:building",
    "issue comment 42 --body-file /tmp/x",
    "issue close 5 --reason 'not planned'",
    "issue create --title t --body b --label loom:triage",
    "issue reopen https://github.com/o/r/issues/9",
    "issue delete 9 --yes",
    "issue lock 9",
    "issue transfer 9 o/other",
    "pr edit 7 --add-label loom:pr",
    "pr comment 123 --body 'LGTM'",
    "pr merge 7 --squash",
    "pr create --label loom:review-requested --body 'Closes #1'",
    "pr review --approve",
    "pr review 7 --request-changes -b no",
    "pr ready 9",
    "pr close 9",
    "label create loom:new --color FFFFFF",
    "run rerun 123",
    "repo edit --enable-auto-merge",
    // Every `api` call (ETag `api` serving is a later slice), reads included.
    "api repos/rjwalters/loom/issues/10331/comments",
    "api repos/{owner}/{repo}/issues/1/comments --paginate --jq '.[].body'",
    "api -X GET repos/o/r/pulls/5/reviews",
    "api rate_limit",
    "api -X POST repos/o/r/issues/5/comments -f body=hi",
    "api repos/o/r/issues/5/labels -F labels[]=x",
    "api --method PATCH repos/o/r/pulls/8 --input body.json",
    "api graphql -f query='mutation { addComment }'",
    "api graphql -f query='{ viewer { login } }'",
    // Reads the ETag modules do not reproduce.
    "pr checks 42 --watch",
    "pr checks 42 --watch --fail-fast",
    "pr checks 42 --watch -i 30",
    "pr checks 42 --required",
    "pr checks 42 --web",
    "pr checks 42 -w",
    "pr checks 42 --json name,bucket --jq '.[] | select(.bucket==\"fail\")'",
    "pr checks 42 --json name -q .",
    "pr checks 42 --json name --template '{{.}}'",
    "pr checks 42 --json name,workflow",
    "pr checks 42 --json event",
    "pr checks feature/issue-42",
    "pr checks https://github.com/o/r/pull/42",
    "pr checks",
    "pr checks 42 -R github.com/o/r",
    "pr checks 42 --help",
    "issue checks 42",
    "pr diff 42",
    "pr diff 42 --name-only",
    "pr status",
    "run view 123456 --log-failed",
    "run list --branch main --limit 5 --json status",
    "repo view --json nameWithOwner -q .nameWithOwner",
    "search issues loom",
    "status",
    "auth status",
    "auth token",
    "--version",
    "",
    "issue",
    "issue view 42",
    "issue view 100 --comments",
    "issue view 100 --comments --json body",
    "issue view 42 --web",
    "issue view 42 -w",
    "issue view 42 --json author",
    "issue view 42 --json comments",
    "issue view https://github.com/o/r/issues/7 --json state",
    "issue view '#42' --json state",
    "issue view 42 --json state --help",
    "issue view 42 --json state --cached",
    "issue view 42 --json state --template '{{.state}}'",
    "issue view 42 --json state -R github.com/o/r",
    "issue view 42 --json state --repo o",
    "pr view --json number",
    "pr view feature/issue-1 --json number",
    "pr view 42 --json mergeStateStatus,statusCheckRollup",
    "pr view 42 --json reviews",
    "issue list --label loom:issue",
    "issue list --label loom:issue --json number,labels",
    "issue list --json number,author",
    "issue list --json number,closedAt",
    "issue list --search '-label:loom:blocked' --json number",
    "issue list -S 'label:loom:issue' --json number",
    "issue list --assignee @me --json number",
    "issue list --label loom:issue --web",
    "pr list --state merged --limit 20 --json number,title",
    "pr list --json number,headRefName",
    "pr list --author app/dependabot --json number",
];

/// `pr checks` shapes served by [`super::super::pr_checks`] (#10516).
const CHECKS: &[&str] = &[
    "pr checks 42",
    "pr checks 10543 --repo rjwalters/loom",
    "pr checks 7 -R o/r --json bucket,name",
    "pr checks 7 --json=name,state,bucket,link,startedAt,completedAt,description",
    "pr checks --json bucket,name 7",
];

#[test]
fn pr_checks_snapshots_route_to_the_rest_checks_front() {
    for cmd in CHECKS {
        assert_eq!(classify(&argv(cmd)), Route::EtagChecks, "gh {cmd}");
    }
}

#[test]
fn views_route_to_the_etag_view() {
    for (cmd, entity) in VIEWS {
        assert_eq!(classify(&argv(cmd)), Route::EtagView(*entity), "gh {cmd}");
    }
}

#[test]
fn lists_route_to_the_etag_list_with_gh_default_limit() {
    for (cmd, entity, served) in LISTS {
        assert_eq!(classify(&argv(cmd)), Route::EtagList(*entity, argv(served)), "gh {cmd}");
    }
}

#[test]
fn mutations_api_and_unknown_shapes_pass_through() {
    for cmd in PASSTHROUGH {
        assert_eq!(classify(&argv(cmd)), Route::Passthrough, "gh {cmd}");
    }
}

#[test]
fn table_is_at_least_sixty_real_vectors() {
    assert!(VIEWS.len() + LISTS.len() + CHECKS.len() + PASSTHROUGH.len() >= 60);
}

#[test]
fn every_issue_and_pr_verb_but_view_and_list_passes_through() {
    for entity in ["issue", "pr"] {
        for verb in [
            "edit",
            "comment",
            "close",
            "reopen",
            "create",
            "merge",
            "review",
            "delete",
            "lock",
            "unlock",
            "pin",
            "develop",
            "ready",
            "checkout",
            "status",
            "frobnicate",
        ] {
            let cmd = argv(&format!("{entity} {verb} 42 --json state"));
            assert_eq!(classify(&cmd), Route::Passthrough, "gh {cmd:?}");
        }
    }
}

#[test]
fn slugs() {
    for ok in ["o/r", "rjwalters/loom", "2AMLogic/2am", "a-b/c.d_e"] {
        assert!(is_slug(ok), "{ok}");
    }
    for bad in [
        "",
        "o",
        "o/",
        "/r",
        "github.com/o/r",
        "https://github.com/o/r",
        "o/r?x",
    ] {
        assert!(!is_slug(bad), "{bad}");
    }
}
