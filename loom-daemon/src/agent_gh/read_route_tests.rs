#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use super::*;

fn argv(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

#[test]
fn classified_reads() {
    for a in [
        "issue view 12 --json title",
        "issue list --state open",
        "issue list -Racme/repo --json number",
        "issue list -lbug",
        "run list -w ci.yml",
        "pr view 7 -R acme/repo",
        "pr list --json number",
        "pr checks 9",
        "pr diff 9",
        "repo view acme/repo",
        "run list --limit 5",
        "run view 123 --log",
        "workflow list",
        "label list",
        "api repos/acme/repo/pulls",
        "api /repos/acme/repo/issues/1/comments --paginate --jq .[].id",
        "api -H Accept:application/json repos/{owner}/{repo}/branches",
        "api user",
    ] {
        assert!(is_read(&argv(a)), "{a}");
    }
}

/// Unclassified means direct: every mutation, every method or body flag,
/// GraphQL, releases, the git credential helper and anything unknown.
#[test]
fn everything_else_stays_direct() {
    for a in [
        "issue create --title x",
        "issue edit 1 --add-label y",
        "issue comment 1 --body hi",
        "pr merge 3",
        "pr checkout 3",
        "pr create",
        "api -X GET repos/acme/repo",
        "api -XGET repos/acme/repo",
        "api --method=GET repos/acme/repo",
        "api repos/acme/repo/labels -f name=x",
        "api repos/acme/repo/labels -Fname=x",
        "api repos/acme/repo/labels --input body.json",
        "api graphql -f query={viewer{login}}",
        "api graphql",
        "release view v1",
        "release download v1",
        "auth git-credential get",
        "auth status",
        "search issues foo",
        "run download 1",
        "repo clone acme/repo",
        "status",
        "my-alias",
        "issue view 1 --web",
        "pr view --help",
        "issue",
        "",
        "issue status",
        "pr status",
        "pr checks 9 --watch",
        "pr view 7 --json title,viewerDidAuthor",
        "issue list --jq .[].viewerSubscription",
        "pr view 7 --unknown-flag x",
        "run view 1 -vx",
    ] {
        assert!(!is_read(&argv(a)), "{a}");
    }
}

#[test]
fn the_owner_comes_only_from_an_unambiguous_source() {
    let none = || None;
    let remote = || Some("Remote-Owner/repo".to_string());
    let t = |a: &str, gh_repo: Option<&str>, r: &dyn Fn() -> Option<String>| {
        target_owner(&argv(a), gh_repo, r)
    };
    assert_eq!(t("pr view 7 -R Acme/repo", None, &none).as_deref(), Some("acme"));
    assert_eq!(t("pr view 7 --repo=acme/repo", None, &none).as_deref(), Some("acme"));
    assert_eq!(t("api repos/acme/repo/pulls", None, &remote).as_deref(), Some("acme"));
    assert_eq!(t("repo view acme/repo", None, &remote).as_deref(), Some("acme"));
    assert_eq!(
        t("pr view https://github.com/acme/repo/pull/7", None, &remote).as_deref(),
        Some("acme")
    );
    assert_eq!(t("issue list", Some("ghrepo/x"), &remote).as_deref(), Some("ghrepo"));
    assert_eq!(t("issue list", None, &remote).as_deref(), Some("remote-owner"));
    assert_eq!(
        t("api repos/{owner}/{repo}/branches", None, &remote).as_deref(),
        Some("remote-owner")
    );
    // Ambiguous or unresolvable: no owner, so the call stays direct.
    assert_eq!(t("issue list", None, &none), None);
    assert_eq!(t("pr view 7 -R ghe.example.com/acme/repo", None, &remote), None);
    assert_eq!(t("pr view https://ghe.example.com/acme/repo/pull/7", None, &remote), None);
    assert_eq!(t("api user", None, &remote), None);
    assert_eq!(t("api orgs/acme/teams", None, &remote), None);
    assert_eq!(t("issue list", Some("not-a-slug"), &remote), None);
    // Attached `-R` and last-wins `--repo`, as gh resolves them.
    assert_eq!(t("pr view 7 -RAcme/repo", None, &none).as_deref(), Some("acme"));
    assert_eq!(t("pr list -R first/x --repo last/y", None, &none).as_deref(), Some("last"));
    assert_eq!(t("pr list --repo=first/x -Rlast/y", None, &none).as_deref(), Some("last"));
    // A URL inside a flag value is not a target: the remote decides.
    assert_eq!(
        t("issue list --search https://github.com/evil/x", None, &remote).as_deref(),
        Some("remote-owner")
    );
    assert_eq!(
        t("pr list --jq https://github.com/evil/x", None, &remote).as_deref(),
        Some("remote-owner")
    );
    // Two sources that disagree, or an unplaceable argv: direct.
    assert_eq!(t("pr view https://github.com/a/r/pull/1 -R b/r", None, &none), None);
    assert_eq!(t("pr view 7 -R", None, &remote), None);
    assert_eq!(t("pr view 7 --what x", None, &remote), None);
}

#[test]
fn plan_uses_the_gateway_only_for_a_read_with_a_live_profile() {
    let dir = PathBuf::from("/ws/.loom/gh-config-gateway/acme");
    let lookup = |o: &str| (o == "acme").then(|| dir.clone());
    let p = |a: &str| plan(&argv(a), None, || None, &lookup);
    assert_eq!(
        p("pr view 7 -R acme/repo"),
        FrontRoute {
            gateway: Some(dir.clone()),
            read: true
        }
    );
    // A read for an owner with no profile: direct, but booked as a read.
    assert_eq!(
        p("pr view 7 -R other/repo"),
        FrontRoute {
            gateway: None,
            read: true
        }
    );
    assert_eq!(p("pr merge 7 -R acme/repo"), FrontRoute::default());
    assert_eq!(p("release view -R acme/repo"), FrontRoute::default());
    // No gateway at all (the break-glass state): nothing changes.
    let off = |_: &str| None::<PathBuf>;
    assert_eq!(plan(&argv("pr view 7 -R acme/repo"), None, || None, &off).gateway, None);
}
