//! Unit tests for the required-check freshness guard (#8248).
//!
//! The two timeline tests reproduce the 2026-09-18 incident exactly — a green
//! `File Size Ratchet` run 22h before a baseline-tightening merge — because a
//! guard about *freshness* is exactly the kind of thing that regresses by
//! someone "simplifying" a comparison, and the incident's timestamps are the
//! sharpest pin available.

use super::*;

fn run(name: &str, status: &str, conclusion: Option<&str>, started: Option<&str>) -> CheckRun {
    CheckRun {
        name: name.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(String::from),
        started_at: started.map(|s| s.parse::<DateTime<Utc>>().expect("test timestamp parses")),
        actions_run_id: None,
    }
}

fn ctx(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

// --- The incident, replayed -------------------------------------------------

// 2026-09-17T22:54:20Z: #8078's File Size Ratchet runs green.
const INCIDENT_RUN_STARTED: &str = "2026-09-17T22:54:20Z";
// 2026-09-18T11:45:21Z: #8204's merge tightens the baseline; this is main's tip.
const INCIDENT_BASE_TIP: &str = "2026-09-18T11:45:21Z";

#[test]
fn incident_timeline_green_run_predating_the_tightened_tip_is_stale() {
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![run(
        "File Size Ratchet",
        "completed",
        Some("success"),
        Some(INCIDENT_RUN_STARTED),
    )];
    let verdict = assess(base_tip, &ctx(&["File Size Ratchet"]), &runs);
    assert_eq!(
        verdict,
        Verdict::Stale {
            check: "File Size Ratchet".to_string(),
            started_at: INCIDENT_RUN_STARTED.parse().unwrap(),
            base_tip,
        },
        "a green run 22h before the base tip must not count as evidence"
    );
}

#[test]
fn incident_message_names_the_check_and_both_timestamps() {
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let started: DateTime<Utc> = INCIDENT_RUN_STARTED.parse().unwrap();
    let msg = stale_message("8078", "File Size Ratchet", started, base_tip, "abc123");
    // chrono's Display renders RFC3339 as "2026-09-17 22:54:20 UTC" — assert
    // the rendered form, which is what an operator actually reads.
    for needle in [
        "File Size Ratchet",
        "2026-09-17 22:54:20 UTC",
        "2026-09-18 11:45:21 UTC",
        "abc123",
    ] {
        assert!(msg.contains(needle), "message must name {needle}: {msg}");
    }
}

// --- Fresh cases ------------------------------------------------------------

#[test]
fn run_started_after_the_tip_is_fresh() {
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![run(
        "File Size Ratchet",
        "completed",
        Some("success"),
        Some("2026-09-18T12:00:00Z"),
    )];
    assert_eq!(assess(base_tip, &ctx(&["File Size Ratchet"]), &runs), Verdict::Fresh);
}

#[test]
fn run_started_exactly_at_the_tip_is_fresh() {
    // "before" is strict: a run that started at the very moment the tip landed
    // verified the world the merge will produce, not a older one.
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![run(
        "File Size Ratchet",
        "completed",
        Some("success"),
        Some(INCIDENT_BASE_TIP),
    )];
    assert_eq!(assess(base_tip, &ctx(&["File Size Ratchet"]), &runs), Verdict::Fresh);
}

#[test]
fn a_fresh_re_run_shadows_an_older_stale_one() {
    // Re-running the job re-dates it; branch protection (and the Checks tab)
    // evaluate the LATEST run per context, so the guard must too.
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![
        run("File Size Ratchet", "completed", Some("success"), Some(INCIDENT_RUN_STARTED)),
        run("File Size Ratchet", "completed", Some("success"), Some("2026-09-18T13:00:00Z")),
    ];
    assert_eq!(assess(base_tip, &ctx(&["File Size Ratchet"]), &runs), Verdict::Fresh);
}

#[test]
fn no_required_checks_is_fresh() {
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![run(
        "File Size Ratchet",
        "completed",
        Some("success"),
        Some(INCIDENT_RUN_STARTED),
    )];
    assert_eq!(assess(base_tip, &[], &runs), Verdict::Fresh);
    // Only REQUIRED contexts gate: a stale green informational check is not
    // merge evidence and is not this guard's business.
    assert_eq!(
        assess(base_tip, &ctx(&["Unrequired Job"]), &runs),
        Verdict::Fresh,
        "an informational (non-required) stale green must not block"
    );
}

#[test]
fn required_context_with_no_run_is_left_to_the_forge() {
    // Nothing green exists to re-validate; branch protection's own BLOCKED
    // state owns that refusal (one mechanism per behaviour).
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![run(
        "Some Other Job",
        "completed",
        Some("success"),
        Some(INCIDENT_RUN_STARTED),
    )];
    assert_eq!(assess(base_tip, &ctx(&["File Size Ratchet"]), &runs), Verdict::Fresh);
}

#[test]
fn pending_and_failing_runs_are_ignored() {
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![
        run("Queued Job", "queued", None, None),
        run("Running Job", "in_progress", None, Some(INCIDENT_RUN_STARTED)),
        run("Failed Job", "completed", Some("failure"), Some(INCIDENT_RUN_STARTED)),
        run("Skipped Job", "completed", Some("skipped"), Some(INCIDENT_RUN_STARTED)),
        run("Neutral Job", "completed", Some("neutral"), Some(INCIDENT_RUN_STARTED)),
    ];
    let required = ctx(&[
        "Queued Job",
        "Running Job",
        "Failed Job",
        "Skipped Job",
        "Neutral Job",
    ]);
    // None of these is green evidence, so none can be STALE-green; and a
    // non-green run with a known start is not an unknown either.
    assert_eq!(assess(base_tip, &required, &runs), Verdict::Fresh);
}

// --- Fail-closed cases ------------------------------------------------------

#[test]
fn green_run_without_started_at_is_unknown() {
    // Completed+success but no start time: the evidence exists and its age
    // cannot be determined. Failing open here is precisely how a degraded API
    // day would re-open the incident.
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![run("File Size Ratchet", "completed", Some("success"), None)];
    match assess(base_tip, &ctx(&["File Size Ratchet"]), &runs) {
        Verdict::Unknown(why) => {
            assert!(why.contains("File Size Ratchet"), "reason names the check: {why}");
        }
        v => panic!("expected Unknown, got {v:?}"),
    }
}

#[test]
fn unknown_message_tells_the_caller_to_refuse() {
    let msg = unknown_message("8078", "base tip commit time unresolvable");
    assert!(msg.contains("Merge blocked"), "{msg}");
    assert!(msg.contains("base tip commit time unresolvable"), "{msg}");
}

// --- Ordering determinism ---------------------------------------------------

#[test]
fn verdict_is_invariant_under_run_order_and_requires_sorted_iteration() {
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let ratchet = run("A Ratchet", "completed", Some("success"), Some(INCIDENT_RUN_STARTED));
    let lint = run("Z Lint", "completed", Some("success"), Some(INCIDENT_RUN_STARTED));
    let required = ctx(&["Z Lint", "A Ratchet"]);
    // Both stale: the FIRST in sorted context order is reported, regardless of
    // the runs' API order.
    for runs in [vec![ratchet.clone(), lint.clone()], vec![lint, ratchet]] {
        assert_eq!(
            assess(base_tip, &required, &runs),
            Verdict::Stale {
                check: "A Ratchet".to_string(),
                started_at: INCIDENT_RUN_STARTED.parse().unwrap(),
                base_tip,
            }
        );
    }
}

#[test]
fn stale_beats_unknown_when_both_exist() {
    // A concrete stale finding is the actionable refusal; an unknown elsewhere
    // still refuses if nothing is stale.
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![
        run("A Stale", "completed", Some("success"), Some(INCIDENT_RUN_STARTED)),
        run("Z Unknown", "completed", Some("success"), None),
    ];
    assert_eq!(
        assess(base_tip, &ctx(&["A Stale", "Z Unknown"]), &runs),
        Verdict::Stale {
            check: "A Stale".to_string(),
            started_at: INCIDENT_RUN_STARTED.parse().unwrap(),
            base_tip,
        }
    );
    // And with the stale one removed, the unknown refuses on its own.
    let runs = vec![run("Z Unknown", "completed", Some("success"), None)];
    assert!(matches!(assess(base_tip, &ctx(&["Z Unknown"]), &runs), Verdict::Unknown(_)));
}

#[test]
fn timestamps_with_fractional_seconds_parse() {
    // GitHub sometimes emits milliseconds; the guard must not misread the one
    // timestamp class it exists to compare.
    let base_tip: DateTime<Utc> = "2026-09-18T11:45:21.500Z".parse().unwrap();
    let runs = vec![run(
        "File Size Ratchet",
        "completed",
        Some("success"),
        Some("2026-09-18T11:45:21.000Z"),
    )];
    assert!(matches!(
        assess(base_tip, &ctx(&["File Size Ratchet"]), &runs),
        Verdict::Stale { .. }
    ));
}

// --- The plan gate (#8844) --------------------------------------------------
//
// `fetch::is_plan_gated` decides which lookup failures are a FACT ("this
// repository's plan has no rulesets, so nothing can be required") rather than
// an unknown. Everything else must keep failing closed, so the predicate is
// pinned from both sides: the real message in the messages `gh` actually
// prints, and the neighbouring 403/401/429/5xx/404 classes that share the
// status code but not the cause.

/// What `gh api` prints on stderr for the plan-gated 403, wrapped the way
/// `fetch::gh_api` reports it (the predicate sees that whole string).
const PLAN_GATED_STDERR: &str = "gh api repos/acme/private/rules/branches/main failed: \
gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)";

#[test]
fn the_free_plan_403_is_recognised_as_a_plan_gate() {
    assert!(fetch::is_plan_gated(PLAN_GATED_STDERR), "{PLAN_GATED_STDERR}");
    // Org-owned repos get the Team/Enterprise wording; the invariant half of
    // the message ("make this repository public") is what carries the meaning.
    assert!(fetch::is_plan_gated(
        "gh: Upgrade to GitHub Team or make this repository public to enable this feature. (HTTP 403)"
    ));
    // Case is not load-bearing.
    assert!(fetch::is_plan_gated(
        "UPGRADE TO GITHUB PRO OR MAKE THIS REPOSITORY PUBLIC TO ENABLE THIS FEATURE."
    ));
}

#[test]
fn every_other_failure_class_is_not_a_plan_gate() {
    // Each of these is a real `gh` failure mode that must keep failing the
    // guard CLOSED — several of them are 403s too, which is exactly why the
    // predicate matches the message and not the status code.
    for err in [
        "gh: Resource not accessible by integration (HTTP 403)",
        "gh: Must have admin rights to Repository. (HTTP 403)",
        "gh: API rate limit exceeded for user ID 1234. (HTTP 403)",
        "gh: Although you appear to have the correct authorization credentials, the \
         organization has enabled OAuth App access restrictions (HTTP 403)",
        "gh: Bad credentials (HTTP 401)",
        "gh: Not Found (HTTP 404)",
        "gh: Server Error (HTTP 500)",
        "could not exec gh api: No such file or directory (os error 2)",
        "exit 1",
        "",
    ] {
        assert!(!fetch::is_plan_gated(err), "must fail closed on: {err}");
    }
}

#[test]
fn half_the_signature_is_not_enough() {
    // A partial match is how a reworded-but-different message would sneak in;
    // both fragments are required.
    assert!(!fetch::is_plan_gated("gh: Upgrade to GitHub Pro (HTTP 403)"));
    assert!(!fetch::is_plan_gated(
        "gh: You must make this repository public before transferring it. (HTTP 422)"
    ));
}

#[test]
fn a_plan_gated_repo_has_no_required_checks_so_a_green_run_is_fresh() {
    // The end state the gate produces: both sources empty -> `required` is
    // empty -> nothing can be stale, which is the verdict #8844 argues is the
    // DEFINITE answer on a plan-gated repo (not a guess).
    let base_tip: DateTime<Utc> = INCIDENT_BASE_TIP.parse().unwrap();
    let runs = vec![run(
        "File Size Ratchet",
        "completed",
        Some("success"),
        Some(INCIDENT_RUN_STARTED),
    )];
    assert_eq!(assess(base_tip, &[], &runs), Verdict::Fresh);
}

#[test]
fn latest_run_picks_max_started_at() {
    let older = run("J", "completed", Some("failure"), Some("2026-09-17T00:00:00Z"));
    let newer = run("J", "completed", Some("success"), Some("2026-09-18T00:00:00Z"));
    assert_eq!(latest_run(&[older, newer.clone()], "J"), Some(&newer));
    // A no-start run ranks oldest even when listed last.
    let queued = run("J", "queued", None, None);
    let green = run("J", "completed", Some("success"), Some("2026-09-16T00:00:00Z"));
    assert_eq!(latest_run(&[queued, green.clone()], "J"), Some(&green));
}
