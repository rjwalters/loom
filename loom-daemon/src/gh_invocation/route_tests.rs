use super::*;

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_string()).collect()
}

fn route(args: &[&str]) -> &'static str {
    classify(&argv(args)).route
}

fn method(args: &[&str]) -> Option<&'static str> {
    classify(&argv(args)).method
}

#[test]
fn rest_paths_become_templates_with_the_query_dropped() {
    for (endpoint, want) in [
        (
            "repos/acme/widgets/pulls/42/reviews?per_page=100",
            "/repos/{owner}/{repo}/pulls/{n}/reviews",
        ),
        ("/repos/acme/widgets/issues/7", "/repos/{owner}/{repo}/issues/{n}"),
        (
            "https://api.github.com/repos/acme/widgets/issues",
            "/repos/{owner}/{repo}/issues",
        ),
        (
            "repos/acme/widgets/commits/0123abcd/check-runs?per_page=100",
            "/repos/{owner}/{repo}/commits/{ref}/check-runs",
        ),
        ("repos/{owner}/{repo}/issues", "/repos/{owner}/{repo}/issues"),
        ("repos/:owner/:repo/pulls/3/files", "/repos/{owner}/{repo}/pulls/{n}/files"),
        (
            "repos/acme/widgets/actions/jobs/123456789/logs",
            "/repos/{owner}/{repo}/actions/jobs/{id}/logs",
        ),
        (
            "repos/acme/widgets/issues/42/labels/loom%3Ablocked",
            "/repos/{owner}/{repo}/issues/{n}/labels/{name}",
        ),
        ("rate_limit", "/rate_limit"),
    ] {
        assert_eq!(route(&["api", endpoint]), want, "{endpoint}");
    }
}

#[test]
fn a_greedy_tail_matches_slashes_and_an_exact_template_wins() {
    assert_eq!(
        route(&["api", "repos/acme/widgets/rules/branches/feature/issue-1"]),
        "/repos/{owner}/{repo}/rules/branches/{branch}"
    );
    assert_eq!(
        route(&["api", "repos/acme/widgets/compare/abc...def"]),
        "/repos/{owner}/{repo}/compare/{basehead}"
    );
    assert_eq!(
        route(&["api", "repos/acme/widgets/branches/main/protection"]),
        "/repos/{owner}/{repo}/branches/{branch}/protection"
    );
    assert_eq!(
        route(&[
            "api",
            "repos/acme/widgets/contents/.github/workflows/ci.yml?ref=abc"
        ]),
        "/repos/{owner}/{repo}/contents/{path}"
    );
}

#[test]
fn markers_for_graphql_unknown_other_host_and_no_endpoint() {
    assert_eq!(route(&["api", "graphql", "-f", "query=x"]), ROUTE_GRAPHQL);
    assert_eq!(route(&["api", "/graphql", "-f", "query=x"]), ROUTE_GRAPHQL);
    assert_eq!(route(&["api", "repos/acme/widgets/no-such-thing"]), ROUTE_UNKNOWN);
    // `{n}` is digits only; a non-number does not fit the issue template.
    assert_eq!(route(&["api", "repos/acme/widgets/issues/abc/comments"]), ROUTE_UNKNOWN);
    assert_eq!(route(&["api", "https://ghe.example.com/api/v3/repos/a/b"]), ROUTE_OTHER_HOST);
    assert_eq!(route(&["api", "--include"]), ROUTE_NONE);
}

#[test]
fn flag_values_are_never_read_as_the_endpoint() {
    let args = [
        "api",
        "--include",
        "-H",
        "If-None-Match: \"abc\"",
        "--jq",
        ".[]",
        "repos/acme/widgets/issues/1",
    ];
    assert_eq!(route(&args), "/repos/{owner}/{repo}/issues/{n}");
    assert_eq!(route(&["api", "-Xget", "repos/acme/widgets"]), "/repos/{owner}/{repo}");
}

#[test]
fn the_method_follows_ghs_rule() {
    assert_eq!(method(&["api", "repos/a/b/issues"]), Some("GET"));
    assert_eq!(method(&["api", "-X", "patch", "repos/a/b/issues/1"]), Some("PATCH"));
    assert_eq!(method(&["api", "-XDELETE", "repos/a/b/issues/1/labels/x"]), Some("DELETE"));
    assert_eq!(method(&["api", "--method=put", "repos/a/b/issues/1/lock"]), Some("PUT"));
    assert_eq!(method(&["api", "repos/a/b/issues", "-f", "title=t"]), Some("POST"));
    assert_eq!(method(&["api", "graphql", "-fquery=x"]), Some("POST"));
    assert_eq!(method(&["api", "-X", "GET", "search/issues", "-f", "q=x"]), Some("GET"));
    assert_eq!(method(&["api", "-X", "BREW", "repos/a/b"]), Some("OTHER"));
}

#[test]
fn porcelain_records_its_command_and_no_method() {
    for (args, command) in [
        (&["pr", "view", "42", "--json", "statusCheckRollup"][..], "pr view"),
        (&["pr", "checks", "42"][..], "pr checks"),
        (&["issue", "view", "7", "-R", "acme/widgets"][..], "issue view"),
        (&["-R", "acme/widgets", "issue", "list", "--label", "x"][..], "issue list"),
        (&["pr", "frobnicate"][..], "pr"),
        (&["my-extension", "go"][..], "other"),
        (&["Weird"][..], "other"),
        (&[][..], "(none)"),
    ] {
        let facts = classify(&argv(args));
        assert_eq!(facts.command, command, "{args:?}");
        assert_eq!(facts.route, ROUTE_PORCELAIN, "{args:?}");
        assert_eq!(facts.method, None, "{args:?}");
    }
    assert_eq!(classify(&argv(&["api", "repos/a/b"])).command, "api");
}

#[test]
fn no_argument_value_ever_leaves_the_classifier() {
    let secret = "s3cr3t-owner";
    let cases: Vec<Vec<String>> = vec![
        argv(&[
            "api",
            &format!("repos/{secret}/{secret}/issues/99?q={secret}"),
        ]),
        argv(&["api", &format!("{secret}/{secret}")]),
        argv(&["api", &format!("https://{secret}.example/x")]),
        argv(&["api", "-X", secret, "repos/a/b"]),
        argv(&["pr", secret]),
        argv(&[secret]),
    ];
    for case in cases {
        let facts = classify(&case);
        assert!(is_known_route(facts.route), "{case:?} -> {}", facts.route);
        for (key, value) in facts.attributes() {
            assert!(!value.contains(secret), "{key}={value} leaked from {case:?}");
            assert!(!value.contains("99"), "{key}={value} leaked a number from {case:?}");
        }
    }
}

#[test]
fn the_front_command_is_the_agents_not_the_facades() {
    clear_test_front_command();
    assert_eq!(front_command(), None);
    set_front_command(&argv(&["pr", "view", "9", "--json", "statusCheckRollup"]));
    assert_eq!(front_command(), Some("pr view"));
    clear_test_front_command();
}

#[test]
fn every_template_is_well_formed() {
    for t in templates() {
        assert!(t.text.starts_with('/') && !t.text.contains('+'), "{}", t.text);
        assert!(is_known_route(t.text));
    }
}
