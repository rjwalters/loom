//! Fixture tests for `merge-group-ci` (#10257): static workflow YAML plus
//! assertions, across the `pull_request` / `push` / `merge_group` contexts.
//! No network; the only `gh` is a local fake that records its argv so the
//! read-only contract can be asserted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use super::audit::{audit, AuditReport, Code};
use super::context::{Event, RunState};
use super::eligibility::{evaluate, probe, Prereq, RepoFacts, Rule};
use super::workflow::{self, Workflow};
use serde_json::json;

fn wf(name: &str, src: &str) -> Workflow {
    workflow::parse(&format!(".github/workflows/{name}"), src).unwrap()
}

fn qualified() -> Workflow {
    wf("ci.yml", include_str!("fixtures/qualified.yml"))
}

fn run(w: &Workflow, required: &[&str]) -> AuditReport {
    let req: Vec<String> = required.iter().map(|s| (*s).to_string()).collect();
    audit(std::slice::from_ref(w), &[], &req)
}

fn codes_for(r: &AuditReport, job: Option<&str>) -> Vec<Code> {
    let mut v: Vec<Code> = r
        .findings
        .iter()
        .filter(|f| f.job.as_deref() == job)
        .map(|f| f.code)
        .collect();
    v.sort();
    v
}

fn state(r: &AuditReport, job: &str, e: Event) -> RunState {
    r.jobs.iter().find(|j| j.id == job).unwrap().states[&e]
}

fn covered(r: &AuditReport, job: &str) -> bool {
    r.jobs.iter().find(|j| j.id == job).unwrap().covered
}

// --- the three event contexts on a qualified workflow ------------------------

#[test]
fn qualified_fixture_has_no_findings_and_covers_every_suite() {
    let r = run(&qualified(), &["Gate", "Tests (1/2)", "Tests (2/2)"]);
    assert!(r.qualified(), "{:#?}", r.findings);
    assert!(covered(&r, "gate"));
    assert!(covered(&r, "tests"));
    assert!(r.required.iter().all(|c| c.covered), "{:#?}", r.required);
}

#[test]
fn qualified_fixture_job_states_per_event() {
    let r = run(&qualified(), &[]);
    // The path-filter helper runs only on PRs and is marked pr-only.
    assert_eq!(state(&r, "changes", Event::PullRequest), RunState::Runs);
    assert_eq!(state(&r, "changes", Event::Push), RunState::Skipped);
    assert_eq!(state(&r, "changes", Event::MergeGroup), RunState::Skipped);
    let changes = r.jobs.iter().find(|j| j.id == "changes").unwrap();
    assert!(!changes.relied_on && !changes.covered);
    assert!(changes.pr_only_marker.is_some());
    // The filtered suite is path-dependent on PRs, unconditional elsewhere.
    assert_eq!(state(&r, "tests", Event::PullRequest), RunState::Unknown);
    assert_eq!(state(&r, "tests", Event::Push), RunState::Runs);
    assert_eq!(state(&r, "tests", Event::MergeGroup), RunState::Runs);
    for e in Event::ALL {
        assert_eq!(state(&r, "gate", e), RunState::Runs, "{e:?}");
    }
}

#[test]
fn required_context_names_are_matrix_expanded() {
    let w = qualified();
    assert_eq!(w.job("tests").unwrap().names, vec!["Tests (1/2)", "Tests (2/2)"]);
}

// --- each finding class ------------------------------------------------------

#[test]
fn missing_merge_group_trigger_is_reported_and_suites_are_uncovered() {
    let r = run(&wf("ci.yml", include_str!("fixtures/missing_trigger.yml")), &["Gate"]);
    assert_eq!(codes_for(&r, None), vec![Code::MissingMergeGroupTrigger]);
    assert_eq!(codes_for(&r, Some("gate")), vec![Code::RequiredSuiteUncovered]);
    assert!(!covered(&r, "gate"));
    assert_eq!(state(&r, "gate", Event::MergeGroup), RunState::Skipped);
    assert_eq!(state(&r, "gate", Event::Push), RunState::Runs);
}

#[test]
fn trigger_with_types_excluding_checks_requested_does_not_count() {
    let src = "on:\n  pull_request:\n  merge_group:\n    types: [destroyed]\njobs:\n  a:\n    steps: []\n";
    let r = run(&wf("x.yml", src), &[]);
    assert_eq!(codes_for(&r, None), vec![Code::MissingMergeGroupTrigger]);
}

#[test]
fn pr_only_conditions_path_filters_and_skipped_dependencies() {
    let r = run(&wf("ci.yml", include_str!("fixtures/pr_only.yml")), &[]);
    assert_eq!(codes_for(&r, Some("pr-gate")), vec![Code::PrOnlyCondition]);
    // `labels.*.name` cannot be resolved statically: unproven is uncovered.
    assert_eq!(codes_for(&r, Some("labelled")), vec![Code::UndeterminedCondition]);
    assert_eq!(codes_for(&r, Some("filtered")), vec![Code::PathFilterSkip]);
    assert_eq!(codes_for(&r, Some("dependent")), vec![Code::SkippedDependency]);
    assert_eq!(codes_for(&r, Some("steps-gate")), vec![Code::PrOnlyStep]);
    // An unmarked PR-only helper is a relied-on suite like any other.
    assert_eq!(codes_for(&r, Some("pr-only-helper")), vec![Code::PrOnlyCondition]);
    // The path-filter job itself runs everywhere it is triggered.
    assert!(covered(&r, "changes"));
    for job in ["pr-gate", "labelled", "filtered", "dependent", "steps-gate"] {
        assert!(!covered(&r, job), "{job} must not count as covered");
    }
    let step = r
        .findings
        .iter()
        .find(|f| f.code == Code::PrOnlyStep)
        .unwrap();
    assert_eq!(step.step.as_deref(), Some("Version guard"));
}

#[test]
fn skipped_suite_never_counts_as_green_coverage() {
    // A required context whose job is skipped on merge_group is reported
    // uncovered even though GitHub would show a skipped check as passing.
    let r = run(&wf("ci.yml", include_str!("fixtures/pr_only.yml")), &["PR Gate"]);
    assert!(!r.qualified());
    let req = &r.required[0];
    assert!(!req.covered);
    assert!(codes_for(&r, Some("pr-gate")).contains(&Code::RequiredSuiteUncovered));
}

#[test]
fn checkout_sha_and_concurrency_findings() {
    let r = run(&wf("ci.yml", include_str!("fixtures/checkout_concurrency.yml")), &[]);
    assert_eq!(
        codes_for(&r, None),
        vec![Code::CancellingConcurrency, Code::SharedConcurrencyGroup]
    );
    assert_eq!(codes_for(&r, Some("head-checkout")), vec![Code::NonMergeGroupCheckout]);
    let head = r
        .findings
        .iter()
        .find(|f| f.job.as_deref() == Some("head-checkout"))
        .unwrap();
    assert!(head.detail.contains("`main`"), "{}", head.detail);
    assert_eq!(codes_for(&r, Some("matrix-checkout")), vec![Code::NonMergeGroupCheckout]);
    // github.sha, an empty PR ref, and another repository are all fine.
    assert!(codes_for(&r, Some("good-checkouts")).is_empty());
    // cancel-in-progress is false on merge_group, but the group is shared.
    assert_eq!(codes_for(&r, Some("deploy-preview")), vec![Code::SharedConcurrencyGroup]);
}

#[test]
fn concurrency_keyed_on_ref_or_run_id_is_unique_on_merge_group() {
    for group in [
        "${{ github.ref }}",
        "x-${{ github.run_id }}",
        "${{ github.sha }}",
    ] {
        let src = format!(
            "on: [pull_request, merge_group]\nconcurrency:\n  group: {group}\n  cancel-in-progress: false\njobs:\n  a:\n    steps: []\n"
        );
        let r = run(&wf("x.yml", &src), &[]);
        assert!(r.qualified(), "{group}: {:#?}", r.findings);
    }
    let src = "on: [pull_request, merge_group]\nconcurrency:\n  group: ci\n  cancel-in-progress: ${{ matrix.x }}\njobs:\n  a:\n    steps: []\n";
    let r = run(&wf("x.yml", src), &[]);
    assert_eq!(
        codes_for(&r, None),
        vec![Code::CancellingConcurrency, Code::SharedConcurrencyGroup]
    );
}

#[test]
fn missing_required_suite_is_reported() {
    let r = run(&qualified(), &["Gate", "Does Not Exist"]);
    assert_eq!(codes_for(&r, None), vec![Code::MissingRequiredSuite]);
    assert!(
        !r.required
            .iter()
            .find(|c| c.context == "Does Not Exist")
            .unwrap()
            .covered
    );
}

#[test]
fn path_filtered_pr_workflow_is_advisory_not_relied_on() {
    let src = "on:\n  pull_request:\n    paths: ['**/*.sh']\njobs:\n  lint:\n    steps: []\n";
    let r = run(&wf("shell-lint.yml", src), &[]);
    assert!(r.qualified());
    assert!(!r.workflows[0].relied_on);
    assert!(r.workflows[0].reason.contains("path-filtered"));
}

#[test]
fn unparseable_workflow_is_a_finding_not_a_silent_pass() {
    let r = audit(&[], &[("x.yml".into(), "boom".into())], &[]);
    assert_eq!(r.findings[0].code, Code::UnparseableWorkflow);
    assert!(!r.qualified());
}

// --- this repository's own workflows -----------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn every_repo_workflow_parses() {
    let loaded = workflow::load_dir(&repo_root()).unwrap();
    assert!(!loaded.is_empty());
    for r in &loaded {
        if let Err((file, why)) = r {
            panic!("{file} did not parse: {why}");
        }
    }
}

#[test]
fn repo_ci_yml_is_merge_group_qualified_for_its_required_checks() {
    let root = repo_root();
    let required = super::configured_required(&root);
    assert!(!required.is_empty(), "branchProtection.requiredStatusChecks is empty");
    let (workflows, bad) = super::load(&root, &[]).unwrap();
    let r = audit(&workflows, &bad, &required);
    assert!(r.qualified(), "{}", super::render_audit(&r, false));
    let ci = r
        .workflows
        .iter()
        .find(|w| w.file.ends_with("/ci.yml"))
        .unwrap();
    assert!(ci.relied_on && ci.merge_group_trigger);
}

#[test]
fn repo_ci_yml_preserves_pull_request_and_push_coverage() {
    // Adding merge_group must not take anything away from PR or push runs:
    // every job except the PR-only path filter still runs on push, and every
    // job may still run on a PR.
    let src = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let w = wf("ci.yml", &src);
    for t in ["push", "pull_request", "merge_group"] {
        assert!(w.trigger(t).is_some(), "ci.yml lost its `{t}` trigger");
    }
    assert!(!w.trigger("pull_request").unwrap().path_filtered);
    let r = run(&w, &[]);
    for j in &r.jobs {
        if j.id == "changes" {
            assert!(j.pr_only_marker.is_some());
            assert_eq!(j.states[&Event::PullRequest], RunState::Runs);
            continue;
        }
        assert_eq!(j.states[&Event::Push], RunState::Runs, "{} no longer runs on push", j.id);
        assert_ne!(
            j.states[&Event::PullRequest],
            RunState::Skipped,
            "{} can no longer run on a PR",
            j.id
        );
        assert_eq!(j.states[&Event::MergeGroup], RunState::Runs, "{} skipped on merge_group", j.id);
    }
}

#[test]
fn audit_flags_the_pre_10257_ci_yml_shape() {
    // Undo each qualification edit on the real file and check the audit
    // notices: the tool must fail on what `main` looked like before #10257.
    let src = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let required = super::configured_required(&repo_root());
    let req: Vec<&str> = required.iter().map(String::as_str).collect();

    let no_trigger = src.replace("  merge_group:\n    types: [checks_requested]\n", "");
    assert_ne!(no_trigger, src);
    let r = run(&wf("ci.yml", &no_trigger), &req);
    assert!(codes_for(&r, None).contains(&Code::MissingMergeGroupTrigger));
    assert!(r.required.iter().all(|c| !c.covered));

    let push_only = src.replace(
        "github.event_name == 'push' || github.event_name == 'merge_group' ||",
        "github.event_name == 'push' ||",
    );
    assert_ne!(push_only, src);
    let r = run(&wf("ci.yml", &push_only), &req);
    let skipped = r
        .findings
        .iter()
        .filter(|f| f.code == Code::PrOnlyCondition)
        .count();
    assert!(skipped >= 10, "expected the path-filtered suites to be flagged, got {skipped}");

    let pr_only_step = src.replace(
        "(github.event_name == 'pull_request' || github.event_name == 'merge_group') }}",
        "github.event_name == 'pull_request' }}",
    );
    assert_ne!(pr_only_step, src);
    let r = run(&wf("ci.yml", &pr_only_step), &req);
    assert!(codes_for(&r, Some("structural-checks")).contains(&Code::PrOnlyStep));
    assert!(codes_for(&r, Some("structural-checks")).contains(&Code::RequiredSuiteUncovered));
}

// --- eligibility: config validation, read-only ------------------------------

fn org_facts(rules: Vec<Rule>) -> RepoFacts {
    RepoFacts {
        repository: Some("acme/widgets".into()),
        owner_type: Some("Organization".into()),
        branch: Some("main".into()),
        can_push: Some(true),
        rules: Some(rules),
    }
}

fn mq_rule() -> Rule {
    Rule {
        kind: "merge_queue".into(),
        ruleset_id: Some(1),
        parameters: Some(json!({"merge_method": "SQUASH", "max_entries_to_build": 5})),
    }
}

fn required_rule(contexts: &[&str]) -> Rule {
    Rule {
        kind: "required_status_checks".into(),
        ruleset_id: Some(1),
        parameters: Some(json!({
            "required_status_checks": contexts.iter().map(|c| json!({"context": c})).collect::<Vec<_>>()
        })),
    }
}

fn prereqs(facts: &RepoFacts, w: &[Workflow]) -> Vec<Prereq> {
    let e = evaluate(facts, w, &[]);
    assert_eq!(e.eligible, e.failures.is_empty());
    e.failures.iter().map(|f| f.prereq).collect()
}

#[test]
fn eligible_when_every_prerequisite_holds() {
    let facts = org_facts(vec![mq_rule(), required_rule(&["Gate", "Tests (1/2)"])]);
    let e = evaluate(&facts, &[qualified()], &[]);
    assert!(e.eligible, "{:#?}", e.failures);
    assert_eq!(e.required_checks, vec!["Gate", "Tests (1/2)"]);
    assert!(e.merge_queue.is_some());
}

#[test]
fn user_owned_repository_is_rejected() {
    let mut facts = org_facts(vec![mq_rule(), required_rule(&["Gate"])]);
    facts.owner_type = Some("User".into());
    assert_eq!(prereqs(&facts, &[qualified()]), vec![Prereq::OwnerNotOrganization]);
}

#[test]
fn missing_queue_rule_is_rejected() {
    let facts = org_facts(vec![required_rule(&["Gate"])]);
    assert_eq!(prereqs(&facts, &[qualified()]), vec![Prereq::NoMergeQueueRule]);
}

#[test]
fn unknown_permission_and_unreadable_rules_are_rejected() {
    let mut facts = org_facts(vec![]);
    facts.can_push = None;
    facts.rules = None;
    facts.owner_type = None;
    assert_eq!(
        prereqs(&facts, &[qualified()]),
        vec![
            Prereq::OwnerTypeUnknown,
            Prereq::PermissionUnknown,
            Prereq::MergeQueueUnknown
        ]
    );
    // An all-unknown facts file (e.g. `{}`) can never come out eligible.
    let empty: RepoFacts = serde_json::from_str("{}").unwrap();
    assert!(!evaluate(&empty, &[qualified()], &[]).eligible);
}

#[test]
fn missing_and_skipped_required_suites_are_rejected() {
    let facts = org_facts(vec![mq_rule(), required_rule(&["Gate", "Nope"])]);
    assert_eq!(prereqs(&facts, &[qualified()]), vec![Prereq::MissingRequiredSuite]);

    let pr_only = wf("ci.yml", include_str!("fixtures/pr_only.yml"));
    let facts = org_facts(vec![mq_rule(), required_rule(&["PR Gate"])]);
    let got = prereqs(&facts, std::slice::from_ref(&pr_only));
    assert!(got.contains(&Prereq::RequiredSuiteUncovered), "{got:?}");
    assert!(got.contains(&Prereq::WorkflowNotQualified), "{got:?}");

    let facts = org_facts(vec![mq_rule()]);
    assert_eq!(prereqs(&facts, &[qualified()]), vec![Prereq::NoRequiredChecks]);
}

#[test]
fn this_repository_shape_is_ineligible() {
    // rjwalters/loom is user-owned (verified 2026-10-04) with no queue rule.
    let facts = RepoFacts {
        repository: Some("rjwalters/loom".into()),
        owner_type: Some("User".into()),
        branch: Some("main".into()),
        can_push: Some(true),
        rules: Some(vec![required_rule(&["Structural Checks"])]),
    };
    let got = prereqs(&facts, &[]);
    assert!(got.contains(&Prereq::OwnerNotOrganization));
    assert!(got.contains(&Prereq::NoMergeQueueRule));
}

/// A fake `gh` that appends its argv to `log` and answers the two reads.
fn fake_gh(dir: &Path, repo_body: Option<&str>, rules_body: Option<&str>) -> (String, PathBuf) {
    let log = dir.join("gh-argv.log");
    let arm = |b: Option<&str>| match b {
        Some(body) => format!("cat <<'EOF'\n{body}\nEOF\nexit 0"),
        None => "echo 'gh: HTTP 403' >&2; exit 1".to_string(),
    };
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$4\" in\n*/rules/branches/*) {} ;;\n*) {} ;;\nesac\n",
        log.display(),
        arm(rules_body),
        arm(repo_body),
    );
    let path = dir.join("fake-gh.sh");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (path.to_str().unwrap().to_string(), log)
}

/// Every recorded call must be an explicit `api --method GET <path>` — no
/// field flags, no input, no other verb.
fn assert_read_only(log: &Path) -> usize {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    for line in text.lines() {
        let argv: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(&argv[..3], &["api", "--method", "GET"], "non-GET call: {line}");
        assert_eq!(argv.len(), 4, "extra arguments on a read: {line}");
    }
    text.lines().count()
}

#[test]
fn probe_makes_only_get_reads_and_maps_facts() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fake_gh(
        dir.path(),
        Some(
            r#"{"owner":{"type":"Organization"},"default_branch":"main","permissions":{"admin":false,"maintain":false,"push":true}}"#,
        ),
        Some(
            r#"[{"type":"merge_queue","ruleset_id":7,"parameters":{"merge_method":"SQUASH"}},{"type":"required_status_checks","ruleset_id":7,"parameters":{"required_status_checks":[{"context":"Gate"}]}}]"#,
        ),
    );
    let facts = probe(&gh, "acme/widgets", None);
    assert_eq!(assert_read_only(&log), 2);
    assert_eq!(facts.owner_type.as_deref(), Some("Organization"));
    assert_eq!(facts.branch.as_deref(), Some("main"));
    assert_eq!(facts.can_push, Some(true));
    assert_eq!(facts.rules.as_ref().unwrap().len(), 2);
    assert!(evaluate(&facts, &[qualified()], &[]).eligible);
}

#[test]
fn probe_failures_fail_closed_with_zero_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) =
        fake_gh(dir.path(), Some(r#"{"owner":{"type":"User"},"default_branch":"main"}"#), None);
    let facts = probe(&gh, "someone/repo", None);
    assert_read_only(&log);
    assert_eq!(facts.can_push, None);
    assert_eq!(facts.rules, None);
    let got = prereqs(&facts, &[qualified()]);
    assert_eq!(
        got,
        vec![
            Prereq::OwnerNotOrganization,
            Prereq::PermissionUnknown,
            Prereq::MergeQueueUnknown
        ]
    );

    // An App installation token reports an all-false permissions object:
    // that is "not reported", so it is PERMISSION_UNKNOWN, not a denial.
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fake_gh(
        dir.path(),
        Some(
            r#"{"owner":{"type":"Organization"},"default_branch":"main","permissions":{"admin":false,"maintain":false,"pull":false,"push":false,"triage":false}}"#,
        ),
        Some("[]"),
    );
    let facts = probe(&gh, "acme/widgets", None);
    assert_read_only(&log);
    assert_eq!(facts.can_push, None);
    assert!(prereqs(&facts, &[qualified()]).contains(&Prereq::PermissionUnknown));

    // A wholly unreadable repository: still only reads, still not eligible.
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fake_gh(dir.path(), None, None);
    let facts = probe(&gh, "acme/hidden", None);
    assert_eq!(assert_read_only(&log), 1, "no branch known, so no rules read");
    assert!(!evaluate(&facts, &[qualified()], &[]).eligible);
}

#[test]
fn evaluate_is_pure() {
    // `evaluate` takes no forge handle at all; this pins that it stays a pure
    // function of its inputs (same facts in, same verdict out).
    let facts = org_facts(vec![mq_rule(), required_rule(&["Gate"])]);
    let a = serde_json::to_value(evaluate(&facts, &[qualified()], &[])).unwrap();
    let b = serde_json::to_value(evaluate(&facts, &[qualified()], &[])).unwrap();
    assert_eq!(a, b);
}
