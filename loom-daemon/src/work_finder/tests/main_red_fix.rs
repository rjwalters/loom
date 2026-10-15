//! The red-main-fix lane (#9244 §6): a halted repo admits only its fixes on
//! a verified-red `main`, and the CI fallback when the gate is disabled.
//! #11547: while `main` is verified red the fix leads the dispatch order; on
//! a green `main` the marker reorders nothing.

use super::*;
use crate::work_finder::main_red_fix::latest_run_is_failure;

fn fix(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into()])
        .with_body(Some("Fixes red main.\n\n<!-- loom:main-red-fix -->\n".into()))
}

#[test]
fn the_marker_is_line_anchored() {
    assert!(fix(1).is_main_red_fix());
    assert!(!issue(1).is_main_red_fix());
    let quoted = issue(2).with_body(Some("Add `<!-- loom:main-red-fix -->` to it".into()));
    assert!(!quoted.is_main_red_fix(), "prose quoting the marker does not fire");
}

#[test]
fn with_the_gate_disabled_ci_decides_red() {
    let gate_off = RedMainLane {
        gate_disabled: true,
        ..RedMainLane::default()
    };
    // #11508 removed the ordering boost this used to be observed through;
    // what CI decides now is whether `main` counts as red (which admits an
    // unpromoted fix, see the `..._unpromoted_fix_is_admitted` test).
    assert!(gate_off.is_red(&[issue(1), fix(9)], || true), "CI red");
    assert!(!gate_off.is_red(&[issue(1), fix(9)], || false), "CI green");
    // The CI read happens only when a marker-bearing candidate exists.
    let mut asked = false;
    assert!(!gate_off.is_red(&[issue(1)], || {
        asked = true;
        true
    }));
    assert!(!asked, "no marker, no forge read");
    // An enabled gate never consults CI.
    assert!(!RedMainLane::default().is_red(&[fix(9)], || true));
}

#[test]
fn the_ci_fallback_reads_the_newest_commits_runs() {
    let red = r#"[
        {"headSha":"new","status":"completed","conclusion":"failure","workflowName":"CI"},
        {"headSha":"new","status":"completed","conclusion":"success","workflowName":"Lint"},
        {"headSha":"old","status":"completed","conclusion":"success","workflowName":"CI"}
    ]"#;
    assert!(latest_run_is_failure(red));
    let green = r#"[
        {"headSha":"new","status":"completed","conclusion":"success","workflowName":"CI"},
        {"headSha":"old","status":"completed","conclusion":"failure","workflowName":"CI"}
    ]"#;
    assert!(!latest_run_is_failure(green), "an older red commit is not today's main");
    assert!(!latest_run_is_failure("[]"));
    assert!(!latest_run_is_failure("not json"));
}

#[test]
fn the_ci_fallback_reads_the_repos_default_branch() {
    use crate::work_finder::main_red_fix::default_branch_for;
    let git = |dir: &std::path::Path, args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    // No `origin/HEAD`: fall back to `main`.
    assert_eq!(default_branch_for(dir.path()), "main");
    git(
        dir.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    assert_eq!(default_branch_for(dir.path()), "trunk");
}

// ---- #10118: unpromoted fixes (triage / curated) and escalation ----

use crate::comment_trust::{Author, TrustPolicy};
use crate::forge_identity::FleetLogins;
use crate::work_finder::main_red_fix::{merge_red_fix_candidates, RedFixWatch};

const MARKER_BODY: &str = "Fixes red main.\n\n<!-- loom:main-red-fix -->\n";

/// A marker-bearing fix still carrying `label` (`loom:triage` / `loom:curated`),
/// filed by the repo owner (a trusted author, #9548).
fn unpromoted(n: u32, label: &str) -> WorkItem {
    unpromoted_by(n, label, Author::new(Some("rjwalters"), Some("OWNER")))
}

/// [`unpromoted`], filed by `author`.
fn unpromoted_by(n: u32, label: &str, author: Author) -> WorkItem {
    WorkItem::new(n, vec![label.into()])
        .with_body(Some(MARKER_BODY.into()))
        .with_author(Some(author))
}

/// The trust rules a repo with one fleet App (`loom-fleet-dispatch`) resolves
/// to: insiders by association, the fleet App, nothing else.
fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::single("loom-fleet-dispatch"), None, Vec::new())
}

/// [`merge_red_fix_candidates`] under [`policy`].
fn merge(ready: Vec<WorkItem>, rows: Vec<WorkItem>) -> Vec<WorkItem> {
    let policy = policy();
    merge_red_fix_candidates(ready, rows, |a| policy.trusts(a))
}

#[test]
fn the_unpromoted_listing_keeps_only_unclaimed_marker_rows() {
    let ready = vec![fix(1)];
    let mut rows = vec![
        unpromoted(1, "loom:triage"), // already listed as loom:issue
        unpromoted(2, "loom:triage"),
        WorkItem::new(3, vec!["loom:triage".into()]), // no marker
        issue(4).with_body(Some("Quote `<!-- loom:main-red-fix -->` here".into())),
    ];
    for (n, extra) in [
        (5, "loom:building"),
        (6, "loom:curating"),
        (7, "loom:epic"),
        (8, "loom:architect"),
        (9, "loom:hermit"),
        (10, "loom:auditor"),
    ] {
        let mut row = unpromoted(n, "loom:curated");
        row.labels.push(extra.into());
        rows.push(row);
    }
    let merged = merge(ready, rows);
    let numbers: Vec<u32> = merged.iter().map(|i| i.number).collect();
    assert_eq!(numbers, vec![1, 2]);
    // The second listing dedups against the first.
    let merged = merge(merged, vec![unpromoted(2, "loom:curated")]);
    assert_eq!(merged.len(), 2);
    // A promoted fix is not "unpromoted": it is a candidate anyway. A
    // priority label does not promote it (#11508): it still needs a red main.
    assert!(!fix(1).is_unpromoted_red_fix());
    for label in ["loom:important", "loom:very-important"] {
        let mut prioritised = unpromoted(2, "loom:triage");
        prioritised.labels.push(label.into());
        assert!(prioritised.is_unpromoted_red_fix(), "{label}");
    }
}

/// The trust policy is consulted only for marker-bearing rows that survive
/// the other filters, so a repo with no such row never resolves one.
#[test]
fn trust_is_consulted_only_for_marker_rows() {
    let mut asked = Vec::new();
    let rows = vec![
        WorkItem::new(1, vec!["loom:triage".into()]),
        unpromoted_by(2, "loom:triage", Author::new(Some("mallory"), Some("NONE"))),
    ];
    let merged = merge_red_fix_candidates(Vec::new(), rows, |a| {
        asked.push(a.login.clone());
        false
    });
    assert!(merged.is_empty());
    assert_eq!(asked, vec![Some("mallory".to_string())]);
}

#[test]
fn the_watch_alerts_once_per_fix_past_the_threshold() {
    let after = std::time::Duration::from_secs(1800);
    let t0 = std::time::Instant::now();
    let at = |mins: u64| t0 + std::time::Duration::from_secs(mins * 60);
    let mut watch = RedFixWatch::default();
    assert!(watch.observe(at(0), true, &[9], after).is_empty());
    assert!(watch.observe(at(29), true, &[9], after).is_empty());
    let due = watch.observe(at(30), true, &[9], after);
    assert_eq!(due.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![9]);
    watch.record_attempt(at(30), 9, true);
    assert!(watch.observe(at(45), true, &[9], after).is_empty(), "once");

    // Claimed (left the waiting set), then released again: the clock restarts.
    assert!(watch.observe(at(46), true, &[], after).is_empty());
    assert!(watch.observe(at(47), true, &[9], after).is_empty());
    assert!(watch.observe(at(76), true, &[9], after).is_empty());
    assert_eq!(watch.observe(at(77), true, &[9], after).len(), 1);
    watch.record_attempt(at(77), 9, true);

    // A green tick resets everything; a green repo never alerts.
    let mut watch = RedFixWatch::default();
    assert!(watch.observe(at(0), true, &[9], after).is_empty());
    assert!(watch.observe(at(20), false, &[9], after).is_empty());
    assert!(watch.observe(at(40), true, &[9], after).is_empty(), "clock restarted at 40");
    assert!(watch.observe(at(100), false, &[9], after).is_empty());
}

#[test]
fn a_failed_alert_filing_is_retried_with_backoff_until_it_succeeds() {
    let after = std::time::Duration::from_secs(30 * 60);
    let t0 = std::time::Instant::now();
    let at = |secs: u64| t0 + std::time::Duration::from_secs(secs);
    let mut watch = RedFixWatch::default();
    assert!(watch.observe(at(0), true, &[9], after).is_empty());
    assert_eq!(watch.observe(at(1800), true, &[9], after).len(), 1);
    watch.record_attempt(at(1800), 9, false); // sink failed once

    assert!(watch.observe(at(1830), true, &[9], after).is_empty(), "backing off 60s");
    assert_eq!(watch.observe(at(1860), true, &[9], after).len(), 1, "retried");
    watch.record_attempt(at(1860), 9, false); // failed again: 120s
    assert!(watch.observe(at(1950), true, &[9], after).is_empty());
    assert_eq!(watch.observe(at(1980), true, &[9], after).len(), 1);
    watch.record_attempt(at(1980), 9, true); // finally filed

    assert!(watch.observe(at(2100), true, &[9], after).is_empty(), "no refile after success");
    assert!(watch.observe(at(9000), true, &[9], after).is_empty());
}
