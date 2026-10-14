//! The Judge no-verdict backstop's pure rules (#11855).
#![allow(clippy::unwrap_used)]

use super::*;
use chrono::TimeZone;

const HEAD: &str = "1111111111111111111111111111111111111111";
const OTHER: &str = "2222222222222222222222222222222222222222";

fn t(h: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 0, 0, 0).unwrap() + Duration::minutes(h * 60)
}

fn rec(head: &str, labels: &str) -> NoVerdict {
    NoVerdict {
        head: head.into(),
        labels: labels.into(),
    }
}

fn marker(head: &str, labels: &[&str]) -> String {
    format!(
        "{RELEASED_JUDGE_PREFIX}host=host-1 sweep=s-1{} -->\nPR #1's claim…",
        released_fields(head, labels)
    )
}

#[test]
fn a_release_marker_round_trips() {
    let body = marker(HEAD, &["loom:review-requested", "loom:reviewing"]);
    assert_eq!(parse(&body), Some(rec(HEAD, "review-requested")));
}

#[test]
fn only_no_verdict_judge_release_records_parse() {
    // A plain release (the pass reached a verdict, or an older daemon).
    assert_eq!(parse("<!-- loom:pr-lease-released role=judge host=h sweep=s -->"), None);
    // A Doctor's release never counts.
    let doctor = marker(HEAD, &["loom:review-requested"]).replace("role=judge", "role=doctor");
    assert_eq!(parse(&doctor), None);
    // A live lease is not a release.
    let live = marker(HEAD, &["loom:review-requested"]).replace("-released", "");
    assert_eq!(parse(&live), None);
    // Only the first line is read.
    assert_eq!(parse(&format!("prose\n{}", marker(HEAD, &[]))), None);
    // A non-hex head is not a head.
    assert_eq!(parse(&marker("not-a-sha", &[])), None);
}

#[test]
fn the_digest_ignores_claims_order_and_non_loom_labels() {
    let a = label_digest(&["loom:review-requested", "loom:reviewing", "bug"]);
    let b = label_digest(&["loom:review-requested", "loom:treating"]);
    assert_eq!(a, "review-requested");
    assert_eq!(a, b);
    assert_eq!(
        label_digest(&["loom:review-requested", "loom:important"]),
        "important,review-requested"
    );
    assert_eq!(label_digest::<&str>(&[]), "-");
}

#[test]
fn a_release_is_no_verdict_only_while_review_is_still_requested_without_a_verdict() {
    assert!(ended_without_verdict(true, &["loom:review-requested", "loom:reviewing"]));
    assert!(!ended_without_verdict(true, &["loom:pr"]));
    assert!(!ended_without_verdict(
        true,
        &["loom:review-requested", "loom:changes-requested"]
    ));
    assert!(!ended_without_verdict(false, &["loom:review-requested"]));
}

#[test]
fn the_hold_escalates_one_hour_four_hours_then_the_window() {
    let r = |h: i64| (t(h), rec(HEAD, "review-requested"));
    let held = |records: &[(DateTime<Utc>, NoVerdict)], now: i64| {
        hold_until(records, HEAD, "review-requested", t(now))
    };
    assert_eq!(held(&[], 0), None);
    // One record: an hour.
    assert_eq!(held(&[r(0)], 0), Some(t(1)));
    assert_eq!(held(&[r(0)], 1), None);
    // Two: four hours after the newest.
    assert_eq!(held(&[r(0), r(1)], 2), Some(t(5)));
    assert_eq!(held(&[r(0), r(1)], 5), None);
    // Three: until the oldest of the last three leaves the window.
    assert_eq!(held(&[r(5), r(0), r(1)], 6), Some(t(24)));
    assert_eq!(held(&[r(0), r(1), r(5)], 24), None);
}

#[test]
fn an_unchanged_pr_gets_at_most_three_runs_a_day() {
    // Simulate a Judge that is served whenever the PR is not held, and never
    // reaches a verdict, at a one-minute tick for 24 hours.
    let mut records = Vec::new();
    let mut runs = 0;
    for minute in 0..(24 * 60) {
        let now = t(0) + Duration::minutes(minute);
        if hold_until(&records, HEAD, "review-requested", now).is_none() {
            runs += 1;
            records.push((now, rec(HEAD, "review-requested")));
        }
    }
    assert_eq!(runs, MAX_PER_WINDOW);
}

#[test]
fn a_new_head_or_a_label_change_releases_the_hold() {
    let records = vec![
        (t(0), rec(OTHER, "review-requested")),
        (t(0), rec(OTHER, "review-requested")),
        (t(0), rec(OTHER, "review-requested")),
    ];
    assert!(hold_until(&records, OTHER, "review-requested", t(1)).is_some());
    assert_eq!(hold_until(&records, HEAD, "review-requested", t(1)), None);
    assert_eq!(hold_until(&records, OTHER, "important,review-requested", t(1)), None);
    // Case never matters for a head.
    assert!(hold_until(&records, &OTHER.to_ascii_uppercase(), "review-requested", t(1)).is_some());
}

#[test]
fn comments_are_read_by_their_updated_at() {
    let comments = vec![
        serde_json::json!({
            "body": marker(HEAD, &["loom:review-requested"]),
            "updated_at": "2026-10-13T00:00:00Z",
        }),
        serde_json::json!({"body": "an ordinary comment", "updated_at": "2026-10-13T00:00:00Z"}),
        serde_json::json!({"body": marker(HEAD, &["loom:review-requested"])}),
    ];
    let labels = ["loom:review-requested", "loom:reviewing"];
    assert_eq!(hold_from_comments(&comments, HEAD, &labels, t(0)), Some(t(1)));
    assert_eq!(hold_from_comments(&comments, OTHER, &labels, t(0)), None);
}

#[test]
fn pending_checks_exempt_only_while_young() {
    let at = |h: i64| t(h).to_rfc3339();
    assert!(pending_exempts(&[at(0)], t(5)));
    // A run stuck queued past the cutoff no longer exempts the pass.
    assert!(!pending_exempts(&[at(0)], t(PENDING_EXEMPT_HOURS)));
    assert!(pending_exempts(&[at(0), at(4)], t(7)));
    // Missing or garbled stamps never exempt.
    assert!(!pending_exempts(&[String::new(), "garbage".into()], t(0)));
    assert!(!pending_exempts(&[], t(0)));
}

#[test]
fn the_switch_reads_the_usual_falsy_values() {
    assert!(enabled_from(None));
    assert!(enabled_from(Some("1")));
    for off in ["0", "false", "NO", " off "] {
        assert!(!enabled_from(Some(off)), "{off}");
    }
}
