//! Tests for `forge rerun` (#10633).
#![allow(clippy::unwrap_used)]

use super::*;

const EP: &str = "repos/o/r/actions/runs/9/rerun-failed-jobs";

fn http(status: u16, headers: &str, body: &str) -> String {
    format!("HTTP/2.0 {status} X\r\n{headers}\r\n{body}")
}

#[test]
fn endpoints_name_run_failed_jobs_and_job() {
    assert_eq!(What::Run(9).endpoint("o/r"), "repos/o/r/actions/runs/9/rerun");
    assert_eq!(What::Failed(9).endpoint("o/r"), EP);
    assert_eq!(What::Job(7).endpoint("o/r"), "repos/o/r/actions/jobs/7/rerun");
}

#[test]
fn a_201_is_ok() {
    let o = classify(What::Failed(9), EP, true, &http(201, "", "{}"), "");
    assert_eq!(o.sentinel(), "LOOM-RERUN-OK run 9");
    assert_eq!(o.exit_code(), 0);
}

#[test]
fn a_missing_actions_write_is_a_permission_denial_naming_the_grant() {
    let body = r#"{"message":"Resource not accessible by integration","status":"403"}"#;
    let o = classify(
        What::Failed(9),
        EP,
        false,
        &http(403, "", body),
        "gh: Resource not accessible by integration (HTTP 403)",
    );
    assert_eq!(
        o.sentinel(),
        format!(
            "LOOM-RERUN-DENIED permission HTTP 403 for {EP}: permission (needs actions:write): \
             Resource not accessible by integration"
        )
    );
    assert_eq!(o.exit_code(), 1);
}

#[test]
fn a_secondary_rate_limit_is_its_own_class_with_exit_2() {
    let body = r#"{"message":"You have exceeded a secondary rate limit."}"#;
    let o = classify(What::Job(7), EP, false, &http(403, "Retry-After: 60\r\n", body), "");
    assert!(
        o.sentinel()
            .starts_with("LOOM-RERUN-DENIED secondary-rate-limit HTTP 403"),
        "{o:?}"
    );
    assert!(!o.sentinel().contains("needs"), "a rate limit names no grant: {o:?}");
    assert_eq!(o.exit_code(), 2);
}

#[test]
fn a_run_still_in_progress_is_forbidden_not_permission() {
    let body = r#"{"message":"This workflow is already running"}"#;
    let o = classify(What::Run(9), EP, false, &http(403, "", body), "");
    assert!(
        o.sentinel()
            .starts_with("LOOM-RERUN-DENIED forbidden HTTP 403"),
        "{o:?}"
    );
    assert!(o.sentinel().ends_with("This workflow is already running"), "{o:?}");
    assert_eq!(o.exit_code(), 1);
}

#[test]
fn a_stderr_only_403_is_still_classified() {
    let o = classify(
        What::Run(9),
        EP,
        false,
        "",
        "gh: Resource not accessible by integration (HTTP 403)",
    );
    assert!(
        o.sentinel()
            .starts_with("LOOM-RERUN-DENIED permission HTTP 403"),
        "{o:?}"
    );
}

#[test]
fn a_404_or_no_response_is_error() {
    let o = classify(What::Run(9), EP, false, &http(404, "", r#"{"message":"Not Found"}"#), "");
    assert_eq!(o.sentinel(), format!("LOOM-RERUN-ERROR HTTP 404 for {EP}: Not Found"));
    assert_eq!(o.exit_code(), 3);
    let o = classify(What::Run(9), EP, false, "", "dial tcp: i/o timeout");
    assert_eq!(o.sentinel(), format!("LOOM-RERUN-ERROR {EP}: dial tcp: i/o timeout"));
}

#[test]
fn rerun_posts_on_the_writer_and_classifies_end_to_end() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("gh");
    let log = dir.path().join("argv");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$*\" > '{}'\nprintf 'HTTP/2.0 403 Forbidden\\r\\n\\r\\n{{\"message\":\"Resource not accessible by integration\"}}'\necho 'gh: Resource not accessible by integration (HTTP 403)' 1>&2\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = rerun(&gh, Some(dir.path()), "o/r", What::Failed(9));
    assert!(o.sentinel().contains("permission (needs actions:write)"), "{o:?}");
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(argv.contains(&format!("api -X POST --include {EP}")), "{argv}");
}
