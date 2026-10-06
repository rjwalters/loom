#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! W5: the row the agent `gh` front books for one passthrough.

use super::*;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn writer() -> CredAttr {
    CredAttr {
        account: "app-42".to_string(),
        owner: Some("acme".to_string()),
        kind: "writer",
    }
}

fn session(role: Option<&str>) -> Session {
    Session {
        role: role.map(str::to_string),
        ..Session::default()
    }
}

fn row(args: &[&str], role: Option<&str>) -> Option<Row> {
    plan(&os(args), &session(role), &writer(), || None)
}

#[test]
fn the_resource_follows_what_gh_spends() {
    let cases: &[(&[&str], &str, Pool)] = &[
        (&["api", "repos/o/r/pulls/1"], "agent.gh.api", Pool::Core),
        (&["api", "graphql", "-f", "query=q"], "agent.gh.api", Pool::Graphql),
        (
            &[
                "api",
                "-X",
                "POST",
                "repos/o/r/issues/1/comments",
                "-f",
                "body=b",
            ],
            "agent.gh.api",
            Pool::Core,
        ),
        (&["api", "search/issues?q=x"], "agent.gh.api", Pool::Search),
        (&["pr", "view", "7", "--json", "state"], "agent.gh.pr", Pool::Graphql),
        (&["pr", "create", "--title", "t", "--body", "b"], "agent.gh.pr", Pool::Graphql),
        (&["issue", "list", "--label", "x"], "agent.gh.issue", Pool::Graphql),
        (&["issue", "comment", "3", "--body", "b"], "agent.gh.issue", Pool::Graphql),
        (&["repo", "view"], "agent.gh.repo", Pool::Graphql),
        (&["search", "issues", "q"], "agent.gh.search", Pool::Search),
        (&["run", "list"], "agent.gh.run", Pool::Core),
        (&["release", "view"], "agent.gh.release", Pool::Core),
        (&["label", "list"], "agent.gh.label", Pool::Core),
        (&["some-extension", "x"], "agent.gh.other", Pool::Other),
    ];
    for (args, caller, pool) in cases {
        let row = row(args, Some("builder")).unwrap_or_else(|| panic!("{args:?} not booked"));
        assert_eq!((row.caller, row.pool), (*caller, *pool), "{args:?}");
        assert_eq!(row.attribution.rr.as_deref(), Some(pool.as_str()), "{args:?}");
    }
}

#[test]
fn a_row_carries_role_credential_and_repo_and_nothing_from_the_argv() {
    let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let args = [
        "issue",
        "comment",
        "3",
        "--repo",
        "acme/widget",
        "--body",
        "a private body with a token",
        "-H",
        secret,
    ];
    let row = row(&args, Some("Builder")).unwrap();
    assert_eq!(row.caller, "agent.gh.issue");
    assert_eq!(row.identity.role.as_deref(), Some("agent-builder"));
    assert_eq!(row.identity.repo.as_deref(), Some("acme/widget"));
    assert_eq!(row.identity.provider.as_deref(), Some("github"));
    assert_eq!(row.identity.origin.as_deref(), Some("github.com"));
    assert_eq!(row.identity.operation, None, "no inventoried operation is claimed");
    let at = &row.attribution;
    assert_eq!(
        (at.ca.as_deref(), at.co.as_deref(), at.tk.as_deref(), at.ro.as_deref()),
        (Some("app-42"), Some("acme"), Some("writer"), Some("target"))
    );
    let all = format!("{row:?}");
    for leaked in ["private body", secret, "comment", "--body"] {
        assert!(!all.contains(leaked), "{leaked:?} leaked into {all}");
    }
}

#[test]
fn the_role_label_is_bounded_and_never_empty() {
    assert_eq!(role_label(Some("judge")), "agent-judge");
    assert_eq!(role_label(Some("Sweep-Lifecycle")), "agent-sweep-lifecycle");
    assert_eq!(role_label(None), "agent-session");
    assert_eq!(role_label(Some("  ")), "agent-session");
    assert_eq!(role_label(Some("a b/c\n$(x)")), "agent-abcx");
    assert_eq!(role_label(Some(&"r".repeat(200))).len(), "agent-".len() + 32);
    assert_eq!(
        row(&["pr", "view", "1"], None)
            .unwrap()
            .identity
            .role
            .as_deref(),
        Some("agent-session")
    );
}

#[test]
fn the_repo_comes_from_the_argv_then_gh_repo_then_the_checkout() {
    let cred = writer();
    let plan_with = |args: &[&str], gh_repo: Option<&str>, remote: Option<&str>| {
        let session = Session {
            gh_repo: gh_repo.map(str::to_string),
            ..Session::default()
        };
        let row = plan(&os(args), &session, &cred, || remote.map(str::to_string)).unwrap();
        (row.identity.repo, row.attribution.ro)
    };
    let got = |repo: &str, ro: &str| (Some(repo.to_string()), Some(ro.to_string()));
    assert_eq!(
        plan_with(&["pr", "list", "-R", "o/r"], Some("x/y"), Some("z/w")),
        got("o/r", "target")
    );
    assert_eq!(plan_with(&["pr", "list", "--repo=o/r"], None, None), got("o/r", "target"));
    assert_eq!(
        plan_with(&["api", "/repos/o/r/issues/1/comments?per_page=100"], None, None),
        got("o/r", "target")
    );
    assert_eq!(plan_with(&["api", "repos/o/r"], None, None), got("o/r", "target"));
    assert_eq!(plan_with(&["pr", "list"], Some("x/y"), Some("z/w")), got("x/y", "target"));
    assert_eq!(plan_with(&["pr", "list"], None, Some("z/w")), got("z/w", "remote"));
    // gh's placeholder is not a repo; a URL-shaped --repo is not a slug.
    assert_eq!(
        plan_with(&["api", "repos/{owner}/{repo}/pulls"], None, Some("z/w")),
        got("z/w", "remote")
    );
    assert_eq!(
        plan_with(&["pr", "list", "-R", "https://github.com/o/r"], Some("x/y"), Some("z/w")),
        (None, Some("none".to_string())),
        "a repo this row cannot name is never guessed from the checkout"
    );
}

#[test]
fn the_remote_is_not_read_when_the_argv_names_the_repo() {
    let row = plan(&os(&["pr", "view", "1", "-R", "o/r"]), &Session::default(), &writer(), || {
        panic!("the checkout must not be read")
    });
    assert!(row.is_some());
}

#[test]
fn commands_that_never_reach_the_api_are_not_booked() {
    let cases: &[&[&str]] = &[
        &[],
        &["--version"],
        &["version"],
        &["help", "pr"],
        &["auth", "git-credential", "get"],
        &["auth", "status"],
        &["config", "get", "editor"],
        &["completion", "-s", "zsh"],
        &["alias", "list"],
        &["extension", "list"],
        &["pr", "--help"],
        &["issue", "list", "-h"],
    ];
    for args in cases {
        assert!(row(args, Some("builder")).is_none(), "{args:?}");
    }
}

#[test]
fn pages_and_the_free_probe_are_flagged() {
    let paged = row(&["api", "--paginate", "repos/o/r/issues"], None).unwrap();
    assert_eq!((paged.attribution.pg, paged.attribution.pu), (None, Some(true)));
    let download = row(&["run", "download", "9"], None).unwrap();
    assert_eq!(download.attribution.pg, Some(2));
    let probe = row(&["api", "rate_limit"], None).unwrap();
    assert_eq!((probe.pool, probe.attribution.fr), (Pool::Other, Some(true)));
    assert_eq!(row(&["api", "repos/o/r"], None).unwrap().attribution.fr, None);
    assert_eq!(row(&["some-extension"], None).unwrap().attribution.fr, None);
}

#[test]
fn an_env_token_or_an_unknown_directory_is_booked_as_such() {
    let args = os(&["pr", "view", "1", "-R", "o/r"]);
    let env = accounting::cred_of_with(None, true);
    let row = plan(&args, &Session::default(), &env, || None).unwrap();
    assert_eq!(
        (row.attribution.ca.as_deref(), row.attribution.co.as_deref()),
        (Some("env-token"), None)
    );
    let ambient = accounting::cred_of_with(Some(Path::new("/home/u/.config/gh")), false);
    let row = plan(&args, &Session::default(), &ambient, || None).unwrap();
    assert_eq!(row.attribution.ca.as_deref(), Some("ambient"));
}

#[test]
fn a_booked_row_rolls_up_by_bucket_and_by_role() {
    use crate::forge_call_stats::buckets::{aggregate_since, GroupBy};
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    for (args, role) in [
        (&["pr", "view", "1", "-R", "acme/widget"][..], "builder"),
        (&["api", "repos/acme/widget/pulls/1"][..], "builder"),
        (&["issue", "list", "-R", "acme/widget"][..], "judge"),
    ] {
        let row = row(args, Some(role)).unwrap();
        forge_call_stats::record_attributed(
            row.caller,
            &row.identity,
            row.pool,
            Outcome::Ok,
            None,
            &row.attribution,
        );
    }
    crate::forge_call_stats::set_test_sink_dir(None);
    let now = chrono::Utc::now().timestamp();
    let by = |group| aggregate_since(sink.path(), now - 60, now + 1, group);
    let charged = |agg: &crate::forge_call_stats::buckets::CallsAggregate, key: &[&str]| {
        agg.groups
            .iter()
            .find(|g| g.key.iter().map(String::as_str).eq(key.iter().copied()))
            .map(|g| g.charged)
    };
    let roles = by(GroupBy::Role);
    assert_eq!(charged(&roles, &["agent-builder"]), Some(2), "{roles:?}");
    assert_eq!(charged(&roles, &["agent-judge"]), Some(1), "{roles:?}");
    let buckets = by(GroupBy::Bucket);
    assert_eq!(charged(&buckets, &["app-42", "acme", "graphql", "-"]), Some(2), "{buckets:?}");
    assert_eq!(charged(&buckets, &["app-42", "acme", "core", "-"]), Some(1), "{buckets:?}");
    assert_eq!((buckets.no_repo, buckets.no_account), (0, 0));
    let callers = by(GroupBy::Caller);
    assert_eq!(charged(&callers, &["agent.gh.pr"]), Some(1), "{callers:?}");
}
