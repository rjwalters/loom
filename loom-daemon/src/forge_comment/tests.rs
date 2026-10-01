//! Tests for the comment chokepoint (#9772).
//!
//! Every test that touches `LOOM_DASHBOARD_URL` is `#[serial]`: the env var is
//! process-global and `cargo test` runs this module's tests on parallel threads.

use super::*;
use serial_test::serial;

/// Run `f` with `LOOM_DASHBOARD_URL` set to `value` (or removed for `None`),
/// restoring whatever was there before. Callers must be `#[serial]`.
fn with_base_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let prior = std::env::var(DASHBOARD_BASE_ENV).ok();
    match value {
        Some(v) => std::env::set_var(DASHBOARD_BASE_ENV, v),
        None => std::env::remove_var(DASHBOARD_BASE_ENV),
    }
    let out = f();
    match prior {
        Some(v) => std::env::set_var(DASHBOARD_BASE_ENV, v),
        None => std::env::remove_var(DASHBOARD_BASE_ENV),
    }
    out
}

#[test]
#[serial]
fn an_unset_base_is_the_public_fleet_dashboard() {
    with_base_env(None, || {
        assert_eq!(dashboard_base(), DEFAULT_DASHBOARD_BASE);
        assert_eq!(
            dashboard_url("o/r", 7, false),
            "https://dashboard.2amlogic.com/github.com/o/r/issues/7"
        );
    });
}

#[test]
#[serial]
fn the_env_override_changes_the_base_and_trailing_slashes_are_trimmed() {
    with_base_env(Some("https://d.example.com"), || {
        assert_eq!(dashboard_url("o/r", 7, false), "https://d.example.com/github.com/o/r/issues/7");
    });
    with_base_env(Some("https://d.example.com///"), || {
        assert_eq!(dashboard_url("o/r", 7, false), "https://d.example.com/github.com/o/r/issues/7");
    });
}

#[test]
#[serial]
fn a_blank_env_override_falls_back_to_the_default_rather_than_an_empty_base() {
    for blank in ["", "   ", "/"] {
        with_base_env(Some(blank), || {
            assert_eq!(dashboard_base(), DEFAULT_DASHBOARD_BASE, "blank {blank:?}");
        });
    }
}

#[test]
#[serial]
fn a_pr_links_to_pull_and_an_issue_to_issues() {
    with_base_env(None, || {
        assert!(dashboard_url("o/r", 42, true).ends_with("/o/r/pull/42"));
        assert!(dashboard_url("o/r", 42, false).ends_with("/o/r/issues/42"));
    });
}

#[test]
#[serial]
fn the_footer_is_a_blank_line_a_visible_link_and_a_hidden_marker() {
    with_base_env(None, || {
        assert_eq!(
            dashboard_footer("o/r", 42, false),
            "\n\n[loom dashboard](https://dashboard.2amlogic.com/github.com/o/r/issues/42)\n\
             <!-- loom:dashboard-link -->\n"
        );
    });
}

#[test]
#[serial]
fn with_footer_appends_once_and_only_once() {
    with_base_env(None, || {
        let once = with_footer("rjwalters/loom", 42, false, "Hello from loom.");
        let twice = with_footer("rjwalters/loom", 42, false, &once);
        assert_eq!(once, twice, "a second pass must not double-append");
        assert_eq!(once.matches(DASHBOARD_MARKER).count(), 1, "exactly one marker: {once:?}");
    });
}

#[test]
#[serial]
fn idempotence_holds_even_when_the_existing_footer_points_somewhere_else() {
    // The roster heartbeat regenerates its body and PATCHes it; a body that
    // already carries a marker (from any base URL) is left strictly alone.
    let carried = "record\n\n[loom dashboard](https://old.example.com/github.com/o/r/issues/1)\n\
                   <!-- loom:dashboard-link -->\n";
    with_base_env(Some("https://new.example.com"), || {
        assert_eq!(with_footer("o/r", 1, false, carried), carried);
    });
}

#[test]
#[serial]
fn trailing_whitespace_on_the_body_collapses_to_exactly_one_blank_line() {
    with_base_env(None, || {
        let a = with_footer("o/r", 1, false, "body");
        for ragged in ["body\n", "body\n\n\n", "body   \n \n"] {
            assert_eq!(with_footer("o/r", 1, false, ragged), a, "ragged {ragged:?}");
        }
    });
}

#[test]
#[serial]
fn an_empty_body_yields_the_footer_with_no_leading_blank_lines() {
    with_base_env(None, || {
        let out = with_footer("o/r", 1, false, "");
        assert!(out.starts_with("[loom dashboard]("), "{out:?}");
        assert!(out.ends_with("<!-- loom:dashboard-link -->\n"), "{out:?}");
    });
}

#[test]
#[serial]
fn a_blank_repo_omits_the_footer_because_there_is_no_url_to_link_to() {
    with_base_env(None, || {
        assert_eq!(dashboard_footer("", 1, false), "");
        assert_eq!(with_footer("  ", 1, false, "body"), "body");
    });
}

#[test]
#[serial]
fn the_rendered_comment_matches_the_checked_in_format_fixture() {
    // The pin a shell-side twin asserts against. If this fails, either the
    // format changed deliberately (update the fixture AND the twin) or it
    // drifted by accident (do not update the fixture).
    with_base_env(None, || {
        assert_eq!(
            with_footer("rjwalters/loom", 42, false, "Hello from loom."),
            FOOTER_FIXTURE,
            "src/forge_comment/fixtures/footer.txt no longer matches with_footer()"
        );
    });
}

#[test]
fn issue_number_widens_every_reference_shape_gh_issue_comment_accepted() {
    // The production shape: `watchdog::peer_coord`'s sentinel records what
    // `create-issue.sh` printed, which is a URL — a bare `parse::<u32>()` here
    // would skip the comment (and, in `dedup_comment`, re-file a duplicate
    // issue on every flap).
    assert_eq!(issue_number("https://github.com/rjwalters/loom/issues/1234"), Some(1234));
    assert_eq!(issue_number("https://github.com/rjwalters/loom/pull/1234/"), Some(1234));
    assert_eq!(
        issue_number("https://github.com/o/r/issues/1234#issuecomment-5920500372"),
        Some(1234)
    );
    assert_eq!(issue_number("1234"), Some(1234));
    assert_eq!(issue_number("  1234  "), Some(1234));
    assert_eq!(issue_number("#1234"), Some(1234));
}

#[test]
fn issue_number_rejects_a_reference_with_no_number_in_it() {
    for bad in [
        "",
        "   ",
        "main",
        "https://github.com/o/r/issues/",
        "o/r#abc",
        "12a",
    ] {
        assert_eq!(issue_number(bad), None, "{bad:?} should not parse");
    }
}

#[test]
fn the_comments_path_is_the_issues_endpoint_for_prs_too() {
    assert_eq!(comments_path("o/r", 9), "repos/o/r/issues/9/comments");
    // A PR is an issue for comments: `is_pr` never reaches the path.
    assert_eq!(comments_path("o/r", 9), comments_path("o/r", 9));
}

#[test]
fn an_unknown_repo_falls_back_to_ghs_own_owner_repo_placeholders() {
    assert_eq!(comments_path("", 9), "repos/{owner}/{repo}/issues/9/comments");
}

#[test]
#[serial]
fn post_command_names_the_endpoint_and_carries_the_footer_in_the_body_field() {
    with_base_env(None, || {
        let cmd = post_command("gh", None, "o/r", 42, true, "hi");
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(argv[0], "api");
        assert_eq!(argv[1], "repos/o/r/issues/42/comments");
        assert_eq!(argv[2], "--method");
        assert_eq!(argv[3], "POST");
        assert_eq!(argv[4], "-f");
        assert!(
            argv[5].starts_with("body=hi\n\n[loom dashboard](")
                && argv[5].ends_with("/github.com/o/r/pull/42)\n<!-- loom:dashboard-link -->\n"),
            "{:?}",
            argv[5]
        );
    });
}

#[test]
#[serial]
fn post_reports_the_endpoint_and_stderr_when_gh_fails() {
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("gh");
    std::fs::write(&gh, "#!/bin/sh\necho 'HTTP 403' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let err = with_base_env(None, || post(&gh, "o/r", 42, false, "hi")).unwrap_err();
    assert!(err.contains("repos/o/r/issues/42/comments"), "{err}");
    assert!(err.contains("HTTP 403"), "{err}");
}

#[test]
#[serial]
fn post_returns_the_forge_response_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("gh");
    std::fs::write(&gh, "#!/bin/sh\nprintf '{\"id\":1}\\n'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = with_base_env(None, || post(&gh, "o/r", 42, false, "hi")).unwrap();
    assert_eq!(out, "{\"id\":1}");
}

#[test]
#[serial]
fn resolve_nwo_prefers_loom_repo_over_the_git_remote() {
    let dir = tempfile::tempdir().unwrap();
    let prior = std::env::var("LOOM_REPO").ok();
    std::env::set_var("LOOM_REPO", "override/repo");
    let got = resolve_nwo(dir.path());
    match prior {
        Some(v) => std::env::set_var("LOOM_REPO", v),
        None => std::env::remove_var("LOOM_REPO"),
    }
    assert_eq!(got.as_deref(), Some("override/repo"));
}
