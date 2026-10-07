//! Fleet snapshot refresh state machine (#10263), over a fake forge.

use crate::eta::fleet::{self, FleetSnapshot};
use crate::eta::fleet_fetch::{
    self, ForgeRead, NoReader, Read, ReadFailure, Reader, RepoTarget, PER_PAGE,
};
use crate::eta::fleet_refresh::{
    self, choose_pass, fit_held, read_state, run_cycle, staging_path, state_path, write_state,
    Budgets, PassKind, RefreshState, StopReason, DERIVATION_REV,
};
use crate::forge_call_stats::ForgeOp;
use crate::pr_latency::{PrEvent, PrHistory, PrState};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

const A: &str = "acme/alpha";
const B: &str = "acme/beta";
const C: &str = "other/gamma";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

/// One listing row; `timeline` is `Some` for a PR.
#[derive(Clone)]
struct Row {
    number: u32,
    updated: DateTime<Utc>,
    created: DateTime<Utc>,
    merged: Option<DateTime<Utc>>,
    timeline: Option<Vec<serde_json::Value>>,
}

/// A merged PR whose last activity was `updated`: requested, approved and
/// merged in the two hours before it.
fn merged_pr(number: u32, updated: DateTime<Utc>) -> Row {
    let created = updated - Duration::hours(2);
    let at = |d: Duration| (created + d).to_rfc3339();
    Row {
        number,
        updated,
        created,
        merged: Some(updated),
        timeline: Some(vec![
            json!({"event": "labeled", "created_at": at(Duration::zero()),
                   "label": {"name": "loom:review-requested"}}),
            json!({"event": "labeled", "created_at": at(Duration::minutes(40)),
                   "label": {"name": "loom:pr"}}),
            json!({"event": "merged", "created_at": updated.to_rfc3339()}),
        ]),
    }
}

fn issue(number: u32, updated: DateTime<Utc>) -> Row {
    Row {
        number,
        updated,
        created: updated - Duration::hours(1),
        merged: None,
        timeline: None,
    }
}

/// `n` merged PRs, one an hour, newest an hour before [`now`].
fn prs(n: u32) -> Vec<Row> {
    (1..=n)
        .map(|i| merged_pr(1000 + i, now() - Duration::hours(i64::from(i))))
        .collect()
}

/// A fake forge: per-repo listings (kept newest-first), paged timelines, and
/// a log of every URL asked for.
#[derive(Default)]
struct Fake {
    repos: BTreeMap<String, Vec<Row>>,
    gets: Vec<String>,
    remaining: Option<u64>,
    /// `remaining` for one repo owner's installation instead (#10329).
    remaining_by_owner: BTreeMap<String, u64>,
    /// Fail the call with this 0-based index.
    fail_at: Option<(usize, ReadFailure, Option<i64>)>,
    breaker: bool,
}

impl Fake {
    fn with(repos: &[(&str, Vec<Row>)]) -> Self {
        let mut fake = Fake::default();
        for (repo, rows) in repos {
            fake.set(repo, rows.clone());
        }
        fake
    }

    fn set(&mut self, repo: &str, mut rows: Vec<Row>) {
        rows.sort_by(|a, b| b.updated.cmp(&a.updated).then(b.number.cmp(&a.number)));
        self.repos.insert(repo.to_string(), rows);
    }

    fn etag(rows: &[Row]) -> String {
        let key: Vec<String> = rows
            .iter()
            .take(PER_PAGE)
            .map(|r| format!("{}@{}", r.number, r.updated.timestamp()))
            .collect();
        format!("W/\"{}\"", crate::short_hash::short_sha16(&key.join(",")))
    }

    /// Timeline reads of `repo`'s PR `number`.
    fn timeline_reads(&self, repo: &str, number: u32) -> usize {
        let needle = format!("repos/{repo}/issues/{number}/timeline");
        self.gets.iter().filter(|u| u.starts_with(&needle)).count()
    }
}

fn page_of(url: &str) -> usize {
    url.rsplit("page=").next().unwrap().parse().unwrap()
}

impl ForgeRead for Fake {
    fn breaker_open(&self) -> bool {
        self.breaker
    }

    fn get(
        &mut self,
        target: &RepoTarget,
        _reader: &Reader,
        url: &str,
        etag: Option<&str>,
        _op: ForgeOp,
    ) -> Read {
        let index = self.gets.len();
        self.gets.push(url.to_string());
        let owner = target.repo.split('/').next().unwrap_or_default();
        let remaining = self
            .remaining_by_owner
            .get(owner)
            .copied()
            .or(self.remaining);
        if let Some((at, failure, reset_epoch)) = self.fail_at {
            if at == index {
                return Read::Failed {
                    failure,
                    remaining,
                    reset_epoch,
                    detail: "fake failure".to_string(),
                };
            }
        }
        let rows = self.repos.get(&target.repo).cloned().unwrap_or_default();
        let page = page_of(url);
        let ok = |status: u16, etag: Option<String>, body: String| Read::Ok {
            status,
            etag,
            body,
            remaining,
        };
        if let Some(rest) = url
            .split("/issues/")
            .nth(1)
            .filter(|r| r.contains("/timeline?"))
        {
            let number: u32 = rest.split('/').next().unwrap().parse().unwrap();
            let entries = rows
                .iter()
                .find(|r| r.number == number)
                .and_then(|r| r.timeline.clone())
                .unwrap_or_default();
            let slice: Vec<_> = entries
                .into_iter()
                .skip((page - 1) * PER_PAGE)
                .take(PER_PAGE)
                .collect();
            return ok(200, None, serde_json::to_string(&slice).unwrap());
        }
        let current = Fake::etag(&rows);
        if page == 1 && etag == Some(current.as_str()) {
            return ok(304, Some(current), String::new());
        }
        let slice: Vec<serde_json::Value> = rows
            .iter()
            .skip((page - 1) * PER_PAGE)
            .take(PER_PAGE)
            .map(|r| {
                let mut row = json!({
                    "number": r.number,
                    "state": if r.merged.is_some() { "closed" } else { "open" },
                    "created_at": r.created.to_rfc3339(),
                    "updated_at": r.updated.to_rfc3339(),
                    "labels": [],
                });
                if r.timeline.is_some() {
                    row["pull_request"] = json!({"merged_at": r.merged.map(|m| m.to_rfc3339())});
                }
                row
            })
            .collect();
        ok(200, (page == 1).then_some(current), serde_json::to_string(&slice).unwrap())
    }
}

fn target(root: &Path, repo: &str, app: &str) -> RepoTarget {
    RepoTarget {
        repo: repo.to_string(),
        host: Some("github.com".to_string()),
        cwd: root.to_path_buf(),
        reader: Ok(Reader {
            app_id: app.to_string(),
            dir: root.join(format!("reader-{app}")),
        }),
    }
}

fn budgets(refresh: u64, backfill: u64) -> Budgets {
    Budgets {
        refresh,
        backfill,
        reserve: 1500,
        backfill_days: 21,
        gap_fill: 100,
    }
}

fn published(root: &Path, repo: &str) -> Option<FleetSnapshot> {
    fleet::read(&fleet::snapshot_path(root, repo))
}

// -- fresh host: backfill ---------------------------------------------------

#[test]
fn a_fresh_host_publishes_every_repo_in_one_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(5)), (B, prs(3))]);
    let targets = [target(root, A, "1"), target(root, B, "1")];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());

    for (repo, n) in [(A, 5u64), (B, 3)] {
        let r = report.repos.iter().find(|r| r.repo == repo).unwrap();
        assert_eq!(r.stop, StopReason::Complete, "{repo}");
        assert_eq!(r.pass, Some(PassKind::Backfill));
        assert!(r.promoted);
        assert_eq!(r.prs_read, n);
        assert_eq!(r.forge_calls, 1 + n, "one listing page plus one timeline per PR");
        let snapshot = published(root, repo).expect("published");
        assert_eq!(snapshot.as_of, now());
        assert_eq!(snapshot.prs.len() as u64, n);
        assert_eq!(r.samples_added, snapshot.samples.len() as i64);
        assert_eq!(r.snapshot_id.as_deref(), Some(snapshot.snapshot_id.as_str()));
        let state = read_state(&state_path(root, repo)).unwrap();
        assert_eq!(state.watermark, Some(now()));
        assert_eq!(state.covered_since, Some(now() - Duration::days(21)));
        assert_eq!(state.derivation_rev, DERIVATION_REV);
        assert!(state.pass.is_none());
        assert!(state.listing_etag.is_some());
        assert!(!staging_path(root, repo).exists());
    }
    assert_eq!(report.backfill_calls, 6 + 4);
    assert_eq!(report.refresh_calls, 0);
    assert_eq!(report.backfill_in_progress_since, None);
}

#[test]
fn a_backfill_reads_only_its_window_and_skips_issues() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut rows = prs(3);
    rows.push(issue(7, now() - Duration::hours(2)));
    rows.push(merged_pr(8, now() - Duration::days(22)));
    rows.push(merged_pr(9, now() - Duration::days(30)));
    let mut fake = Fake::with(&[(A, rows)]);
    let report = run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(300, 1500), now());
    let r = &report.repos[0];
    assert_eq!(r.stop, StopReason::Complete);
    assert_eq!(r.prs_read, 3, "the issue is not a PR; #8 and #9 are older than S");
    assert_eq!(
        fake.timeline_reads(A, 8) + fake.timeline_reads(A, 9) + fake.timeline_reads(A, 7),
        0
    );
}

#[test]
fn a_quiet_repo_with_nothing_to_read_still_publishes_as_of_now() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, Vec::new())]);
    let report = run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(300, 1500), now());
    assert_eq!(report.repos[0].stop, StopReason::Complete);
    assert_eq!(published(root, A).unwrap().as_of, now(), "the fit gate reads as_of");
}

// -- exact resume -----------------------------------------------------------

/// A backfill stopped by its budget mid-page and resumed over later cycles
/// (same `L`) publishes the same bytes as an uninterrupted run at that `L`,
/// and reads every PR's timeline exactly once.
#[test]
fn an_interrupted_backfill_resumes_to_a_byte_identical_snapshot() {
    let rows = prs(150);

    let whole = tempfile::tempdir().unwrap();
    let mut fake = Fake::with(&[(A, rows.clone())]);
    let report = run_cycle(
        whole.path(),
        &[target(whole.path(), A, "1")],
        &mut fake,
        budgets(300, 1500),
        now(),
    );
    assert_eq!(report.repos[0].stop, StopReason::Complete);
    assert_eq!(report.repos[0].forge_calls, 2 + 150);
    let expected = std::fs::read(fleet::snapshot_path(whole.path(), A)).unwrap();

    let split = tempfile::tempdir().unwrap();
    let root = split.path();
    let mut fake = Fake::with(&[(A, rows)]);
    let mut cycles = 0;
    loop {
        let at = now() + Duration::hours(cycles);
        let report = run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(300, 40), at);
        cycles += 1;
        let r = &report.repos[0];
        if r.stop == StopReason::Complete {
            assert!(r.promoted);
            break;
        }
        assert_eq!(r.stop, StopReason::Budget);
        assert_eq!(r.forge_calls, 40);
        assert!(published(root, A).is_none(), "nothing is published before the pass completes");
        let state = read_state(&state_path(root, A)).unwrap();
        let pass = state.pass.expect("a pass in progress");
        assert_eq!(pass.listed_at, now(), "every cycle continues the same pass");
        assert_eq!(state.last_stop.unwrap().reason, StopReason::Budget);
        assert_eq!(report.backfill_in_progress_since, Some(now()));
        assert!(cycles < 20, "the pass must finish");
    }
    assert!(cycles > 3);
    assert_eq!(std::fs::read(fleet::snapshot_path(root, A)).unwrap(), expected);
    for i in 1..=150 {
        assert_eq!(fake.timeline_reads(A, 1000 + i), 1, "PR {} read once", 1000 + i);
    }
}

// -- incremental refresh ----------------------------------------------------

fn backfilled(root: &Path, fake: &mut Fake) {
    let report = run_cycle(root, &[target(root, A, "1")], fake, budgets(300, 1500), now());
    assert_eq!(report.repos[0].stop, StopReason::Complete);
    fake.gets.clear();
}

#[test]
fn a_quiet_refresh_costs_one_304_and_advances_as_of_and_the_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(5))]);
    backfilled(root, &mut fake);
    let before = published(root, A).unwrap();

    let later = now() + Duration::hours(1);
    let report = run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(300, 1500), later);
    let r = &report.repos[0];
    assert_eq!(r.pass, Some(PassKind::Refresh));
    assert_eq!(r.stop, StopReason::NotModified);
    assert_eq!((r.forge_calls, r.not_modified_calls), (1, 1));
    assert_eq!(report.refresh_calls, 1);
    let after = published(root, A).unwrap();
    assert_eq!(after.as_of, later);
    assert_eq!(after.samples, before.samples);
    assert_eq!(read_state(&state_path(root, A)).unwrap().watermark, Some(later));
}

#[test]
fn an_active_refresh_reads_only_prs_updated_since_the_watermark_slack() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut rows = prs(5);
    let mut fake = Fake::with(&[(A, rows.clone())]);
    backfilled(root, &mut fake);

    // Two new PRs, and one just inside the 10-minute slack before the
    // watermark (updated after the backfill listed it).
    rows.push(merged_pr(2001, now() + Duration::minutes(30)));
    rows.push(merged_pr(2002, now() + Duration::minutes(20)));
    rows.push(merged_pr(2003, now() - Duration::minutes(5)));
    fake.set(A, rows);
    let later = now() + Duration::hours(1);
    let report = run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(300, 1500), later);
    let r = &report.repos[0];
    assert_eq!(r.pass, Some(PassKind::Refresh));
    assert_eq!(r.stop, StopReason::Complete);
    assert_eq!(r.prs_read, 3);
    assert_eq!(r.forge_calls, 1 + 3);
    for old in 1001..=1005 {
        assert_eq!(fake.timeline_reads(A, old), 0, "#{old} predates the watermark");
    }
    let snapshot = published(root, A).unwrap();
    assert!(snapshot.prs.contains(&2001) && snapshot.prs.contains(&1001));
    assert!(r.samples_added > 0);
    assert_eq!(read_state(&state_path(root, A)).unwrap().watermark, Some(later));
}

// -- stops resume cleanly ---------------------------------------------------

#[test]
fn the_reserve_keeps_the_page_and_skips_the_rest_of_that_reader() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(3)), (B, prs(3)), (C, prs(2))]);
    fake.remaining = Some(100);
    let targets = [
        target(root, A, "1"),
        target(root, B, "1"),
        target(root, C, "2"),
    ];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());
    let by = |repo: &str| {
        report
            .repos
            .iter()
            .find(|r| r.repo == repo)
            .unwrap()
            .clone()
    };
    assert_eq!(by(A).stop, StopReason::Reserve);
    assert_eq!(by(A).forge_calls, 1, "the page that reported the low reserve is kept");
    assert_eq!(by(A).ratelimit_remaining_min, Some(100));
    assert_eq!((by(B).stop, by(B).forge_calls), (StopReason::Reserve, 0), "same reader App");
    assert_eq!(by(C).stop, StopReason::Reserve, "C's own reads report the low reserve too");
    assert!(read_state(&state_path(root, A)).unwrap().pass.is_some());

    fake.remaining = Some(4000);
    let report =
        run_cycle(root, &targets, &mut fake, budgets(300, 1500), now() + Duration::hours(1));
    assert!(report.repos.iter().all(|r| r.stop == StopReason::Complete), "{report:?}");
    assert_eq!(published(root, A).unwrap().as_of, now(), "A resumed its pass at L");
}

/// The reserve is per installation (App and owner, #10329): a low bucket for
/// (App 1, acme) skips acme's other repos on App 1, never another owner's.
#[test]
fn the_reserve_skips_that_installation_not_the_apps_other_owners() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(3)), (B, prs(3)), (C, prs(2))]);
    fake.remaining = Some(4000);
    fake.remaining_by_owner.insert("acme".to_string(), 100);
    let targets = [
        target(root, A, "1"),
        target(root, B, "1"),
        target(root, C, "1"),
    ];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());
    let by = |repo: &str| {
        report
            .repos
            .iter()
            .find(|r| r.repo == repo)
            .unwrap()
            .clone()
    };
    assert_eq!((by(A).stop, by(A).forge_calls), (StopReason::Reserve, 1));
    assert_eq!((by(B).stop, by(B).forge_calls), (StopReason::Reserve, 0), "(1, acme)");
    assert_eq!(by(C).stop, StopReason::Complete, "(1, other) is another bucket");
    assert!(by(C).forge_calls > 0);
}

#[test]
fn a_rate_limit_ends_the_cycle_and_reports_the_reset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(3)), (B, prs(3))]);
    fake.fail_at = Some((2, ReadFailure::RateLimited, Some(1_791_200_000)));
    let targets = [target(root, A, "1"), target(root, B, "2")];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());
    assert_eq!(report.repos[0].stop, StopReason::RateLimited);
    assert_eq!(report.repos[0].forge_calls, 3);
    assert_eq!(
        (report.repos[1].stop, report.repos[1].forge_calls),
        (StopReason::RateLimited, 0)
    );
    assert_eq!(report.rate_limited, Some(Some(1_791_200_000)));
    assert_eq!(fake.gets.len(), 3, "no retry anywhere after a failed read");
    let pass = read_state(&state_path(root, A)).unwrap().pass.unwrap();
    assert_eq!(pass.done.len(), 1);

    fake.fail_at = None;
    let report =
        run_cycle(root, &targets, &mut fake, budgets(300, 1500), now() + Duration::hours(1));
    assert!(report.repos.iter().all(|r| r.stop == StopReason::Complete));
    assert_eq!(fake.timeline_reads(A, 1001), 1, "a PR in `done` is not read again");
}

#[test]
fn an_open_breaker_makes_no_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(3)), (B, prs(3))]);
    fake.breaker = true;
    let targets = [target(root, A, "1"), target(root, B, "2")];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());
    assert!(fake.gets.is_empty());
    assert!(report
        .repos
        .iter()
        .all(|r| r.stop == StopReason::BreakerOpen && r.forge_calls == 0));
    // A stopped at its first call, B skipped whole: both are in progress (#10292).
    for repo in [A, B] {
        let pass = read_state(&state_path(root, repo)).unwrap().pass.unwrap();
        assert_eq!((pass.kind, pass.listed_at), (PassKind::Backfill, now()), "{repo}");
    }
    assert!(fit_held(report.backfill_in_progress_since, now()));
}

#[test]
fn a_coverage_gap_stops_only_that_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(2)), (B, prs(2))]);
    fake.fail_at = Some((0, ReadFailure::Coverage, None));
    let targets = [target(root, A, "1"), target(root, B, "1")];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());
    assert_eq!(report.repos[0].stop, StopReason::Coverage);
    assert_eq!(report.repos[1].stop, StopReason::Complete);
    assert!(published(root, A).is_none());
}

#[test]
fn a_crash_orphan_is_repaired_on_the_next_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(150)), (B, prs(2))]);
    let targets = [target(root, A, "1")];
    run_cycle(root, &targets, &mut fake, budgets(300, 10), now());
    assert!(read_state(&state_path(root, A)).unwrap().pass.is_some());

    // (1) A pass whose staging file is unreadable restarts, at a new `L`.
    std::fs::write(staging_path(root, A), "{ torn").unwrap();
    let later = now() + Duration::hours(1);
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), later);
    assert_eq!(report.repos[0].stop, StopReason::Complete);
    assert_eq!(published(root, A).unwrap().as_of, later);

    // (2) A staging file with no pass is deleted. B has no reader, so the
    // cycle never pends it a fresh staging file (#10292); recovery still runs.
    let orphan = staging_path(root, B);
    fleet::write(&orphan, &FleetSnapshot::empty(B)).unwrap();
    let mut state = RefreshState::new(B);
    state.last_stop = None;
    write_state(&state_path(root, B), &state).unwrap();
    let mut b = target(root, B, "1");
    b.reader = Err(NoReader::NoReader);
    run_cycle(root, &[b], &mut fake, budgets(300, 0), later);
    assert!(!orphan.exists());
}

// -- reader-only ------------------------------------------------------------

#[test]
fn no_usable_reader_costs_zero_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(2)), (B, prs(2))]);
    let mut a = target(root, A, "1");
    a.reader = Err(NoReader::NoReader);
    let mut b = target(root, B, "1");
    b.reader = Err(NoReader::UnsupportedForge);
    let report = run_cycle(root, &[a, b], &mut fake, budgets(300, 1500), now());
    assert!(fake.gets.is_empty());
    let stops: Vec<StopReason> = report.repos.iter().map(|r| r.stop).collect();
    assert_eq!(stops, vec![StopReason::NoReader, StopReason::UnsupportedForge]);
    assert!(!fleet_refresh::refresh_dir(root).exists(), "never pended (#10292)");
    assert_eq!(report.backfill_in_progress_since, None);
}

/// The new forge path can reach the forge only through the reader-only
/// primitive. Needles are assembled at runtime so this file does not match
/// itself.
#[test]
fn the_refresh_sources_reach_the_forge_only_through_the_reader() {
    const SOURCES: &[(&str, &str)] = &[
        ("eta/fleet_fetch.rs", include_str!("../fleet_fetch.rs")),
        ("eta/fleet_refresh.rs", include_str!("../fleet_refresh.rs")),
        (
            "observability/eta_fleet_refresh.rs",
            include_str!("../../observability/eta_fleet_refresh.rs"),
        ),
    ];
    let forbidden = [
        ["fetch_", "conditional("].concat(),
        ["run_", "gh"].concat(),
        ["gh_", "query"].concat(),
        ["Command", "::new"].concat(),
        ["gh_config_dir", "_for_root"].concat(),
    ];
    for (name, source) in SOURCES {
        for needle in &forbidden {
            assert!(!source.contains(needle.as_str()), "{name} mentions `{needle}`");
        }
    }
    assert!(SOURCES[0].1.contains("fetch_with_reader("));
}

// -- files never leak into load_all ------------------------------------------

#[test]
fn state_and_staging_files_never_appear_in_load_all() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(150)), (B, prs(2))]);
    run_cycle(root, &[target(root, B, "1")], &mut fake, budgets(300, 1500), now());
    run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(300, 10), now());
    assert!(staging_path(root, A).exists() && state_path(root, A).exists());
    let loaded = fleet::load_all(root);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].repo, B);
}

// -- REST → PrHistory -------------------------------------------------------

#[test]
fn rest_rows_and_a_multi_page_timeline_map_to_the_hand_built_history() {
    let t = |h: u32| Utc.with_ymd_and_hms(2026, 10, 1, h, 0, 0).unwrap();
    let body = json!([
        {"number": 1, "state": "closed", "created_at": t(1), "updated_at": t(9),
         "labels": [{"name": "loom:pr"}], "pull_request": {"merged_at": t(8)}},
        {"number": 2, "state": "closed", "created_at": t(2), "updated_at": t(7),
         "labels": [], "pull_request": {"merged_at": null}},
        {"number": 3, "state": "open", "created_at": t(3), "updated_at": t(6),
         "labels": [{"name": "loom:review-requested"}], "pull_request": {}},
        {"number": 4, "state": "open", "created_at": t(4), "updated_at": t(5), "labels": []}
    ])
    .to_string();
    let rows = fleet_fetch::parse_listing(&body).unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows[3].pr.is_none(), "an issue row");
    let states: Vec<PrState> = rows[..3]
        .iter()
        .map(|r| r.pr.as_ref().unwrap().state)
        .collect();
    assert_eq!(states, vec![PrState::Merged, PrState::Closed, PrState::Open]);

    // 101 entries across two explicit pages: the first is full, the second short.
    let mut entries: Vec<serde_json::Value> = (0..100)
        .map(|_| json!({"event": "subscribed", "created_at": t(1)}))
        .collect();
    entries[0] = json!({"event": "labeled", "created_at": t(2), "label": {"name": "loom:pr"}});
    let page1 = serde_json::to_vec(&entries).unwrap();
    let page2 = serde_json::to_vec(&[json!({"event": "merged", "created_at": t(8)})]).unwrap();
    let (mut events, raw1) = crate::pr_latency::timeline::parse_timeline_page(&page1).unwrap();
    let (more, raw2) = crate::pr_latency::timeline::parse_timeline_page(&page2).unwrap();
    assert_eq!((raw1, raw2), (PER_PAGE, 1));
    events.extend(more);

    let built = fleet_fetch::history(1, rows[0].pr.as_ref().unwrap(), events, true);
    let hand = PrHistory::new(
        1,
        t(1),
        PrState::Merged,
        Some(t(8)),
        vec!["loom:pr".to_string()],
        vec![
            PrEvent::Labeled {
                label: "loom:pr".to_string(),
                at: t(2),
            },
            PrEvent::Merged { at: t(8) },
        ],
        true,
    );
    let fields = |h: &PrHistory| {
        (
            h.number,
            h.created_at,
            h.state,
            h.merged_at,
            h.current_labels.clone(),
            h.events.clone(),
            h.timeline_complete,
        )
    };
    assert_eq!(fields(&built), fields(&hand));
}

// -- pass choice and the fit hold ---------------------------------------------

#[test]
fn a_pass_is_a_backfill_until_coverage_and_derivation_are_current() {
    let days = 21;
    assert_eq!(choose_pass(None, true, now(), days), PassKind::Backfill, "no state file");
    let mut state = RefreshState::new(A);
    state.watermark = Some(now() - Duration::hours(1));
    state.covered_since = Some(now() - Duration::days(22));
    state.derivation_rev = DERIVATION_REV;
    assert_eq!(choose_pass(Some(&state), true, now(), days), PassKind::Refresh);
    assert_eq!(choose_pass(Some(&state), false, now(), days), PassKind::Backfill, "no snapshot");
    let mut old = state.clone();
    old.derivation_rev = DERIVATION_REV - 1;
    assert_eq!(
        choose_pass(Some(&old), true, now(), days),
        PassKind::Backfill,
        "derivation bump"
    );
    let mut shallow = state.clone();
    shallow.covered_since = Some(now() - Duration::days(4));
    assert_eq!(
        choose_pass(Some(&shallow), true, now(), days),
        PassKind::Backfill,
        "CLI-depth snapshot"
    );
    assert_eq!(
        choose_pass(Some(&state), true, now(), 30),
        PassKind::Backfill,
        "backfillDays raised"
    );
}

#[test]
fn an_in_progress_backfill_holds_the_fit_for_six_hours() {
    assert!(!fit_held(None, now()));
    assert!(fit_held(Some(now()), now() + Duration::hours(5)));
    assert!(!fit_held(Some(now()), now() + Duration::hours(fleet_refresh::FIT_HOLD_HOURS)));
}

// -- a backfill skipped before its first call holds the fit (#10292) ----------

/// B's pending pass: a backfill begun at `at`, nothing read yet, staging there.
fn assert_pended(root: &Path, repo: &str, at: DateTime<Utc>) {
    let pass = read_state(&state_path(root, repo))
        .unwrap()
        .pass
        .expect("a pending pass");
    assert_eq!(pass.kind, PassKind::Backfill, "{repo}");
    assert_eq!(pass.listed_at, at, "{repo}");
    assert_eq!((pass.next_page, pass.done.len()), (1, 0), "{repo}");
    assert!(staging_path(root, repo).exists(), "{repo}: recover() keeps the pass");
}

#[test]
fn a_backfill_skipped_for_the_reserve_holds_the_fit_until_l_plus_six_hours() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, Vec::new()), (B, prs(3))]);
    fake.remaining = Some(100);
    let targets = [target(root, A, "1"), target(root, B, "1")];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());
    assert_eq!(report.repos[0].stop, StopReason::Complete);
    assert!(report.repos[0].promoted, "A finished first and is published");
    assert_eq!((report.repos[1].stop, report.repos[1].forge_calls), (StopReason::Reserve, 0));
    assert_pended(root, B, now());
    assert_eq!(report.backfill_in_progress_since, Some(now()));
    assert!(fit_held(report.backfill_in_progress_since, now()));

    // Skipped again an hour later: B resumes its pass, so `L` stays put.
    let later = now() + Duration::hours(1);
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), later);
    assert_eq!((report.repos[1].stop, report.repos[1].forge_calls), (StopReason::Reserve, 0));
    assert_pended(root, B, now());
    assert!(fit_held(report.backfill_in_progress_since, later));

    // Still skipped at `L + 6 h`: the hold has lapsed.
    let lapsed = now() + Duration::hours(fleet_refresh::FIT_HOLD_HOURS);
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), lapsed);
    assert_eq!(report.repos[1].stop, StopReason::Reserve);
    assert_pended(root, B, now());
    assert!(!fit_held(report.backfill_in_progress_since, lapsed));
}

#[test]
fn a_backfill_budget_spent_exactly_at_a_repo_boundary_pends_the_next_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(2)), (B, prs(3))]);
    let targets = [target(root, A, "1"), target(root, B, "1")];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 3), now());
    assert_eq!((report.repos[0].stop, report.repos[0].forge_calls), (StopReason::Complete, 3));
    assert_eq!(report.remaining.1, 0);
    assert_eq!((report.repos[1].stop, report.repos[1].forge_calls), (StopReason::Budget, 0));
    assert_pended(root, B, now());
    assert_eq!(report.backfill_in_progress_since, Some(now()));
}

#[test]
fn a_halted_cycle_pends_the_backfills_it_never_reached() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(3)), (B, prs(3))]);
    fake.fail_at = Some((0, ReadFailure::RateLimited, None));
    let targets = [target(root, A, "1"), target(root, B, "2")];
    let report = run_cycle(root, &targets, &mut fake, budgets(300, 1500), now());
    assert_eq!(report.repos[0].stop, StopReason::RateLimited);
    assert_eq!(
        (report.repos[1].stop, report.repos[1].forge_calls),
        (StopReason::RateLimited, 0)
    );
    assert_pended(root, B, now());
    assert!(fit_held(report.backfill_in_progress_since, now()));
}

#[test]
fn a_skipped_refresh_is_not_pended_and_does_not_hold_the_fit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(2))]);
    backfilled(root, &mut fake);
    let later = now() + Duration::hours(1);
    let report = run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(0, 1500), later);
    assert_eq!(
        (report.repos[0].pass, report.repos[0].stop),
        (Some(PassKind::Refresh), StopReason::Budget)
    );
    assert!(read_state(&state_path(root, A)).unwrap().pass.is_none());
    assert!(!staging_path(root, A).exists());
    assert_eq!(report.backfill_in_progress_since, None);
}

#[test]
fn pending_never_rewrites_a_pass_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut fake = Fake::with(&[(A, prs(150))]);
    run_cycle(root, &[target(root, A, "1")], &mut fake, budgets(300, 10), now());
    let before = read_state(&state_path(root, A)).unwrap();
    assert!(before.pass.is_some());
    let later = now() + Duration::hours(2);
    fleet_refresh::pend_backfill(root, A, Some(before.clone()), later, 21, StopReason::Budget)
        .unwrap();
    assert_eq!(read_state(&state_path(root, A)).unwrap(), before);
}
