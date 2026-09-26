//! Credential-rejection tests (Issue #8850) — a 401 "Bad credentials" aborts
//! the whole cycle as `CycleError::CredentialRejected`, is named as such, and
//! never notifies the rate-limit breaker.
//!
//! A sibling module of `super` (the phase-1 suite) rather than more lines in
//! it: `ci_telemetry/tests.rs` is at the file-size ratchet, and these tests
//! are a self-contained concern.

use super::*;

/// The body GitHub answers a dead or expired token with.
fn bad_credentials() -> ApiResponse {
    ApiResponse {
        status: 401,
        body: "{\"message\":\"Bad credentials\",\"documentation_url\":\"https://docs.github.com/rest\"}"
            .into(),
        ..ApiResponse::default()
    }
}

fn breaker_notifications() -> usize {
    super::poll::BREAKER_NOTIFICATIONS.with(std::cell::Cell::get)
}

#[test]
fn credential_failure_signature_is_narrow() {
    use super::poll::indicates_credential_failure;
    assert!(indicates_credential_failure("gh: Bad credentials (HTTP 401)"));
    assert!(indicates_credential_failure("{\"message\":\"Requires authentication\"}"));
    assert!(!indicates_credential_failure("{\"message\":\"Not Found\"}"));
    assert!(!indicates_credential_failure("HTTP 404: Not Found"));
    assert!(!indicates_credential_failure("You have exceeded a secondary rate limit"));
    assert!(!indicates_credential_failure("Resource not accessible by integration"));
}

#[test]
fn a_rejected_credential_aborts_the_cycle_with_no_further_repo_request() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    // alpha is the first repo discovered.
    api.set_override(&format!("repos/{ORG}/alpha/actions/runs?per_page=100"), bad_credentials());
    let breaker_before = breaker_notifications();
    let suppressed_before = crate::rate_limit_breaker::global_is_suppressed();

    let result = run_cycle(&ctx(dir.path()), &api);
    let Err(error @ CycleError::CredentialRejected { .. }) = result else {
        panic!(
            "a 401 Bad credentials must abort the whole cycle as CredentialRejected: {result:?}"
        );
    };
    assert!(!matches!(error, CycleError::RateLimited { .. }));
    let named = error.to_string();
    assert!(named.starts_with("credential-rejected:"), "{named}");
    assert!(!named.contains("rate-limit"), "{named}");

    // AC1: beta — every repo after the failing one — is never requested.
    let requests = api.requests();
    assert!(
        !requests.iter().any(|(p, _)| p.contains("/beta/")),
        "no further per-repo request after a rejected credential: {requests:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(p, _)| p.starts_with("repos/"))
            .count(),
        1,
        "exactly the one failing request: {requests:?}"
    );

    // AC2: the breaker is not notified, and no rate-limit backoff is recorded.
    assert_eq!(breaker_notifications(), breaker_before);
    assert_eq!(crate::rate_limit_breaker::global_is_suppressed(), suppressed_before);
    let status = state::load_status(&state_dir(dir.path()));
    assert_eq!(status.backoff_until, None);
    let last_error = status.last_error.unwrap();
    assert!(last_error.starts_with("credential-rejected:"), "{last_error}");
    assert!(!last_error.contains("rate-limit"), "{last_error}");
    assert_eq!(status.consecutive_failures, 1);

    // Nothing is waited out: the next cycle (credential renewed) polls at once.
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
}

#[test]
fn a_rate_limit_does_notify_the_breaker_seam() {
    // Control for the negative assertion above: the counter really does move
    // on the path that is supposed to feed the breaker.
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.set_override(
        &format!("repos/{ORG}/alpha/actions/runs?per_page=100"),
        ApiResponse {
            status: 429,
            retry_after_secs: Some(60),
            ..ApiResponse::default()
        },
    );
    let before = breaker_notifications();
    assert!(matches!(run_cycle(&ctx(dir.path()), &api), Err(CycleError::RateLimited { .. })));
    assert_eq!(breaker_notifications(), before + 1);
}

#[test]
fn a_rejected_credential_during_discovery_is_named_not_a_discovery_failure() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.set_override(&format!("orgs/{ORG}/repos?per_page=100&type=all"), bad_credentials());
    let before = breaker_notifications();
    assert!(matches!(
        run_cycle(&ctx(dir.path()), &api),
        Err(CycleError::CredentialRejected { .. })
    ));
    assert_eq!(breaker_notifications(), before);
    assert_eq!(api.requests().len(), 1);
}

#[test]
fn a_404_on_one_repo_still_fails_only_that_repo() {
    // AC3: the credential signature must not swallow a genuinely per-repo
    // failure — a repo the token cannot see, in an org listing it can.
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.set_override(
        &format!("repos/{ORG}/alpha/actions/runs?per_page=100"),
        ApiResponse {
            status: 404,
            body: "{\"message\":\"Not Found\"}".into(),
            ..ApiResponse::default()
        },
    );
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.repo_errors.len(), 1);
    assert!(
        report.repo_errors[0].contains("fixture-org/alpha")
            && report.repo_errors[0].contains("HTTP 404")
    );
    assert_eq!(report.summary.repos_polled, 1);
    assert_eq!(report.summary.runs_emitted, 3, "beta is still polled");
    assert!(api.requests().iter().any(|(p, _)| p.contains("/beta/")));
}

#[test]
fn progress_committed_before_the_credential_died_is_counted_and_never_re_emitted() {
    // AC4: alpha completes, then the credential dies on beta.
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.set_override(&format!("repos/{ORG}/beta/actions/runs?per_page=100"), bad_credentials());
    let Err(CycleError::CredentialRejected { progress, .. }) = run_cycle(&ctx(dir.path()), &api)
    else {
        panic!("a 401 on beta must abort the cycle as CredentialRejected");
    };
    let (runs, jobs, _, _) = kind_counts(dir.path());
    assert_eq!(progress.repos_polled, 1);
    assert_eq!(progress.runs_emitted, 3, "alpha's runs");
    assert_eq!((progress.runs_emitted, progress.jobs_emitted), (runs, jobs));
    assert!(jobs > 0);
    let status = state::load_status(&state_dir(dir.path()));
    assert_eq!(status.last_cycle.map(|s| (s.runs_emitted, s.jobs_emitted)), Some((3, jobs)));
    assert!(status
        .last_error
        .unwrap()
        .contains("after 1 repo(s), 3 run(s)"));

    // Credential renewed: only beta's work is emitted, and nothing twice.
    let report = run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!(report.summary.runs_emitted, 3, "beta's runs only");
    assert_eq!(report.summary.jobs_emitted, 24 - jobs);
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
    assert_no_duplicates(dir.path());
}
