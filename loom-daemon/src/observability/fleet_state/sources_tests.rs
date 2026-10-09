//! The `fleet.state` reads (Issue #10196): the paged review listings and the
//! ready queue's completeness.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::Utc;

use super::super::{build_view, decide, Emitted, FleetInput, ReadyItem};
use crate::telemetry::kinds::fleet_state::PlannerStamps;
use crate::types::{DispatchPlanContext, ReadyQueueRow, WorkFinderTickSummary};

/// A `gh` stub serving the open-item listings. `loom:review-requested`
/// answers `rr1.json` on page 1 and `rr2.json` on `&page=2`; the other review
/// labels answer an empty page. A `&page=N` request fails when `fail<N>`
/// exists. Every call's argv is logged to `calls.log`.
fn review_stub(dir: &Path) -> PathBuf {
    let path = dir.join("fake-gh-review.sh");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
d={dir}
echo "$*" >> "$d/calls.log"
n=1
case "$*" in
  *'&page='*)
    n=$(echo "$*" | sed 's/.*&page=\([0-9]*\).*/\1/')
    if [ -f "$d/fail$n" ]; then echo 'gh: Server Error (HTTP 502)' 1>&2; exit 1; fi ;;
esac
case "$*" in
  *'labels=loom:review-requested&'*'&page=2'*) f=rr2.json ;;
  *'labels=loom:review-requested&'*) f=rr1.json ;;
  *) f=empty.json ;;
esac
printf 'HTTP/2.0 200 OK\r\n'
[ "$(grep -o '"number"' "$d/$f" | wc -l)" -ge 100 ] && [ ! -f "$d/last$n" ] && printf 'Link: <https://api.github.com/next>; rel="next"\r\n'
printf '\r\n'
cat "$d/$f"
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("empty.json"), "[]\n").unwrap();
    path
}

/// Open PRs `numbers` under `loom:review-requested`, each closing issue
/// `number + 10_000`.
fn pr_page(numbers: std::ops::Range<u32>) -> String {
    let rows: Vec<String> = numbers
        .map(|n| {
            format!(
                r#"{{"number": {n}, "state": "open", "pull_request": {{}},
                    "labels": [{{"name": "loom:review-requested"}}],
                    "body": "Closes #{}"}}"#,
                n + 10_000
            )
        })
        .collect();
    format!("[{}]\n", rows.join(","))
}

/// More than one page of PRs under one review label: every one of them is a
/// listed PR, so none can vanish from the rows or the census.
#[test]
fn a_review_label_with_more_than_a_page_of_prs_lists_every_pr() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/fleet-review-{}", std::process::id());
    std::fs::write(dir.path().join("rr1.json"), pr_page(1..101)).unwrap();
    std::fs::write(dir.path().join("rr2.json"), pr_page(101..131)).unwrap();
    let gh = review_stub(dir.path());

    let listing = super::review_listing_with(&gh, dir.path(), Some(&repo), "acme/app").unwrap();
    assert_eq!(listing.repo, "acme/app");
    let numbers: Vec<u32> = listing.prs.iter().map(|pr| pr.number).collect();
    assert_eq!(numbers, (1..131).collect::<Vec<_>>());
    assert!(listing
        .prs
        .iter()
        .all(|pr| pr.issue == Some(pr.number + 10_000)));
    let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    assert!(calls.contains("&page=2"), "{calls}");

    // Fed to the view, the census counts all of them.
    let input = super::super::FleetInput {
        managed: ["acme/app".to_string()].into_iter().collect(),
        listings: vec![listing],
        ..Default::default()
    };
    let view = super::super::build_view(&input, None, chrono::Utc::now());
    assert_eq!(view.repos["acme/app"].census.as_ref().unwrap().open, 130);
    assert_eq!(view.repos["acme/app"].rows.len(), 130);
}

/// A walk that cannot finish (page 2 fails) is an error, so the repo is
/// unobserved this pass rather than listed with the first page only.
#[test]
fn an_unfinished_review_walk_is_an_error_not_the_first_page() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/fleet-review-fail-{}", std::process::id());
    std::fs::write(dir.path().join("rr1.json"), pr_page(1..101)).unwrap();
    std::fs::write(dir.path().join("rr2.json"), pr_page(101..131)).unwrap();
    std::fs::write(dir.path().join("fail2"), "").unwrap();
    let gh = review_stub(dir.path());

    let err = super::review_listing_with(&gh, dir.path(), Some(&repo), "acme/app").unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 502"), "{err:#}");
}

fn row(repo: &str, issue: u32) -> ReadyQueueRow {
    serde_json::from_value(serde_json::json!({
        "rank": issue, "repo": repo, "issue": issue, "workspace_priority": 100,
        "urgent": false, "disposition": "queued"
    }))
    .unwrap()
}

/// (c) A repo is `ready_complete` exactly when the tick walked its ready
/// listing to the last page (#11139), whatever its row count. A failed repo is
/// not `listed`; a partial one is listed but not complete, so the wire marks
/// it `ready_replace` and an issue absent from the tick is replaced away,
/// never sent in `removed[]`. A complete repo is diffed: an issue that left
/// is sent in `removed[]`.
#[test]
fn only_a_whole_listing_is_ready_complete() {
    let managed: BTreeSet<String> = [
        "acme/full",
        "acme/short",
        "acme/failed",
        "acme/idle",
        "acme/partial",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let per_page = u32::try_from(crate::forge_listing::PER_PAGE).unwrap();
    let mut summary = WorkFinderTickSummary {
        plan: Some(DispatchPlanContext::default()),
        listing_failed: vec!["/src/failed".to_string()],
        listing_incomplete: vec!["/src/partial".to_string()],
        ..WorkFinderTickSummary::default()
    };
    summary.queue = (1..=per_page).map(|n| row("/src/full", n)).collect();
    summary.queue.extend((1..=40).map(|n| row("/src/short", n)));
    summary
        .queue
        .extend((1..=per_page).map(|n| row("/src/partial", n)));
    let slug = |root: &str| root.strip_prefix("/src/").map(|r| format!("acme/{r}"));

    let ready = super::ready_from_tick(&summary, slug, &managed);
    let names = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>();
    assert_eq!(names(&ready.listed), ["acme/full", "acme/idle", "acme/partial", "acme/short"]);
    assert_eq!(names(&ready.complete), ["acme/full", "acme/idle", "acme/short"]);

    let input = |ready| FleetInput {
        managed: managed.clone(),
        ready: Some(ready),
        ..Default::default()
    };
    let now = Utc::now();
    let mut first = input(ready.clone());
    for repo in ["acme/short", "acme/partial"] {
        let template = ready.items.iter().find(|i| i.repo == repo).unwrap().clone();
        first.ready.as_mut().unwrap().items.push(ReadyItem {
            issue: 999,
            ..template
        });
    }
    let view = build_view(&first, None, now);
    let anchor = decide(&view, &PlannerStamps::default(), None, now).unwrap();
    for repo in &anchor.repos {
        let whole = repo.repo != "acme/partial";
        assert_eq!(repo.ready_complete, whole, "{}", repo.repo);
        assert_eq!(repo.ready_replace, !whole, "{}", repo.repo);
    }
    assert!(anchor.repos.iter().any(|r| r.repo == "acme/partial"));

    // #999 left both queues: the whole repo says so, the partial one cannot.
    let later = now + chrono::Duration::minutes(5);
    let second = build_view(&input(ready), Some(&view), later);
    let last = Emitted {
        view,
        stamps: PlannerStamps::default(),
        as_of: now,
        anchor_as_of: now,
    };
    let delta = decide(&second, &PlannerStamps::default(), Some(&last), later).unwrap();
    let short = delta.repos.iter().find(|r| r.repo == "acme/short").unwrap();
    assert!(short.ready_complete && !short.ready_replace);
    assert_eq!(short.removed, [999]);
    let partial = delta
        .repos
        .iter()
        .find(|r| r.repo == "acme/partial")
        .unwrap();
    assert!(!partial.ready_complete && partial.ready_replace);
    assert!(partial.removed.is_empty());
    assert_eq!(partial.rows.len(), crate::forge_listing::PER_PAGE);
    assert!(!second.repos["acme/partial"].rows.contains_key(&999));
}

#[test]
fn account_counts_separate_exhausted_from_other_unavailable() {
    use super::parse_account_counts;
    let ranking =
        "a|available|0.1\nb|exhausted|0.99\nc|blocked|0.0\nd|rate_limited|0.5\ne|mystery|0.0\n";
    assert_eq!(parse_account_counts(ranking), Some((1, 1)));
    // Blocked, rate-limited and unknown accounts are not exhausted.
    assert_eq!(
        parse_account_counts("c|blocked|0.0\nd|rate_limited|0.5\ne|mystery|0.0\n"),
        Some((0, 0))
    );
    assert_eq!(parse_account_counts("b|exhausted|0.99\n"), Some((0, 1)));
    assert_eq!(parse_account_counts("# only a comment\n"), None);
}

#[test]
fn live_workers_counts_every_nonterminal_sweep_regardless_of_kind_or_slug() {
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use crate::types::{SweepKind, SweepState};

    // Plain temp dirs: not git checkouts, so no repo slug can resolve.
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let reg_a = SweepRegistry::shared(SweepRegistryConfig::new(a.path().to_path_buf()));
    let reg_b = SweepRegistry::shared(SweepRegistryConfig::new(b.path().to_path_buf()));
    {
        let mut r = reg_a.lock().unwrap();
        r.seed_entry_for_test(SweepKind::Issue(1), SweepState::Running);
        r.seed_entry_for_test(
            SweepKind::Issue(2),
            SweepState::Exited {
                code: Some(0),
                at: Utc::now(),
            },
        );
    }
    reg_b
        .lock()
        .unwrap()
        .seed_entry_for_test(SweepKind::PrSet(vec![10, 20]), SweepState::Running);

    assert_eq!(super::live_sweep_count(&[reg_a, reg_b]), 2);
    assert_eq!(super::live_sweep_count(&[]), 0);
}
