#![allow(clippy::unwrap_used)]
use super::*;

const MARKER: &str = "<!-- loom:estimate lines=7 files=1 doctor_risk=0.3 -->";

fn row(number: u32, labels: &[&str], body: &str, updated: DateTime<Utc>) -> RestIssue {
    RestIssue {
        number,
        title: None,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: None,
        updated_at: Some(updated.to_rfc3339()),
        closed_at: None,
        state: "open".into(),
        body: Some(body.into()),
        author: Some("owner".into()),
        author_association: Some("OWNER".into()),
        is_pull_request: false,
        comments: 0,
        open_sub_issues: None,
        labels_at: None,
        draft: false,
    }
}

fn journal_count(root: &Path) -> usize {
    std::fs::read_dir(root.join(".loom/logs/issue-estimates"))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn curated_issue_emits_once_and_reobservation_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let policy = TrustPolicy::for_root(dir.path());
    let now = Utc::now();
    let rows = [row(5, &["loom:curated", "points:3"], MARKER, now)];
    assert_eq!(observe(dir.path(), "o/r", &rows, &policy, now), 1);
    assert_eq!(observe(dir.path(), "o/r", &rows, &policy, now), 0);
    assert_eq!(journal_count(dir.path()), 1);
}

#[test]
fn skips_uncurated_malformed_stale_and_markerless() {
    let dir = tempfile::tempdir().unwrap();
    let policy = TrustPolicy::for_root(dir.path());
    let now = Utc::now();
    let old = now - Duration::days(30);
    let rows = [
        row(1, &["loom:issue"], MARKER, now),
        row(2, &["loom:curated"], "<!-- loom:estimate lines=x -->", now),
        row(3, &["loom:curated"], MARKER, old),
        row(4, &["loom:curated"], "no marker", now),
    ];
    assert_eq!(observe(dir.path(), "o/r", &rows, &policy, now), 0);
    assert_eq!(journal_count(dir.path()), 0);
}
