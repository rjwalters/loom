//! Stale `loom:curating` reconciliation (#14816): the pure lease classifier
//! and decision, one case per keep shape the acceptance criteria name, plus
//! end-to-end runs against a fake `gh` for the write paths — including the
//! #14811 incident's exact label shapes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use chrono::TimeZone;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

use CuratingAction as A;
use CuratingKeepReason as K;

/// A UTC timestamp from `(days_ago, h, m, s)` — ages below are relative to
/// `NOW`, so tests never touch the wall clock.
fn at(days_ago: u32, hour: u32, min: u32, sec: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 21 - days_ago, hour, min, sec)
        .unwrap()
}

fn now() -> DateTime<Utc> {
    at(0, 12, 0, 0)
}

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn row(id: u64, minutes_ago: i64, body: &str) -> RoleLeaseRow {
    RoleLeaseRow {
        id,
        updated_at: now() - chrono::Duration::minutes(minutes_ago),
        body: body.to_string(),
    }
}

const LEASE: &str = "<!-- loom:pr-lease role=curator host=h sweep=s1 -->\nholds";
const LEASE_PEER: &str = "<!-- loom:pr-lease role=curator host=p sweep=s2 -->\nholds";
const RELEASED: &str = "<!-- loom:pr-lease-released role=curator host=h sweep=s1 -->\ndone";
const YIELD: &str =
    "<!-- loom:pr-lease-yield role=curator host=h sweep=s1 earliest_host=b earliest_sweep=s0 -->";

// -----------------------------------------------------------------------
// classify_curator_lease
// -----------------------------------------------------------------------

#[test]
fn a_fresh_lease_is_live_whatever_its_host() {
    for body in [LEASE, LEASE_PEER] {
        let got = classify_curator_lease(&[row(1, 5, body)], now(), 15.0);
        assert_eq!(got, CuratorLeaseEvidence::Live { age_minutes: 5.0 });
    }
}

#[test]
fn a_renewed_lease_outranks_an_older_released_record_of_the_same_identity() {
    // The #14811 shape after a re-claim: the old record was edited to
    // released, a newer live lease exists. Renewal (self or peer) wins.
    let rows = [row(1, 600, RELEASED), row(2, 3, LEASE)];
    let got = classify_curator_lease(&rows, now(), 15.0);
    assert_eq!(got, CuratorLeaseEvidence::Live { age_minutes: 3.0 });
}

#[test]
fn a_released_record_names_the_release() {
    let got = classify_curator_lease(&[row(1, 600, RELEASED)], now(), 15.0);
    assert_eq!(got, CuratorLeaseEvidence::Released { age_minutes: 600.0 });
}

#[test]
fn an_unrenewed_lease_is_expired() {
    let got = classify_curator_lease(&[row(1, 40, LEASE)], now(), 15.0);
    assert_eq!(got, CuratorLeaseEvidence::Expired { age_minutes: 40.0 });
}

#[test]
fn a_later_yield_kills_the_lease_it_names() {
    let rows = [row(1, 5, LEASE), row(2, 4, YIELD)];
    let got = classify_curator_lease(&rows, now(), 15.0);
    assert_eq!(got, CuratorLeaseEvidence::Yielded);
}

#[test]
fn other_roles_records_do_not_count() {
    let judge = "<!-- loom:pr-lease role=judge host=h sweep=s1 -->\nholds";
    let got = classify_curator_lease(&[row(1, 5, judge)], now(), 15.0);
    assert_eq!(got, CuratorLeaseEvidence::Absent);
}

#[test]
fn no_records_at_all_is_absent() {
    assert_eq!(classify_curator_lease(&[], now(), 15.0), CuratorLeaseEvidence::Absent);
}

// -----------------------------------------------------------------------
// decide_curating_claim
// -----------------------------------------------------------------------

const STALE: f64 = 45.0;
/// A `labeled` event a week ago — comfortably past every threshold.
fn stale_labeled_at() -> Option<DateTime<Utc>> {
    Some(at(7, 0, 0, 0))
}

#[test]
fn a_frozen_issue_is_never_written() {
    let lease = CuratorLeaseEvidence::Released { age_minutes: 600.0 };
    let got = decide_curating_claim(
        &labels(&["loom:curating", "loom:frozen", "loom:issue"]),
        stale_labeled_at(),
        lease,
        now(),
        STALE,
    );
    assert_eq!(got, A::Keep(K::FrozenItem));
}

#[test]
fn a_live_lease_keeps_the_claim_however_stale_the_label() {
    let lease = CuratorLeaseEvidence::Live { age_minutes: 2.0 };
    let got = decide_curating_claim(
        &labels(&["loom:curating", "loom:issue"]),
        stale_labeled_at(),
        lease,
        now(),
        STALE,
    );
    assert_eq!(got, A::Keep(K::LeaseLive { age_minutes: 2.0 }));
}

#[test]
fn a_fresh_claim_is_kept_even_behind_a_released_lease() {
    // The arbitration window (`stale_after_minutes`, the same 45 minutes
    // `claim-staleness.sh` uses) still protects the claimant.
    let lease = CuratorLeaseEvidence::Released { age_minutes: 600.0 };
    let got = decide_curating_claim(
        &labels(&["loom:curating", "loom:issue"]),
        Some(at(0, 11, 30, 0)), // 30 minutes old
        lease,
        now(),
        STALE,
    );
    assert_eq!(
        got,
        A::Keep(K::ClaimTooFresh {
            age_minutes: 30.0,
            stale_minutes: STALE,
        })
    );
}

#[test]
fn an_unknown_claim_age_is_kept() {
    let lease = CuratorLeaseEvidence::Released { age_minutes: 600.0 };
    let got =
        decide_curating_claim(&labels(&["loom:curating", "loom:issue"]), None, lease, now(), STALE);
    assert_eq!(got, A::Keep(K::ClaimAgeUnknown));
}

#[test]
fn no_lease_record_keeps_the_claim_however_stale_the_label() {
    // The attended Curator path takes no lease; `claim-staleness.sh` owns
    // that case. Absence is not evidence.
    let got = decide_curating_claim(
        &labels(&["loom:curating"]),
        stale_labeled_at(),
        CuratorLeaseEvidence::Absent,
        now(),
        STALE,
    );
    assert_eq!(got, A::Keep(K::NoLeaseRecord));
}

#[test]
fn the_incident_shape_releases_the_claim_and_records_recuration() {
    // #14811's klayout-tools shape: curating + triage + the requeue's
    // unearned `loom:issue`. Only the claim goes; the record flags the
    // approval's unknown provenance.
    let lease = CuratorLeaseEvidence::Released { age_minutes: 600.0 };
    let got = decide_curating_claim(
        &labels(&["loom:curating", "loom:triage", "loom:issue"]),
        stale_labeled_at(),
        lease,
        now(),
        STALE,
    );
    assert_eq!(
        got,
        A::RemoveClaim {
            recuration_record: true
        }
    );
}

#[test]
fn an_earned_loom_curated_beside_loom_issue_needs_no_record() {
    let lease = CuratorLeaseEvidence::Expired { age_minutes: 90.0 };
    let got = decide_curating_claim(
        &labels(&["loom:curating", "loom:curated", "loom:issue"]),
        stale_labeled_at(),
        lease,
        now(),
        STALE,
    );
    assert_eq!(
        got,
        A::RemoveClaim {
            recuration_record: false
        }
    );
}

#[test]
fn an_unadvanced_issue_restores_triage() {
    let lease = CuratorLeaseEvidence::Expired { age_minutes: 90.0 };
    let got =
        decide_curating_claim(&labels(&["loom:curating"]), stale_labeled_at(), lease, now(), STALE);
    assert_eq!(got, A::Restore);
}

#[test]
fn an_operator_hold_releases_the_claim_without_touching_the_hold() {
    let lease = CuratorLeaseEvidence::Yielded;
    let got = decide_curating_claim(
        &labels(&["loom:curating", "loom:operator"]),
        stale_labeled_at(),
        lease,
        now(),
        STALE,
    );
    assert_eq!(
        got,
        A::RemoveClaim {
            recuration_record: false
        }
    );
}

#[test]
fn a_claim_missing_from_the_labels_is_already_gone() {
    let lease = CuratorLeaseEvidence::Released { age_minutes: 600.0 };
    let got =
        decide_curating_claim(&labels(&["loom:issue"]), stale_labeled_at(), lease, now(), STALE);
    assert_eq!(got, A::Keep(K::ClaimAlreadyGone));
}

#[test]
fn the_recuration_record_body_carries_the_marker_first_line() {
    let body =
        recuration_record_body(&CuratorLeaseEvidence::Released { age_minutes: 600.0 }, now());
    assert!(body.starts_with("<!-- loom:recuration-needed claim=loom:curating -->"));
}

// -----------------------------------------------------------------------
// End to end against a fake `gh`
// ---------------------------------------------------------------------------

/// The fake `gh`'s issue-side answers, one field per arm.
struct FakeIssue {
    /// Labels of the discovery listing row AND the fresh `issue view` read.
    labels: &'static [&'static str],
    /// The trusted comments body (already a JSON array of lease-family and
    /// marker rows authored by the fleet App).
    comments: String,
    /// The `labeled` timeline answer for `loom:curating` (a quoted
    /// RFC-3339 timestamp), or `None` to fail the read.
    labeled_at: Option<&'static str>,
}

fn fleet_row(id: u64, minutes_ago: i64, body: &str) -> String {
    let updated = (Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339();
    format!(
        r#"{{"id":{id},"updated_at":"{updated}","body":{body},"user":{{"login":"loom-fleet-dispatch[bot]","type":"Bot"}},"author_association":"NONE"}}"#
    )
}

/// JSON-quote a comment body for a fixture row.
fn quoted(body: &str) -> String {
    serde_json::to_string(body).unwrap()
}

fn write_fake_gh(
    dir: &std::path::Path,
    issue: &FakeIssue,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-curating.sh");
    let listing_labels = issue
        .labels
        .iter()
        .map(|l| format!(r#"{{"name":"{l}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let timeline = issue
        .labeled_at
        .map(|t| format!("echo '{t}'"))
        .unwrap_or_else(|| "exit 1".to_string());
    let mut recorded_comments: Vec<serde_json::Value> =
        serde_json::from_str(&issue.comments).unwrap();
    let fresh_lease = fleet_row(15, 0, &quoted(LEASE));
    let fresh_lease: serde_json::Value = serde_json::from_str(&fresh_lease).unwrap();
    let record =
        fleet_row(14, 0, &quoted("<!-- loom:recuration-needed claim=loom:curating -->\nrecorded"));
    recorded_comments.push(serde_json::from_str(&record).unwrap());
    let recorded_comments_json = serde_json::to_string(&recorded_comments).unwrap();
    recorded_comments.push(fresh_lease);
    let renewed_comments = serde_json::to_string(&recorded_comments).unwrap();
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"api -X POST "*"issues/300/comments"*)
    if [ -f '{log}.reject-post' ]; then exit 1; fi
    if [ -f '{log}.post-renewal' ]; then touch '{log}.renewed'; fi
    if [ -f '{log}.lost-response' ]; then touch '{log}.recorded'; exit 1; fi
    if [ ! -f '{log}.unconfirmed-post' ]; then echo '123'; fi
    exit 0 ;;
  *"issues/300/comments"*)
    if [ -f '{log}.renewed' ]; then printf '%s' '{renewed_comments}';
    elif [ -f '{log}.recorded' ]; then printf '%s' '{recorded_comments}';
    else printf '%s' '{comments}'; fi
    exit 0 ;;
  *"issues/300/timeline"*)
    if [ -f '{log}.relabel-on-add' ] && [ -f '{log}.triage' ]; then
      printf '%s' '{fresh_label_at}'
    else {timeline}; fi
    exit 0 ;;
  *"issues?labels=loom:curating"*|*"labels=loom:curating"*)
    printf 'HTTP/2.0 200 OK\r\n\r\n'
    if [ -f '{log}.removed' ]; then echo '[]';
    else echo '[{{"number":300,"state":"open","labels":[{listing_labels}],"updated_at":"2026-11-20T00:00:00Z"}}]'; fi
    exit 0 ;;
esac
if [ "$1" = "api" ]; then
  case "$*" in
    *"issues/300"*) printf '"open"' ;;
    *) echo '[]' ;;
  esac
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  labels='{listing_labels}'
  if [ -f '{log}.triage' ]; then
    labels="$labels,"'{{"name":"loom:triage"}}'
    if [ -f '{log}.hold-on-add' ]; then labels="$labels,"'{{"name":"loom:blocked"}}'; fi
    if [ -f '{log}.approve-on-add' ]; then labels="$labels,"'{{"name":"loom:issue"}}'; fi
  fi
  printf '{{"labels":[%s]}}\n' "$labels"
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "edit" ]; then
  case "$*" in
    *"--remove-label loom:curating"*) touch '{log}.removed' ;;
  esac
  case "$*" in
    *"--add-label loom:triage"*)
      if [ -f '{log}.reject-add' ]; then exit 1; fi
      if [ ! -f '{log}.unconfirmed-add' ]; then touch '{log}.triage'; fi ;;
  esac
fi
exit 0
"#,
        log = log.display(),
        comments = issue.comments,
        recorded_comments = recorded_comments_json,
        renewed_comments = renewed_comments,
        timeline = timeline,
        fresh_label_at = quoted(&Utc::now().to_rfc3339()),
        listing_labels = listing_labels,
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

fn released_lease_comments(extra: &[String]) -> String {
    let mut rows = vec![fleet_row(11, 600, &quoted(RELEASED))];
    rows.extend(extra.iter().cloned());
    format!("[{}]", rows.join(","))
}

/// The pass against a fake `gh`. The process-global ETag cache stays cold
/// (a fresh tempdir root per call; the fake serves no ETag, so nothing is
/// cached anyway).
fn run_pass(gh: &std::path::Path, root: &std::path::Path) {
    reconcile_curating_claims(gh, root);
}

#[test]
#[serial]
fn the_incident_shape_releases_the_claim_posts_the_record_and_preserves_labels() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &[
            "loom:curating",
            "loom:triage",
            "loom:issue",
            "loom:very-important",
        ],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains("issue edit 300 --remove-label loom:curating"),
        "expected the claim release; got: {calls}"
    );
    assert!(
        !calls.contains("--add-label"),
        "no label may be added to an advanced item: {calls}"
    );
    assert!(
        calls.contains("api -X POST repos/{owner}/{repo}/issues/300/comments")
            && calls.contains("--jq .id"),
        "expected the re-curation record post: {calls}"
    );
}

#[test]
#[serial]
fn an_unconfirmed_recuration_record_keeps_the_claim_for_retry() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let mut fake = FakeIssue {
        labels: &["loom:curating", "loom:issue"],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    let release = "issue edit 300 --remove-label loom:curating";

    for failure in ["reject-post", "unconfirmed-post"] {
        let flag = log.with_extension(format!("log.{failure}"));
        std::fs::write(&flag, "").unwrap();
        run_pass(&gh, &root);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("-X POST").count(), 1, "{calls}");
        assert!(!calls.contains(release), "leave the claim discoverable: {calls}");
        std::fs::remove_file(&flag).unwrap();
        std::fs::write(&log, "").unwrap();
    }

    // A new owner can arrive between attempts. The next pass must read that
    // lease afresh and leave both the comment and the claim alone.
    let fresh_lease = fleet_row(12, 5, &quoted(LEASE));
    fake.comments = released_lease_comments(&[fresh_lease]);
    write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("-X POST") && !calls.contains(release), "{calls}");
    std::fs::write(&log, "").unwrap();

    // The intended stale claim remains discoverable and the issue is eligible
    // again. A confirmed record must precede the release.
    fake.comments = released_lease_comments(&[]);
    write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("-X POST").count(), 1, "{calls}");
    assert!(calls.find("-X POST").unwrap() < calls.find(release).unwrap(), "{calls}");
}

#[test]
#[serial]
fn a_live_lease_writes_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating", "loom:issue"],
        comments: format!("[{}]", fleet_row(11, 5, &quoted(LEASE))),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("issue edit"), "no write may happen: {calls}");
    assert!(!calls.contains("-X POST"), "no record may be posted: {calls}");
}

#[test]
#[serial]
fn an_unreadable_lease_read_writes_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating", "loom:issue"],
        comments: "[]".to_string(),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (_gh, log) = write_fake_gh(dir.path(), &fake);
    // Serve the listing, fail the comments read: rewrite the comments arm.
    let bin = dir.path().join("fake-gh-curating.sh");
    let script = std::fs::read_to_string(&bin)
        .unwrap()
        .replace("*\"issues/300/comments\"*)\n", "*\"issues/300/comments\"*)\n    exit 1\n");
    std::fs::write(&bin, script).unwrap();
    run_pass(&bin, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("issue edit"), "an unverifiable read must not write: {calls}");
}

#[test]
#[serial]
fn no_lease_record_writes_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating"],
        comments: "[]".to_string(),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        !calls.contains("issue edit"),
        "absence is not evidence; the attended no-lease path must not be touched: {calls}"
    );
}

#[test]
#[serial]
fn an_unadvanced_issue_restores_loom_triage() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating"],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    let add = "issue edit 300 --add-label loom:triage";
    let remove = "issue edit 300 --remove-label loom:curating";
    assert!(calls.find(add).unwrap() < calls.find(remove).unwrap(), "{calls}");
    assert!(log.with_extension("log.triage").exists());
    assert!(log.with_extension("log.removed").exists());
}

#[test]
#[serial]
fn a_renewal_during_the_recuration_post_keeps_the_claim() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating", "loom:issue"],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    std::fs::write(log.with_extension("log.post-renewal"), "").unwrap();
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("-X POST") && calls.contains("--jq .id"), "{calls}");
    assert!(log.with_extension("log.renewed").exists());
    assert!(!calls.contains("issue edit"), "keep the renewed claim: {calls}");
}

#[test]
#[serial]
fn a_lost_record_response_reuses_the_confirmed_marker_on_retry() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating", "loom:issue"],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    std::fs::write(log.with_extension("log.lost-response"), "").unwrap();
    run_pass(&gh, &root);
    assert!(log.with_extension("log.recorded").exists());
    assert!(!log.with_extension("log.removed").exists());
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("-X POST").count(), 1, "{calls}");
    assert!(log.with_extension("log.removed").exists());
}

#[test]
#[serial]
fn rejected_or_unconfirmed_triage_add_leaves_a_discoverable_retry() {
    for failure in ["reject-add", "unconfirmed-add"] {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let fake = FakeIssue {
            labels: &["loom:curating"],
            comments: released_lease_comments(&[]),
            labeled_at: Some("\"2026-11-13T23:42:26Z\""),
        };
        let (gh, log) = write_fake_gh(dir.path(), &fake);
        let flag = log.with_extension(format!("log.{failure}"));
        std::fs::write(&flag, "").unwrap();
        run_pass(&gh, &root);
        assert!(!log.with_extension("log.triage").exists());
        assert!(!log.with_extension("log.removed").exists());
        std::fs::remove_file(flag).unwrap();

        run_pass(&gh, &root);
        assert!(log.with_extension("log.triage").exists());
        assert!(log.with_extension("log.removed").exists());
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("--add-label loom:triage").count(), 2, "{calls}");
        assert_eq!(calls.matches("--remove-label loom:curating").count(), 1, "{calls}");
    }
}

#[test]
#[serial]
fn a_hold_or_approval_after_adding_triage_defers_the_release() {
    for state_change in ["hold-on-add", "approve-on-add"] {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let fake = FakeIssue {
            labels: &["loom:curating", "loom:very-important"],
            comments: released_lease_comments(&[]),
            labeled_at: Some("\"2026-11-13T23:42:26Z\""),
        };
        let (gh, log) = write_fake_gh(dir.path(), &fake);
        std::fs::write(log.with_extension(format!("log.{state_change}")), "").unwrap();
        run_pass(&gh, &root);
        assert!(log.with_extension("log.triage").exists());
        assert!(!log.with_extension("log.removed").exists());
        let before = std::fs::read_to_string(&log).unwrap();

        run_pass(&gh, &root);
        let after = std::fs::read_to_string(&log).unwrap();
        let retry = after.strip_prefix(&before).unwrap();
        assert!(!retry.contains("--add-label"), "{retry}");
        assert!(retry.contains("issue edit 300 --remove-label loom:curating"), "{retry}");
        assert!(!retry.contains("--remove-label loom:very-important"), "{retry}");
        assert_eq!(retry.contains("-X POST"), state_change == "approve-on-add", "{retry}");
    }
}

#[test]
#[serial]
fn a_new_claim_after_adding_triage_is_not_removed() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating"],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    std::fs::write(log.with_extension("log.relabel-on-add"), "").unwrap();
    for _ in 0..2 {
        run_pass(&gh, &root);
    }
    assert!(log.with_extension("log.triage").exists());
    assert!(!log.with_extension("log.removed").exists());
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("--add-label loom:triage").count(), 1, "{calls}");
}

#[test]
#[serial]
fn an_existing_record_is_not_reposted_on_the_retry_after_a_partial_repair() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    // A partial repair left the record posted but the label in place (the
    // edit failed last pass): the record row is in the comments, the claim
    // is still on the item. This pass must edit, not post again.
    let marker =
        fleet_row(12, 30, &quoted("<!-- loom:recuration-needed claim=loom:curating -->\nrecorded"));
    let fake = FakeIssue {
        labels: &["loom:curating", "loom:issue"],
        comments: released_lease_comments(&[marker]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains("issue edit 300 --remove-label loom:curating"),
        "the edit must be retried: {calls}"
    );
    assert!(!calls.contains("-X POST"), "the record must not be posted twice: {calls}");
}

#[test]
#[serial]
fn a_completed_repair_never_reaches_a_write() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        // The listing still shows the claim (it is the stale ETag row), but
        // the fresh read sees the repair completed: nothing to write.
        labels: &["loom:issue"],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-13T23:42:26Z\""),
    };
    let (_gh, log) = write_fake_gh(dir.path(), &fake);
    // Force the decision past the listing gate by listing the claim but
    // reading it back healed: give the listing its own rendered row.
    let bin = dir.path().join("fake-gh-curating.sh");
    let healed = r#"echo '[{"number":300,"state":"open","labels":[{"name":"loom:issue"}],"updated_at":"2026-11-20T00:00:00Z"}]'"#;
    let claimed = r#"echo '[{"number":300,"state":"open","labels":[{"name":"loom:curating"},{"name":"loom:issue"}],"updated_at":"2026-11-20T00:00:00Z"}]'"#;
    let script = std::fs::read_to_string(&bin)
        .unwrap()
        .replacen(healed, claimed, 1);
    std::fs::write(&bin, script).unwrap();
    run_pass(&bin, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("issue edit"), "a healed item must not be written: {calls}");
}

#[test]
#[serial]
fn a_fresh_claim_writes_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let fake = FakeIssue {
        labels: &["loom:curating"],
        comments: released_lease_comments(&[]),
        labeled_at: Some("\"2026-11-21T11:40:00Z\""), // 20 minutes old
    };
    let (gh, log) = write_fake_gh(dir.path(), &fake);
    run_pass(&gh, &root);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        !calls.contains("issue edit"),
        "a claim inside the arbitration window must not be touched: {calls}"
    );
}

#[test]
#[serial]
fn the_staleness_floor_resolves_through_claim_liveness() {
    // The registry (or `LOOM_STALE_CURATING_MINUTES`) must supply the
    // threshold — this pass adds no second knob.
    let minutes = crate::claim_liveness::stale_after_minutes("loom:curating");
    assert!(minutes.is_some_and(|m| m > 0.0), "no registry threshold");
}
