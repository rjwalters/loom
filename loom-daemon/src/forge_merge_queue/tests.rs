//! Tests for the dormant merge-queue controls (#10255).

#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::path::Path;

use serde_json::json;
use serial_test::serial;
use tempfile::tempdir;

use super::github::{classify_failure, FailureClass, GhQueueApi};
use super::mode::*;
use super::ops::*;
use super::preflight::*;
use super::*;
use crate::forge_merge_config::BranchRule;

const APPROVED: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

// --- config ---------------------------------------------------------------

#[test]
fn mode_defaults_to_direct() {
    let r = resolve_merge_mode_from(None, &json!({})).unwrap();
    assert_eq!(r.mode, MergeMode::Direct);
    assert_eq!(r.source, MergeModeSource::Default);
    let r = resolve_merge_mode_from(None, &json!({"champion": {"mergeMode": null}})).unwrap();
    assert_eq!(r.mode, MergeMode::Direct);
}

#[test]
fn mode_config_override_and_env_precedence() {
    let cfg = json!({"champion": {"mergeMode": "queue"}});
    let r = resolve_merge_mode_from(None, &cfg).unwrap();
    assert_eq!((r.mode, r.source), (MergeMode::Queue, MergeModeSource::Config));
    let r = resolve_merge_mode_from(Some("direct"), &cfg).unwrap();
    assert_eq!((r.mode, r.source), (MergeMode::Direct, MergeModeSource::Env));
    // Empty env is unset, not an invalid value.
    let r = resolve_merge_mode_from(Some("  "), &cfg).unwrap();
    assert_eq!(r.source, MergeModeSource::Config);
}

#[test]
fn mode_rejects_unknown_values_without_fallback() {
    for bad in [
        json!("queued"),
        json!("Queue"),
        json!(""),
        json!(true),
        json!(1),
    ] {
        let cfg = json!({"champion": {"mergeMode": bad}});
        let e = resolve_merge_mode_from(None, &cfg).unwrap_err();
        assert_eq!(e.source, MergeModeSource::Config, "{bad}");
        assert!(e.to_string().contains("champion.mergeMode"), "{e}");
    }
    let e = resolve_merge_mode_from(Some("fast"), &json!({})).unwrap_err();
    assert_eq!(e.source, MergeModeSource::Env);
    assert!(e.to_string().contains(MERGE_MODE_ENV), "{e}");
}

#[test]
#[serial(loom_config_env)]
fn mode_resolves_through_the_tier_chain() {
    std::env::remove_var(MERGE_MODE_ENV);
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(dir.path().join(".loom/config.json"), r#"{"champion":{"mergeMode":"direct"}}"#)
        .unwrap();
    std::fs::create_dir_all(dir.path().join(".loom-local")).unwrap();
    std::fs::write(
        dir.path().join(".loom-local/local.json"),
        r#"{"champion":{"mergeMode":"queue"}}"#,
    )
    .unwrap();
    let r = resolve_merge_mode(dir.path()).unwrap();
    assert_eq!((r.mode, r.source), (MergeMode::Queue, MergeModeSource::Config));
}

// --- ops against a scripted fake -------------------------------------------

#[derive(Default)]
struct FakeApi {
    statuses: RefCell<Vec<Result<PrQueueStatus, QueueError>>>,
    enqueue_result: Option<Result<EnqueueAck, QueueError>>,
    dequeue_result: Option<Result<DequeueAck, QueueError>>,
    calls: RefCell<Vec<String>>,
}

impl FakeApi {
    fn with(statuses: Vec<Result<PrQueueStatus, QueueError>>) -> Self {
        Self {
            statuses: RefCell::new(statuses.into_iter().rev().collect()),
            ..Self::default()
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl QueueApi for FakeApi {
    fn status(&self, pr: u32) -> Result<PrQueueStatus, QueueError> {
        self.calls.borrow_mut().push(format!("status {pr}"));
        self.statuses.borrow_mut().pop().unwrap()
    }
    fn enqueue(&self, _pr: u32, node: &str, head: &str) -> Result<EnqueueAck, QueueError> {
        self.calls
            .borrow_mut()
            .push(format!("enqueue {node} {head}"));
        self.enqueue_result.clone().unwrap()
    }
    fn dequeue(&self, _pr: u32, node: &str) -> Result<DequeueAck, QueueError> {
        self.calls.borrow_mut().push(format!("dequeue {node}"));
        self.dequeue_result.clone().unwrap()
    }
}

fn st(state: PrState, head: &str, queued_at: Option<&str>) -> Result<PrQueueStatus, QueueError> {
    Ok(PrQueueStatus {
        number: 7,
        node_id: "PR_node".into(),
        state,
        head_oid: head.into(),
        entry: queued_at.map(|h| QueueEntry {
            state: "QUEUED".into(),
            position: Some(2),
            head_oid: Some(h.into()),
        }),
    })
}

#[test]
fn enqueue_pins_the_approved_sha() {
    let mut api = FakeApi::with(vec![st(PrState::Open, APPROVED, None)]);
    api.enqueue_result = Some(Ok(EnqueueAck::Enqueued { position: Some(1) }));
    let out = enqueue(&api, 7, &APPROVED.to_uppercase()).unwrap();
    assert_eq!(out, EnqueueOutcome::Enqueued { position: Some(1) });
    assert_eq!(
        api.calls(),
        vec![
            "status 7".to_string(),
            format!("enqueue PR_node {APPROVED}")
        ]
    );
}

#[test]
fn enqueue_head_mismatch_never_sends_the_mutation() {
    let api = FakeApi::with(vec![st(PrState::Open, OTHER, None)]);
    let e = enqueue(&api, 7, APPROVED).unwrap_err();
    assert_eq!(e.code(), "HEAD_MISMATCH");
    assert!(e.to_string().contains("never retried"), "{e}");
    assert_eq!(api.calls(), vec!["status 7"]);
}

#[test]
fn enqueue_forge_side_head_mismatch_is_not_retried() {
    let mut api = FakeApi::with(vec![st(PrState::Open, APPROVED, None)]);
    api.enqueue_result = Some(Err(QueueError::HeadMismatch {
        pr: 7,
        approved: APPROVED.into(),
        actual: None,
    }));
    assert_eq!(enqueue(&api, 7, APPROVED).unwrap_err().code(), "HEAD_MISMATCH");
    assert_eq!(api.calls().len(), 2, "one status + one enqueue, no retry");
}

#[test]
fn enqueue_already_queued_is_an_idempotent_no_op() {
    let api = FakeApi::with(vec![st(PrState::Open, APPROVED, Some(APPROVED))]);
    let out = enqueue(&api, 7, APPROVED).unwrap();
    assert_eq!(out, EnqueueOutcome::AlreadyQueued { position: Some(2) });
    assert_eq!(api.calls(), vec!["status 7"]);
}

#[test]
fn enqueue_already_queued_at_another_head_is_refused() {
    let api = FakeApi::with(vec![st(PrState::Open, OTHER, Some(OTHER))]);
    assert_eq!(enqueue(&api, 7, APPROVED).unwrap_err().code(), "QUEUED_AT_OTHER_HEAD");
}

#[test]
fn enqueue_race_already_queued_rereads_status() {
    let mut api = FakeApi::with(vec![
        st(PrState::Open, APPROVED, None),
        st(PrState::Open, APPROVED, Some(APPROVED)),
    ]);
    api.enqueue_result = Some(Ok(EnqueueAck::AlreadyQueued));
    assert_eq!(
        enqueue(&api, 7, APPROVED).unwrap(),
        EnqueueOutcome::AlreadyQueued { position: Some(2) }
    );
}

#[test]
fn enqueue_rejects_closed_prs_and_short_shas() {
    let api = FakeApi::with(vec![st(PrState::Merged, APPROVED, None)]);
    assert_eq!(enqueue(&api, 7, APPROVED).unwrap_err().code(), "PR_NOT_OPEN");
    let api = FakeApi::with(vec![]);
    assert_eq!(enqueue(&api, 7, "abc123").unwrap_err().code(), "INVALID_SHA");
    assert!(api.calls().is_empty());
}

#[test]
fn enqueue_denied_and_unavailable_propagate() {
    for err in [
        QueueError::Denied {
            detail: "Resource not accessible".into(),
        },
        QueueError::QueueUnavailable {
            detail: "merge queue is not enabled".into(),
        },
        QueueError::RateLimited {
            detail: "API rate limit exceeded".into(),
        },
    ] {
        let mut api = FakeApi::with(vec![st(PrState::Open, APPROVED, None)]);
        api.enqueue_result = Some(Err(err.clone()));
        assert_eq!(enqueue(&api, 7, APPROVED).unwrap_err(), err);
    }
}

#[test]
fn dequeue_absent_is_a_no_op() {
    let api = FakeApi::with(vec![st(PrState::Open, APPROVED, None)]);
    assert_eq!(dequeue(&api, 7).unwrap(), DequeueOutcome::NotQueued);
    assert_eq!(api.calls(), vec!["status 7"]);
}

#[test]
fn dequeue_rereads_and_tolerates_a_merge_race() {
    let mut api = FakeApi::with(vec![
        st(PrState::Open, APPROVED, Some(APPROVED)),
        st(PrState::Open, APPROVED, None),
    ]);
    api.dequeue_result = Some(Ok(DequeueAck::Dequeued));
    assert_eq!(dequeue(&api, 7).unwrap(), DequeueOutcome::Dequeued);

    let mut api = FakeApi::with(vec![
        st(PrState::Open, APPROVED, Some(APPROVED)),
        st(PrState::Merged, APPROVED, None),
    ]);
    api.dequeue_result = Some(Ok(DequeueAck::Dequeued));
    assert_eq!(dequeue(&api, 7).unwrap(), DequeueOutcome::AlreadyMerged);
}

#[test]
fn dequeue_denied_propagates() {
    let mut api = FakeApi::with(vec![st(PrState::Open, APPROVED, Some(APPROVED))]);
    api.dequeue_result = Some(Err(QueueError::Denied { detail: "x".into() }));
    assert_eq!(dequeue(&api, 7).unwrap_err().code(), "DENIED");
}

// --- regression: direct mode never reaches the queue API --------------------

#[test]
fn direct_mode_never_invokes_the_queue_api() {
    let api = FakeApi::with(vec![]);
    let e = guarded_enqueue(MergeMode::Direct, true, &api, 7, APPROVED).unwrap_err();
    assert_eq!(e, QueueError::NotQueueMode);
    let e = guarded_dequeue(MergeMode::Direct, true, &api, 7).unwrap_err();
    assert_eq!(e, QueueError::NotQueueMode);
    assert!(api.calls().is_empty(), "{:?}", api.calls());
}

#[test]
fn queue_mode_is_dormant_in_this_phase() {
    const { assert!(!QUEUE_EXECUTION_ENABLED, "Phase A must ship dormant (#9978)") };
    let api = FakeApi::with(vec![]);
    let e =
        guarded_enqueue(MergeMode::Queue, QUEUE_EXECUTION_ENABLED, &api, 7, APPROVED).unwrap_err();
    assert_eq!(e.code(), "EXECUTION_DORMANT");
    assert!(
        e.to_string()
            .contains("nothing fell back to a direct merge"),
        "{e}"
    );
    assert!(api.calls().is_empty());
}

// --- the fake `gh` ----------------------------------------------------------

/// A fake `gh` that logs every argv to `calls.log` and answers by matching
/// the GraphQL document: `(needle, exit_code, stdout, stderr)`.
fn fake_gh(dir: &Path, arms: &[(&str, i32, &str, &str)]) -> String {
    let mut script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n",
        dir.join("calls.log").display()
    );
    for (needle, code, out, err) in arms {
        script.push_str(&format!(
            "  *\"{needle}\"*) cat <<'EOF'\n{out}\nEOF\n  printf '%s\\n' '{err}' >&2; exit {code} ;;\n"
        ));
    }
    script.push_str("  *) echo 'unexpected call' >&2; exit 9 ;;\nesac\n");
    let path = dir.join("gh");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path.to_str().unwrap().to_string()
}

fn calls_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

fn status_json(head: &str, queued: bool) -> String {
    let entry = if queued {
        format!(r#"{{"state":"QUEUED","position":1,"headCommit":{{"oid":"{head}"}}}}"#)
    } else {
        "null".to_string()
    };
    format!(
        r#"{{"data":{{"repository":{{"pullRequest":{{"id":"PR_kw","state":"OPEN","headRefOid":"{head}","mergeQueueEntry":{entry}}}}}}}}}"#
    )
}

#[test]
fn github_enqueue_sends_expected_head_oid_and_never_jump() {
    let dir = tempdir().unwrap();
    let status = status_json(APPROVED, false);
    let gh = fake_gh(
        dir.path(),
        &[
            (
                "enqueuePullRequest",
                0,
                r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"state":"QUEUED","position":3}}}}"#,
                "",
            ),
            ("pullRequest(number", 0, &status, ""),
        ],
    );
    let api = GhQueueApi::new(&gh, "acme/widgets").unwrap();
    assert_eq!(
        enqueue(&api, 7, APPROVED).unwrap(),
        EnqueueOutcome::Enqueued { position: Some(3) }
    );
    let log = calls_log(dir.path());
    assert!(log.contains(&format!("expectedHeadOid={APPROVED}")), "{log}");
    assert!(log.contains("pullRequestId=PR_kw"), "{log}");
    assert!(log.contains("$expectedHeadOid: GitObjectID!"), "{log}");
    assert!(!log.contains("jump"), "{log}");
}

#[test]
fn github_already_queued_and_not_queued_texts_are_idempotent() {
    let dir = tempdir().unwrap();
    let gh = fake_gh(
        dir.path(),
        &[
            ("enqueuePullRequest", 1, "", "gh: Pull request is already in the merge queue"),
            ("dequeuePullRequest", 1, "", "gh: Pull request is not in the merge queue"),
        ],
    );
    let api = GhQueueApi::new(&gh, "acme/widgets").unwrap();
    assert_eq!(api.enqueue(7, "PR_kw", APPROVED).unwrap(), EnqueueAck::AlreadyQueued);
    assert_eq!(api.dequeue(7, "PR_kw").unwrap(), DequeueAck::NotQueued);
}

#[test]
fn github_denied_error_is_classified_and_redacts_tokens() {
    let dir = tempdir().unwrap();
    let gh = fake_gh(
        dir.path(),
        &[(
            "enqueuePullRequest",
            1,
            "",
            "gh: Resource not accessible by integration (token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345)",
        )],
    );
    let api = GhQueueApi::new(&gh, "acme/widgets").unwrap();
    let e = api.enqueue(7, "PR_kw", APPROVED).unwrap_err();
    assert_eq!(e.code(), "DENIED");
    let msg = e.to_string();
    assert!(!msg.contains("ghp_ABCDEFGH"), "{msg}");
    assert!(msg.contains("<redacted-"), "{msg}");
}

#[test]
fn github_status_reads_queue_entry_and_missing_pr() {
    let dir = tempdir().unwrap();
    let status = status_json(APPROVED, true);
    let gh = fake_gh(dir.path(), &[("pullRequest(number", 0, &status, "")]);
    let api = GhQueueApi::new(&gh, "acme/widgets").unwrap();
    let s = api.status(7).unwrap();
    assert_eq!(s.node_id, "PR_kw");
    assert_eq!(s.entry.unwrap().head_oid.as_deref(), Some(APPROVED));

    let dir = tempdir().unwrap();
    let gh = fake_gh(
        dir.path(),
        &[("pullRequest(number", 0, r#"{"data":{"repository":{"pullRequest":null}}}"#, "")],
    );
    let api = GhQueueApi::new(&gh, "acme/widgets").unwrap();
    assert_eq!(api.status(7).unwrap_err(), QueueError::NotFound { pr: 7 });
}

#[test]
fn graphql_documents_keep_fields_separated() {
    use super::github::{DEQUEUE_MUTATION, ENQUEUE_MUTATION, STATUS_QUERY};
    // Every selected field must stand alone (regression: a `\` continuation
    // once fused `headRefOid` and `mergeQueueEntry`).
    let words: Vec<&str> = STATUS_QUERY
        .split(|c: char| !c.is_ascii_alphanumeric())
        .collect();
    for field in [
        "id",
        "state",
        "headRefOid",
        "mergeQueueEntry",
        "position",
        "headCommit",
        "oid",
    ] {
        assert!(words.contains(&field), "{field} missing from {STATUS_QUERY}");
    }
    assert!(ENQUEUE_MUTATION.contains("expectedHeadOid: $expectedHeadOid"));
    assert!(!ENQUEUE_MUTATION.contains("jump"));
    assert!(DEQUEUE_MUTATION.contains("dequeuePullRequest(input: { id: $id })"));
}

#[test]
fn failure_texts_classify_rate_limit_before_denial() {
    assert_eq!(
        classify_failure("HTTP 403: You have exceeded a secondary rate limit"),
        FailureClass::RateLimited
    );
    assert_eq!(classify_failure("HTTP 403: Forbidden"), FailureClass::Denied);
    assert_eq!(
        classify_failure("Expected head OID did not match: head branch was modified"),
        FailureClass::HeadMismatch
    );
    assert_eq!(
        classify_failure("Merge queue is not enabled for this branch"),
        FailureClass::QueueUnavailable
    );
    assert_eq!(classify_failure("something else"), FailureClass::Other);
}

// --- preflight ---------------------------------------------------------------

fn rule(kind: &str, params: serde_json::Value) -> BranchRule {
    BranchRule {
        kind: kind.into(),
        ruleset_id: Some(42),
        parameters: Some(params),
    }
}

fn org() -> RepoFacts {
    RepoFacts {
        owner: Some(Owner {
            kind: Some("Organization".into()),
        }),
        ..RepoFacts::default()
    }
}

fn checks() -> BranchRule {
    rule(
        "required_status_checks",
        json!({"required_status_checks": [{"context": "ci / test"}, {"context": "ci / lint"}]}),
    )
}

#[test]
fn preflight_capable_branch() {
    let c =
        evaluate_capability("main", &org(), &[rule("merge_queue", json!({})), checks()]).unwrap();
    assert_eq!(c.queue_rulesets, vec![42]);
    assert_eq!(c.required_checks, vec!["ci / lint", "ci / test"]);
}

#[test]
fn preflight_failure_kinds_are_distinct() {
    let code = |r: Result<Capability, CapabilityError>| r.unwrap_err().code();
    assert_eq!(code(evaluate_capability("main", &org(), &[checks()])), "MISSING_QUEUE_RULE");
    assert_eq!(
        code(evaluate_capability("main", &org(), &[rule("merge_queue", json!({}))])),
        "MISSING_REQUIRED_CHECKS"
    );
    let user = RepoFacts {
        owner: Some(Owner {
            kind: Some("User".into()),
        }),
        ..RepoFacts::default()
    };
    assert_eq!(code(evaluate_capability("main", &user, &[checks()])), "UNSUPPORTED_REPOSITORY");
    // Same verdict as `merge-group-ci eligibility` (#10257): a user-owned
    // repository is unsupported even if a queue rule somehow appears.
    assert_eq!(
        code(evaluate_capability("main", &user, &[rule("merge_queue", json!({})), checks()])),
        "UNSUPPORTED_REPOSITORY"
    );
    let archived = RepoFacts {
        archived: true,
        ..org()
    };
    assert_eq!(code(evaluate_capability("main", &archived, &[])), "UNSUPPORTED_REPOSITORY");
}

#[test]
fn preflight_unreadable_rules_are_inaccessible_not_a_verdict() {
    let dir = tempdir().unwrap();
    let gh = fake_gh(
        dir.path(),
        &[
            ("rules/branches", 1, "", "gh: Not Found (HTTP 404)"),
            (
                "repos/acme/widgets",
                0,
                r#"{"default_branch":"main","owner":{"type":"Organization"}}"#,
                "",
            ),
        ],
    );
    let e = github_preflight(&gh, "acme/widgets", None).unwrap_err();
    assert_eq!(e.code(), "CONFIG_INACCESSIBLE");
    assert_eq!(capability_exit_code(&e), 3);

    let dir = tempdir().unwrap();
    let gh = fake_gh(
        dir.path(),
        &[("repos/acme/widgets", 1, "", "gh: API rate limit exceeded (HTTP 403)")],
    );
    assert_eq!(
        github_preflight(&gh, "acme/widgets", None)
            .unwrap_err()
            .code(),
        "RATE_LIMITED"
    );
}

#[test]
fn preflight_reads_rules_from_the_effective_rules_endpoint() {
    let dir = tempdir().unwrap();
    let rules = r#"[{"type":"merge_queue","ruleset_id":9,"parameters":{}},{"type":"required_status_checks","ruleset_id":9,"parameters":{"required_status_checks":[{"context":"build"}]}}]"#;
    let gh = fake_gh(
        dir.path(),
        &[
            ("rules/branches/trunk", 0, rules, ""),
            (
                "repos/acme/widgets",
                0,
                r#"{"default_branch":"trunk","owner":{"type":"Organization"}}"#,
                "",
            ),
        ],
    );
    let c = github_preflight(&gh, "acme/widgets", None).unwrap();
    assert_eq!(
        (c.branch.as_str(), c.required_checks.clone()),
        ("trunk", vec!["build".to_string()])
    );
}

// --- CLI surface ---------------------------------------------------------------

fn env(gh: String, mode: MergeMode, forge: ForgeType) -> Env {
    Env {
        forge,
        gh,
        default_repo: Some("acme/widgets".into()),
        mode: Ok(ResolvedMergeMode {
            mode,
            source: MergeModeSource::Config,
        }),
        execution_enabled: QUEUE_EXECUTION_ENABLED,
    }
}

#[test]
fn cli_direct_mode_enqueue_never_spawns_gh() {
    let dir = tempdir().unwrap();
    let gh = fake_gh(dir.path(), &[]);
    let cmd = MergeQueueCmd::Enqueue {
        pr: 7,
        approved_sha: APPROVED.into(),
        repo: None,
    };
    let r = run(&cmd, &env(gh.clone(), MergeMode::Direct, ForgeType::GitHub));
    assert_eq!(r.code, 4);
    assert!(r.stderr[0].contains("[NOT_QUEUE_MODE]"), "{r:?}");
    let r = run(
        &MergeQueueCmd::Dequeue { pr: 7, repo: None },
        &env(gh.clone(), MergeMode::Queue, ForgeType::GitHub),
    );
    assert!(r.stderr[0].contains("[EXECUTION_DORMANT]"), "{r:?}");
    assert_eq!(calls_log(dir.path()), "", "gh must never be invoked");
}

#[test]
fn cli_mode_and_invalid_config_and_gitea() {
    let r = run(&MergeQueueCmd::Mode, &env("gh".into(), MergeMode::Direct, ForgeType::GitHub));
    assert_eq!(r.stdout, vec!["merge-queue: mode=direct source=config execution=dormant"]);

    let mut bad = env("gh".into(), MergeMode::Direct, ForgeType::GitHub);
    bad.mode = Err(MergeModeError {
        source: MergeModeSource::Config,
        value: "\"queued\"".into(),
    });
    let r = run(&MergeQueueCmd::Mode, &bad);
    assert_eq!(r.code, 2);
    assert!(r.stderr[0].contains("[INVALID_MERGE_MODE]"), "{r:?}");

    let r = run(
        &MergeQueueCmd::Preflight {
            repo: None,
            branch: None,
        },
        &env("gh".into(), MergeMode::Queue, ForgeType::Gitea),
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr[0].contains("[UNSUPPORTED_FORGE]"), "{r:?}");
}

#[test]
fn cli_status_with_fake_forge() {
    let dir = tempdir().unwrap();
    let status = status_json(APPROVED, true);
    let gh = fake_gh(dir.path(), &[("pullRequest(number", 0, &status, "")]);
    let r = run(
        &MergeQueueCmd::Status { pr: 7, repo: None },
        &env(gh, MergeMode::Direct, ForgeType::GitHub),
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.stdout[0].contains("queued state=QUEUED position=1"), "{r:?}");
}
