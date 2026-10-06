//! `forge_comment` tests: footer format (the contract the shell twin pins
//! to, #9774), idempotence, env override, reference parsing, and the POST
//! wire shape against a stub `gh`.

use super::*;

#[test]
fn dashboard_url_issues_vs_pull() {
    let _env = DefaultDashboardEnv::hold();
    assert_eq!(
        forge_dashboard_url("o/r", 42, false),
        format!("{DEFAULT_DASHBOARD_BASE_URL}/github.com/o/r/issues/42")
    );
    assert_eq!(
        forge_dashboard_url("o/r", 42, true),
        format!("{DEFAULT_DASHBOARD_BASE_URL}/github.com/o/r/pull/42")
    );
}

#[test]
fn footer_is_the_pinned_format() {
    // Byte-exact: the shell twin (#9774) asserts the same string. Change both
    // or neither.
    assert_eq!(
        build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, "body text"),
        "body text\n\n[loom dashboard](https://dashboard.2amlogic.com/github.com/o/r/issues/9)\n<!-- loom:dashboard-link -->\n"
    );
}

#[test]
fn footer_is_idempotent_on_its_marker() {
    let once = build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, "body");
    assert_eq!(
        build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, &once),
        once,
        "a body already carrying the marker must not double-append"
    );
}

#[test]
fn footer_or_body_leaves_unresolvable_slugs_untouched() {
    let _env = DefaultDashboardEnv::hold();
    assert_eq!(footer_or_body(None, 9, false, "body"), "body");
    assert_eq!(
        footer_or_body(Some("o/r"), 9, false, "body"),
        build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, "body")
    );
}

#[test]
fn env_override_changes_the_base_and_trims_trailing_slashes() {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let previous = std::env::var(ENV_DASHBOARD_BASE_URL).ok();
    std::env::set_var(ENV_DASHBOARD_BASE_URL, "https://d.example.com///");
    assert_eq!(dashboard_base_url(), "https://d.example.com");
    assert_eq!(
        forge_dashboard_url("o/r", 1, false),
        "https://d.example.com/github.com/o/r/issues/1"
    );
    match previous {
        Some(value) => std::env::set_var(ENV_DASHBOARD_BASE_URL, value),
        None => std::env::remove_var(ENV_DASHBOARD_BASE_URL),
    }
    drop(guard);
}

#[test]
fn blank_env_falls_back_to_the_default() {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let previous = std::env::var(ENV_DASHBOARD_BASE_URL).ok();
    std::env::set_var(ENV_DASHBOARD_BASE_URL, "   ");
    assert_eq!(dashboard_base_url(), DEFAULT_DASHBOARD_BASE_URL);
    match previous {
        Some(value) => std::env::set_var(ENV_DASHBOARD_BASE_URL, value),
        None => std::env::remove_var(ENV_DASHBOARD_BASE_URL),
    }
    drop(guard);
}

#[test]
fn parse_issue_ref_accepts_the_stored_shapes() {
    assert_eq!(parse_issue_ref("9772"), Some((None, 9772)));
    assert_eq!(parse_issue_ref(" 9772 "), Some((None, 9772)));
    assert_eq!(parse_issue_ref("o/r#9772"), Some((Some("o/r".to_string()), 9772)));
    assert_eq!(parse_issue_ref("o/r/issues/9772"), Some((Some("o/r".to_string()), 9772)));
    assert_eq!(
        parse_issue_ref("https://github.com/o/r/issues/9772"),
        Some((Some("o/r".to_string()), 9772))
    );
    assert_eq!(
        parse_issue_ref("http://www.github.com/o/r/pull/9"),
        Some((Some("o/r".to_string()), 9))
    );
    assert_eq!(parse_issue_ref(""), None);
    assert_eq!(parse_issue_ref("o/r"), None);
    assert_eq!(parse_issue_ref("o/r/issues/notanumber"), None);
    assert_eq!(parse_issue_ref("https://github.com/o/r/pull/"), None);
}

#[test]
fn post_comment_sends_the_footer_through_the_rest_endpoint() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh();
    let result = post_comment(stub.path().join("gh"), None, "o/r", 42, false, "the body")
        .expect("stub gh succeeds");
    assert_eq!(result, "ok\n");

    let recorded =
        std::fs::read_to_string(stub.path().join("gh.args")).expect("stub recorded its args");
    assert!(
        recorded.contains("repos/o/r/issues/42/comments"),
        "REST endpoint, one number sequence: {recorded}"
    );
    assert!(
        recorded.contains("--input") && !recorded.contains(" -f "),
        "JSON body file, never -f: {recorded}"
    );
    let payload = std::fs::read_to_string(stub.path().join("gh.stdin"))
        .expect("stub recorded the --input body");
    let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON payload");
    let body = parsed["body"].as_str().expect("body field");
    assert!(
        body.starts_with("the body\n\n[loom dashboard]("),
        "footer appended to the caller's body: {body}"
    );
    assert!(
        body.contains("](https://dashboard.2amlogic.com/github.com/o/r/issues/42)\n"),
        "link points at the issues page: {body}"
    );
    assert!(
        body.ends_with(&format!("{FOOTER_MARKER}\n")),
        "marker terminates (with the format's trailing newline): {body}"
    );
}

#[test]
fn post_comment_surveys_stderr_on_failure() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_failing();
    let error =
        post_comment(stub.path().join("gh"), None, "o/r", 42, false, "b").expect_err("stub fails");
    assert!(error.contains("API rate limit exceeded"), "{error}");
}

// ---------------------------------------------------------------------------
// #10025: GraphQL `addComment` fallback on a REST rate limit.
// ---------------------------------------------------------------------------

/// A stub `gh` whose REST POST fails with `rest_stderr` and whose `api
/// graphql` calls answer the node-id lookup and the mutation — or fail too,
/// when `graphql_ok` is false. Every call's `--input <file>` body lands in
/// `gh.stdin.<n>` (n = 1-based call index) so the bodies can be compared byte
/// for byte.
fn temp_stub_gh_rest_limited(rest_stderr: &str, graphql_ok: bool) -> StubGh {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let graphql = if graphql_ok {
        r#"case "$(cat "$in")" in
      *addComment*) echo '{"data":{"addComment":{"commentEdge":{"node":{"id":"IC_new","url":"https://github.com/o/r/issues/42#issuecomment-7"}}}}}' ;;
      *) echo '{"data":{"repository":{"issueOrPullRequest":{"id":"I_node42"}}}}' ;;
    esac"#
            .to_string()
    } else {
        "echo 'GraphQL: Something went wrong' >&2; exit 1".to_string()
    };
    let script = format!(
        "#!/bin/sh
echo \"$@\" >> {d}/gh.args
n=$(wc -l < {d}/gh.args | tr -d ' ')
in={d}/gh.stdin.$n
prev=
for a in \"$@\"; do [ \"$prev\" = --input ] && cat \"$a\" > \"$in\"; prev=\"$a\"; done
if [ \"$2\" = graphql ]; then
    {graphql}
    exit 0
fi
echo '{rest_stderr}' >&2
exit 1
"
    );
    let path = dir.path().join("gh");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    StubGh(dir)
}

fn recorded_json(stub: &StubGh, call: usize) -> serde_json::Value {
    let raw = std::fs::read_to_string(stub.path().join(format!("gh.stdin.{call}")))
        .unwrap_or_else(|e| panic!("stub recorded call {call}'s --input body: {e}"));
    serde_json::from_str(&raw).expect("each request body is JSON")
}

#[test]
fn rest_rate_limit_falls_back_to_graphql_add_comment_byte_identically() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_rest_limited("HTTP 403: API rate limit exceeded for user ID 1.", true);
    let body = "multi-line\n\n- with `markdown` and \"quotes\"\n";
    let response = post_comment(stub.path().join("gh"), None, "o/r", 42, false, body)
        .expect("GraphQL fallback answers");

    let args = std::fs::read_to_string(stub.path().join("gh.args")).unwrap();
    let calls: Vec<&str> = args.lines().collect();
    assert_eq!(calls.len(), 3, "REST POST, node-id lookup, addComment: {args}");
    assert!(calls[0].contains("repos/o/r/issues/42/comments"), "{args}");
    assert!(calls[1].starts_with("api graphql --input "), "{args}");
    assert!(calls[2].starts_with("api graphql --input "), "{args}");

    let lookup = recorded_json(&stub, 2);
    assert_eq!(lookup["variables"]["owner"], "o");
    assert_eq!(lookup["variables"]["name"], "r");
    assert_eq!(lookup["variables"]["number"], 42);

    // Byte-identity: the GraphQL body IS the footered body REST was handed.
    let rest_body = recorded_json(&stub, 1)["body"]
        .as_str()
        .unwrap()
        .to_string();
    let mutation = recorded_json(&stub, 3);
    assert_eq!(mutation["variables"]["subjectId"], "I_node42");
    assert_eq!(mutation["variables"]["body"].as_str().unwrap(), rest_body);
    assert_eq!(rest_body, append_dashboard_footer("o/r", 42, false, body));
    assert!(rest_body.ends_with(&format!("{FOOTER_MARKER}\n")));

    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed["html_url"], "https://github.com/o/r/issues/42#issuecomment-7");
}

#[test]
fn graphql_fallback_failure_returns_the_original_rest_error() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_rest_limited("HTTP 403: API rate limit exceeded for user ID 1.", false);
    let error = post_comment(stub.path().join("gh"), None, "o/r", 42, false, "b")
        .expect_err("both transports fail");
    assert!(error.starts_with("gh api (comment on o/r#42) failed:"), "{error}");
    assert!(error.contains("API rate limit exceeded"), "{error}");
    assert!(!error.contains("GraphQL"), "the REST error, not the fallback's: {error}");
}

#[test]
fn non_rate_limit_rest_failure_does_not_try_graphql() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_rest_limited("HTTP 404: Not Found", true);
    let error = post_comment(stub.path().join("gh"), None, "o/r", 42, false, "b")
        .expect_err("a 404 is not retried");
    assert!(error.contains("HTTP 404"), "{error}");
    let args = std::fs::read_to_string(stub.path().join("gh.args")).unwrap();
    assert_eq!(args.lines().count(), 1, "no GraphQL call for a non-rate-limit failure: {args}");
}

// ---------------------------------------------------------------------------
// Stub `gh` fixtures (the `merge_pr/redate` pattern: a shell script standing
// in for the binary, recording what it was handed).
// ---------------------------------------------------------------------------

struct StubGh(tempfile::TempDir);

impl StubGh {
    fn path(&self) -> &Path {
        self.0.path()
    }
}

fn write_stub(dir: &Path, exit_code: u8) {
    let args_path = dir.join("gh.args");
    let stdin_path = dir.join("gh.stdin");
    let script = dir.join("gh");
    let record = format!(
        "echo \"$@\" >> {args}\nprev=\nfor a in \"$@\"; do [ \"$prev\" = --input ] && cat \"$a\" > {stdin}; prev=\"$a\"; done\n",
        args = args_path.display(),
        stdin = stdin_path.display(),
    );
    let body = if exit_code == 0 {
        format!("#!/bin/sh\n{record}echo ok\n")
    } else {
        format!("#!/bin/sh\n{record}echo 'API rate limit exceeded' >&2\nexit {exit_code}\n")
    };
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn temp_stub_gh() -> StubGh {
    let dir = tempfile::tempdir().unwrap();
    write_stub(dir.path(), 0);
    StubGh(dir)
}

fn temp_stub_gh_failing() -> StubGh {
    let dir = tempfile::tempdir().unwrap();
    write_stub(dir.path(), 1);
    StubGh(dir)
}

/// Serializes the env-mutating tests — `cargo test` runs in parallel threads
/// in one process, and a global env var would race (the same reasoning
/// `merge_pr/redate` documents for parameterizing on the binary).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds `ENV_LOCK` and pins `ENV_DASHBOARD_BASE_URL` unset for a test whose
/// result depends on the footer URL (every `post_comment` call reads it), so a
/// parallel env-override test cannot change the base mid-test. Restores the
/// previous value on drop, before releasing the lock.
struct DefaultDashboardEnv {
    previous: Option<String>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl DefaultDashboardEnv {
    fn hold() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var(ENV_DASHBOARD_BASE_URL).ok();
        std::env::remove_var(ENV_DASHBOARD_BASE_URL);
        Self {
            previous,
            _guard: guard,
        }
    }
}

impl Drop for DefaultDashboardEnv {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            std::env::set_var(ENV_DASHBOARD_BASE_URL, value);
        }
    }
}
