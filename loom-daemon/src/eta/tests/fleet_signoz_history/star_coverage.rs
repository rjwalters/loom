//! #10746: star coverage keeps advancing under `historyPrimary`, each
//! listing is frozen from its own rows, and a gap-filled PR closed unmerged
//! resolves closed. A child of the #10520 history tests, sharing their
//! fixtures.

use super::*;

// -- star coverage of a raw cache SigNoz stops (#10520, judge round 3) ---------

/// A pre-#10520 raw cache for [`A`]: PR 101 links issue 7 (known at
/// `linked`) and gets a head commit at `linked + 12h`, the pulls listing's
/// newest row; issue 7 is labelled (no star) at `linked` and again at
/// `labeled`, the newest row. No cursor stamp.
fn plant_raw_cache(root: &Path, linked: DateTime<Utc>, labeled: DateTime<Utc>) {
    use crate::eta::fleet_events::{self, EventKind, EventLog, ItemKind, RawEvent};
    let rows = [
        RawEvent::new(
            A,
            101,
            ItemKind::Pr,
            EventKind::ClosingRef,
            Some("closes".into()),
            linked,
            "forge",
            1,
            linked,
        )
        .with_target(Some(7)),
        RawEvent::new(
            A,
            101,
            ItemKind::Pr,
            EventKind::HeadCommit,
            None,
            linked + Duration::hours(12),
            "forge",
            4,
            linked + Duration::hours(12),
        ),
        RawEvent::new(
            A,
            7,
            ItemKind::Issue,
            EventKind::LabelAdded,
            Some("loom:issue".into()),
            linked,
            "forge",
            2,
            linked,
        ),
        RawEvent::new(
            A,
            7,
            ItemKind::Issue,
            EventKind::LabelAdded,
            Some("loom:ready".into()),
            labeled,
            "forge",
            3,
            labeled,
        ),
    ];
    EventLog::open(&fleet_events::events_path(root, A))
        .unwrap()
        .append(&rows)
        .unwrap();
}

/// The judge's star finding at the production boundary: SigNoz covers the
/// repo, so its raw cache is not read and stops advancing. Before the fix the
/// cache still "covered" every later cutoff, so a link or star added after it
/// stopped read as known-unstarred. Now the cycle freezes each listing's
/// coverage at its own newest row (#10746), and a cutoff after it reads
/// unknown (`None`);
/// before it the cache still answers. A completed refresh (SigNoz history
/// off) stamps the listings at the cycle's `now` instead.
#[test]
fn a_covered_repo_freezes_raw_cache_coverage_so_stars_read_unknown_after_it() {
    use crate::eta::fleet_events::{self, EventsCursor};
    use crate::eta::star::{listing_keys, StarInputs};
    let prs = [approved(101, now() - Duration::hours(10))];
    let (linked, labeled) = (now() - Duration::days(3), now() - Duration::days(2));
    let repos = [A.to_string()];

    // Covered: no raw-event read, coverage frozen at the newest cached row.
    let dir = tempfile::tempdir().unwrap();
    plant_raw_cache(dir.path(), linked, labeled);
    let mut forge = Counting::with(&prs);
    let mut signoz = signoz_rows(&prs, true);
    let (r, seen) = production_cycle(dir.path(), &mut forge, Some(&mut signoz), 5);
    assert_eq!((r.history, seen.len()), (Some(HistorySource::Signoz), 0));
    let star = StarInputs::load(dir.path(), &repos);
    let star = &star.repos[A];
    // Each listing is frozen at its own newest row (#10746): the pulls
    // listing's newest row (the head commit) is older than the issue
    // listing's, so coverage ends there, not at the cache's newest row.
    let pulled = linked + Duration::hours(12);
    assert_eq!(star.synced_through, Some(pulled), "frozen at the pulls listing's own row");
    let after = pulled + Duration::hours(1);
    assert_eq!(star.state_at(101, 0, None, after), None, "after the cache stopped: unknown");
    let between = linked + Duration::hours(1);
    assert!(star.state_at(101, 0, None, between).is_some(), "before it, the cache answers");

    // A frozen stamp stays put on later covered cycles.
    let mut signoz = signoz_rows(&prs, true);
    production_cycle(dir.path(), &mut forge, Some(&mut signoz), 5);
    let cursor = EventsCursor::read(&fleet_events::cursor_path(dir.path(), A), A);
    assert_eq!(cursor.synced_through(&listing_keys()), Some(pulled));

    // History off, both listings refreshing to completion: stamped at `now`.
    let dir = tempfile::tempdir().unwrap();
    plant_raw_cache(dir.path(), linked, labeled);
    let cursor_file = fleet_events::cursor_path(dir.path(), A);
    let mut cursor = EventsCursor::read(&cursor_file, A);
    for key in listing_keys() {
        cursor.endpoints.entry(key).or_default().backfill_complete = true;
    }
    cursor.write(&cursor_file).unwrap();
    let mut forge = Counting::with(&prs);
    let (_, seen) = production_cycle(dir.path(), &mut forge, None, 5);
    assert_eq!(seen.len(), 2, "both listings refreshed");
    let star = StarInputs::load(dir.path(), &repos);
    assert_eq!(star.repos[A].synced_through, Some(now()));
    assert!(star.repos[A].state_at(101, 0, None, now()).is_some());
}

fn forge_key(endpoint: crate::eta::fleet_events_forge::ForgeEndpoint) -> String {
    format!("{}:{}", crate::eta::fleet_events::SOURCE_FORGE, endpoint.name())
}

fn stamp_of(
    root: &Path,
    endpoint: crate::eta::fleet_events_forge::ForgeEndpoint,
) -> Option<DateTime<Utc>> {
    use crate::eta::fleet_events::{cursor_path, EventsCursor};
    EventsCursor::read(&cursor_path(root, A), A)
        .endpoints
        .get(&forge_key(endpoint))
        .and_then(|e| e.synced_through)
}

fn stamp(root: &Path, endpoint: crate::eta::fleet_events_forge::ForgeEndpoint, at: DateTime<Utc>) {
    use crate::eta::fleet_events::{cursor_path, mark_synced_through};
    mark_synced_through(&cursor_path(root, A), A, &forge_key(endpoint), at).unwrap();
}

/// One raw forge row of the given listing, appended to `A`'s cache.
fn plant(root: &Path, rows: &[crate::eta::fleet_events::RawEvent]) {
    use crate::eta::fleet_events::{events_path, EventLog};
    EventLog::open(&events_path(root, A))
        .unwrap()
        .append(rows)
        .unwrap();
}

fn forge_row(
    item: u32,
    item_kind: crate::eta::fleet_events::ItemKind,
    kind: crate::eta::fleet_events::EventKind,
    label: Option<&str>,
    at: DateTime<Utc>,
    seq: u64,
) -> crate::eta::fleet_events::RawEvent {
    crate::eta::fleet_events::RawEvent::new(
        A,
        item,
        item_kind,
        kind,
        label.map(String::from),
        at,
        "forge",
        seq,
        at,
    )
}

/// Mixed listings: pulls and issue events have different newest rows, and a
/// newer unrelated row (a review, a merge shared by both listings) exists.
/// Each unstamped listing gets only its own newest row; a stamped one keeps
/// its stamp; a listing with no qualifying row stays unstamped (uncovered).
#[test]
fn freeze_stamps_each_listing_from_its_own_newest_row() {
    use crate::eta::fleet_events::{EventKind as K, ItemKind as I};
    use crate::eta::fleet_events_forge::ForgeEndpoint::{IssuesEvents, Pulls};
    let (t_pull, t_issue, t_new) = (t(100), t(200), t(900));
    let dir = tempfile::tempdir().unwrap();
    plant(
        dir.path(),
        &[
            forge_row(1, I::Pr, K::ClosingRef, None, t_pull, 1),
            forge_row(7, I::Issue, K::LabelAdded, Some("loom:issue"), t_issue, 2),
            forge_row(1, I::Pr, K::Review, Some("APPROVED"), t_new, 3),
            forge_row(1, I::Pr, K::Merged, None, t_new, 4),
        ],
    );
    crate::eta::fleet_signoz_history::freeze_raw_cache(dir.path(), A);
    assert_eq!(stamp_of(dir.path(), Pulls), Some(t_pull));
    assert_eq!(stamp_of(dir.path(), IssuesEvents), Some(t_issue));

    // An existing stamp is preserved, even with newer own rows.
    let dir = tempfile::tempdir().unwrap();
    plant(
        dir.path(),
        &[
            forge_row(1, I::Pr, K::ClosingRef, None, t_pull, 1),
            forge_row(7, I::Issue, K::LabelAdded, Some("loom:issue"), t_issue, 2),
        ],
    );
    stamp(dir.path(), Pulls, t(50));
    crate::eta::fleet_signoz_history::freeze_raw_cache(dir.path(), A);
    assert_eq!(stamp_of(dir.path(), Pulls), Some(t(50)), "existing stamp unchanged");
    assert_eq!(stamp_of(dir.path(), IssuesEvents), Some(t_issue));

    // No qualifying row for a listing: it stays uncovered, however new the
    // rest of the cache is.
    let dir = tempfile::tempdir().unwrap();
    plant(
        dir.path(),
        &[forge_row(
            7,
            I::Issue,
            K::LabelAdded,
            Some("loom:issue"),
            t_issue,
            2,
        )],
    );
    plant(dir.path(), &[forge_row(1, I::Pr, K::Merged, None, t_new, 3)]);
    crate::eta::fleet_signoz_history::freeze_raw_cache(dir.path(), A);
    assert_eq!(stamp_of(dir.path(), Pulls), None, "unrelated newer rows do not stamp it");
    assert_eq!(stamp_of(dir.path(), IssuesEvents), Some(t_issue));
}

/// Gap-filled PR resolution from the forge timeline's own close / reopen
/// evidence (#10746), through the REST parser.
#[test]
fn a_gap_filled_closed_unmerged_pr_resolves_closed() {
    let listed = ListedPr {
        created_at: t(0),
        state: PrState::Open, // SigNoz saw no close event
        merged_at: None,
        labels: Vec::new(),
    };
    let fill = |body: Value, cutoff: DateTime<Utc>| {
        let events = parse_timeline(body.to_string().as_bytes()).unwrap();
        gap_fill_history(9, &listed, events, cutoff)
    };
    let closed = json!({"event": "closed", "created_at": t(100).to_rfc3339()});
    let reopened = json!({"event": "reopened", "created_at": t(200).to_rfc3339()});
    let closed_again = json!({"event": "closed", "created_at": t(300).to_rfc3339()});
    let merged = json!({"event": "merged", "created_at": t(400).to_rfc3339()});

    let h = fill(json!([closed]), t(1000));
    assert_eq!((h.state, h.merged_at), (PrState::Closed, None));
    // The evidence is resolved into the state, not published as an event.
    assert!(h.events.is_empty());
    // Reopened after the close: open again.
    assert_eq!(fill(json!([closed, reopened]), t(1000)).state, PrState::Open);
    // Closed again after the reopen: closed.
    assert_eq!(fill(json!([closed, reopened, closed_again]), t(1000)).state, PrState::Closed);
    // Cutoff ordering: a close after the cutoff does not apply...
    assert_eq!(fill(json!([closed, reopened, closed_again]), t(250)).state, PrState::Open);
    assert_eq!(fill(json!([closed]), t(50)).state, PrState::Open);
    // ...and a close before it applies even with a later reopen after it.
    assert_eq!(fill(json!([closed, reopened]), t(150)).state, PrState::Closed);
    // A merge outranks every close (a merge emits its own `closed`).
    let h = fill(json!([closed, merged]), t(1000));
    assert_eq!((h.state, h.merged_at), (PrState::Merged, Some(t(400))));
    // No close or reopen evidence: the SigNoz state stands, never inferred.
    assert_eq!(fill(json!([]), t(1000)).state, PrState::Open);
}

/// A SigNoz timeline of issue 7's star changes (and the anchor issue 1), as
/// knowable at `cutoff`, planned for a window from `since`.
fn star_plan(
    changes: &[(&str, bool, DateTime<Utc>)],
    anchored: bool,
    cutoff: DateTime<Utc>,
) -> crate::eta::fleet_signoz_history::Plan {
    let mut lines = Vec::new();
    if anchored {
        let old = cutoff - Duration::days(25);
        lines.push(webhook("a-o", "issue", 1, "opened", None, old, false));
        lines.push(webhook("a-l", "issue", 1, "labeled", Some("loom:issue"), old, false));
    }
    // PR rows alone, so the label and lifecycle families are covered either way.
    let old_pr = cutoff - Duration::days(25);
    lines.push(webhook("p-o", "pr", 90, "opened", None, old_pr, false));
    for (i, (label, added, at)) in changes.iter().enumerate() {
        let action = if *added { "labeled" } else { "unlabeled" };
        lines.push(webhook(&format!("s{i}"), "issue", 7, action, Some(label), *at, false));
    }
    let mut rows = FileRows::parse(&lines.join("\n")).unwrap();
    let (rows, _) = walk(A, &mut rows, cutoff - Duration::days(40), cutoff, limits()).unwrap();
    let timeline = Timeline::build(&rows, cutoff);
    crate::eta::fleet_signoz_history::plan(&timeline, cutoff, cutoff - Duration::hours(1))
}

/// The cache of [`A`]: PR 101 links issue 7 from `s0`, PR 102 from `s0 + 90m`;
/// both listings stamped at `s0`.
fn plant_star_cache(root: &Path, s0: DateTime<Utc>) {
    use crate::eta::fleet_events::{EventKind as K, ItemKind as I};
    use crate::eta::fleet_events_forge::ForgeEndpoint::{IssuesEvents, Pulls};
    let link = |pr, at, seq| {
        forge_row(pr, I::Pr, K::ClosingRef, Some("closes"), at, seq).with_target(Some(7))
    };
    plant(
        root,
        &[
            link(101, s0 - Duration::hours(1), 1),
            link(102, s0 + Duration::minutes(90), 2),
            forge_row(7, I::Issue, K::LabelAdded, Some("loom:issue"), s0 - Duration::hours(1), 3),
        ],
    );
    stamp(root, Pulls, s0);
    stamp(root, IssuesEvents, s0);
}

fn star_of(root: &Path, pr: u32, cutoff: DateTime<Utc>) -> Option<bool> {
    use crate::eta::star::StarInputs;
    let inputs = StarInputs::load(root, &[A.to_string()]);
    inputs.repos[A]
        .state_at(pr, 0, None, cutoff)
        .map(|s| s.source.starred())
}

/// Two successive covered windows advance linked-issue star coverage from
/// SigNoz; a later transition changes only rows whose cutoffs include it, and
/// a link learned after a cutoff never stars that cutoff.
#[test]
fn signoz_star_history_advances_coverage_across_covered_windows() {
    use crate::eta::fleet_events_forge::ForgeEndpoint::{IssuesEvents, Pulls};
    use crate::eta::fleet_signoz_history::advance_star_coverage;
    const STAR: &str = "loom:operator-priority";
    let s0 = now() - Duration::days(2);
    let dir = tempfile::tempdir().unwrap();
    plant_star_cache(dir.path(), s0);

    // Window 1: starred at s0+1h.
    let l1 = s0 + Duration::hours(2);
    let plan = star_plan(&[(STAR, true, s0 + Duration::hours(1))], true, l1);
    assert!(advance_star_coverage(dir.path(), A, &plan));
    assert_eq!(stamp_of(dir.path(), IssuesEvents), Some(l1));
    // The pulls listing (links) is the forge's: until it refreshes, coverage
    // stays at its stamp.
    assert_eq!(star_of(dir.path(), 101, s0 + Duration::minutes(90)), None);
    stamp(dir.path(), Pulls, l1);
    assert_eq!(
        star_of(dir.path(), 101, s0 + Duration::minutes(30)),
        Some(false),
        "before the star"
    );
    assert_eq!(star_of(dir.path(), 101, s0 + Duration::minutes(90)), Some(true));
    assert_eq!(
        star_of(dir.path(), 101, l1 + Duration::hours(1)),
        None,
        "past coverage: unknown"
    );
    // PR 102's link became knowable at s0+90m: not starred before it.
    assert_eq!(star_of(dir.path(), 102, s0 + Duration::minutes(80)), Some(false));
    assert_eq!(star_of(dir.path(), 102, s0 + Duration::minutes(100)), Some(true));

    // Window 2: unstarred at s0+3h. Re-applying window 1's data is idempotent.
    let l2 = s0 + Duration::hours(4);
    let plan = star_plan(
        &[
            (STAR, true, s0 + Duration::hours(1)),
            (STAR, false, s0 + Duration::hours(3)),
        ],
        true,
        l2,
    );
    assert!(advance_star_coverage(dir.path(), A, &plan));
    assert!(advance_star_coverage(dir.path(), A, &plan));
    stamp(dir.path(), Pulls, l2);
    assert_eq!(
        star_of(dir.path(), 101, s0 + Duration::minutes(150)),
        Some(true),
        "window 1 row unchanged"
    );
    assert_eq!(
        star_of(dir.path(), 101, s0 + Duration::minutes(210)),
        Some(false),
        "after the unstar"
    );
    assert_eq!(star_of(dir.path(), 101, l2 + Duration::hours(1)), None);
}

/// A star transition after a training cutoff must not leak into that row, and
/// coverage SigNoz cannot prove is never advanced.
#[test]
fn signoz_star_coverage_needs_issue_history_and_never_reads_unstarred() {
    use crate::eta::fleet_events_forge::ForgeEndpoint::IssuesEvents;
    use crate::eta::fleet_signoz_history::advance_star_coverage;
    const STAR: &str = "loom:operator-priority";
    let s0 = now() - Duration::days(2);
    let l1 = s0 + Duration::hours(2);

    // PR rows alone (no issue row at all): the PR-history query succeeding
    // proves nothing about issue labels.
    let dir = tempfile::tempdir().unwrap();
    plant_star_cache(dir.path(), s0);
    let plan = star_plan(&[], false, l1);
    assert!(!advance_star_coverage(dir.path(), A, &plan));
    assert_eq!(stamp_of(dir.path(), IssuesEvents), Some(s0), "stamp did not move");

    // Issue rows that begin after the stamp leave a gap: not contiguous.
    let dir = tempfile::tempdir().unwrap();
    plant_star_cache(dir.path(), s0);
    let late = [(STAR, true, s0 + Duration::hours(1))];
    let plan = star_plan(&late, false, l1);
    assert!(!advance_star_coverage(dir.path(), A, &plan));
    assert_eq!(stamp_of(dir.path(), IssuesEvents), Some(s0));
    assert_eq!(star_of(dir.path(), 101, s0 + Duration::minutes(90)), None);

    // A change after the cutoff of a window is not in it.
    let dir = tempfile::tempdir().unwrap();
    plant_star_cache(dir.path(), s0);
    let plan = star_plan(&[(STAR, true, l1 + Duration::hours(1))], true, l1);
    assert!(advance_star_coverage(dir.path(), A, &plan));
    crate::eta::fleet_events::EventLog::open(&crate::eta::fleet_events::events_path(dir.path(), A))
        .unwrap();
    stamp(dir.path(), crate::eta::fleet_events_forge::ForgeEndpoint::Pulls, l1);
    assert_eq!(star_of(dir.path(), 101, l1), Some(false), "a later star never leaks back");

    // A cache with no stamp and no rows: nothing to advance from.
    let dir = tempfile::tempdir().unwrap();
    let plan = star_plan(&[(STAR, true, s0)], true, l1);
    assert!(!advance_star_coverage(dir.path(), A, &plan));
}

/// A production cycle (spending 3 calls per listing) over a covered repo whose
/// listings finished backfill.
fn star_cycle(
    root: &Path,
    signoz: &mut dyn SignozRead,
    gap: u64,
    stop: Option<StopReason>,
) -> (crate::eta::fleet_refresh::RepoReport, Seen) {
    use crate::eta::config::FleetRefreshConfig;
    use crate::observability::eta_fleet_refresh::{cycle_with, TaskState};
    let config = FleetRefreshConfig {
        gap_fill_max_calls_per_pass: gap,
        ..FleetRefreshConfig::default()
    };
    let mut forge = Counting::default();
    let mut seen = Seen::new();
    let mut events = |_: &RepoTarget,
                      _: &Reader,
                      endpoint: crate::eta::fleet_events_forge::ForgeEndpoint,
                      left: u64,
                      _: crate::eta::fleet_events::SyncMode| {
        seen.push((endpoint, left));
        (0, left.min(3), stop)
    };
    let outcome = cycle_with(
        root,
        &[target(root)],
        (&mut forge, Some((signoz, limits()))),
        &mut events,
        &config,
        &mut TaskState::default(),
        now(),
    );
    (outcome.report.repos[0].clone(), seen)
}

fn mark_backfilled(root: &Path) {
    use crate::eta::fleet_events::{cursor_path, EventsCursor};
    let file = cursor_path(root, A);
    let mut cursor = EventsCursor::read(&file, A);
    for key in crate::eta::star::listing_keys() {
        cursor.endpoints.entry(key).or_default().backfill_complete = true;
    }
    cursor.write(&file).unwrap();
}

/// At the production boundary a covered repo refreshes only the listings
/// SigNoz cannot answer for, counted against (and capped by) the per-repo
/// gap-fill budget: the pulls listing (links) always, the issue-events
/// listing only when SigNoz did not prove its coverage. An interrupted
/// refresh stamps nothing, so coverage stays frozen, never "unstarred".
#[test]
fn a_covered_repo_refreshes_only_the_star_listings_signoz_cannot_answer() {
    use crate::eta::fleet_events_forge::ForgeEndpoint::{IssuesEvents, Pulls};
    let s0 = now() - Duration::days(2);

    // SigNoz proves issue coverage: only the pulls listing is read.
    let dir = tempfile::tempdir().unwrap();
    plant_star_cache(dir.path(), s0);
    mark_backfilled(dir.path());
    let prs = [approved(101, now() - Duration::hours(10))];
    let mut signoz = signoz_rows(&prs, true);
    let (r, seen) = star_cycle(dir.path(), &mut signoz, 5, None);
    assert_eq!(seen, vec![(Pulls, 5)], "issue events come from SigNoz");
    assert_eq!((r.forge_calls, r.gap_fill_calls), (3, Some(3)), "counted as gap-fill");
    assert_eq!(stamp_of(dir.path(), IssuesEvents), Some(now()));
    assert_eq!(stamp_of(dir.path(), Pulls), Some(now()));
    assert_eq!(star_of(dir.path(), 101, now()), Some(false));

    // No issue rows in SigNoz: the issue-events listing is read too, within
    // the same cap (5 = 3 + the 2 left).
    let dir = tempfile::tempdir().unwrap();
    plant_star_cache(dir.path(), s0);
    mark_backfilled(dir.path());
    let old_pr = approved(90, now() - Duration::days(25));
    let mut signoz = signoz_rows(&[old_pr, prs[0].clone()], false);
    let (r, seen) = star_cycle(dir.path(), &mut signoz, 5, None);
    assert_eq!(seen, vec![(IssuesEvents, 5), (Pulls, 2)]);
    assert_eq!(r.gap_fill_calls, Some(5), "never over the budget");

    // An interrupted refresh stamps nothing: coverage stays frozen.
    let dir = tempfile::tempdir().unwrap();
    plant_star_cache(dir.path(), s0);
    mark_backfilled(dir.path());
    let mut signoz = signoz_rows(&prs, true);
    star_cycle(dir.path(), &mut signoz, 5, Some(StopReason::Budget));
    assert_eq!(stamp_of(dir.path(), Pulls), Some(s0));
    assert_eq!(star_of(dir.path(), 101, now()), None, "unknown, never unstarred");
}
