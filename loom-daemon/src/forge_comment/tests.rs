//! `forge_comment` tests: footer format (the contract the shell twin pins
//! to, #9774), idempotence, env override, reference parsing, and the POST
//! wire shape against a stub `gh`.

use super::*;

#[test]
fn dashboard_url_issues_vs_pull() {
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
    let stub = temp_stub_gh_failing();
    let error =
        post_comment(stub.path().join("gh"), None, "o/r", 42, false, "b").expect_err("stub fails");
    assert!(error.contains("API rate limit exceeded"), "{error}");
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
