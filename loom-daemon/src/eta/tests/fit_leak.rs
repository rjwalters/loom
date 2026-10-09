//! The leak test (#10245): nothing knowable at or after `T − 120 s` may move
//! a fit at cutoff `T`. Each perturbation below must leave the coefficient
//! file **byte-identical**; a positive control (one merge an hour later, still
//! before the cutoff) must change it, and the baseline must actually fit, so
//! the test cannot pass vacuously. Also: the runner's writes, retention and
//! daily trigger.

use super::fit_rows::{approve, open, snapshot, OPERATOR, OTHER, REPO, STAR};
use crate::eta::fit::coeffs::{self, Fitter};
use crate::eta::fit::{fit_dir, path_for, rows, run};
use crate::eta::fleet::{self, FleetSnapshot};
use crate::eta::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};
use crate::eta::star::{RepoStar, StarInputs};
use crate::eta::Stage;
use crate::pr_latency::history::fixtures::{labeled, t, unlabeled};
use crate::pr_latency::history::{PrEvent, PrHistory, PrState};
use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use chrono::{DateTime, Duration, Utc};
use std::path::Path;

/// The cutoff: 2026-09-29T00:00Z, so the window opens on day 14.
fn end() -> DateTime<Utc> {
    t(28 * 86_400)
}

/// Every baseline snapshot is as of six hours after the cutoff.
fn as_of() -> DateTime<Utc> {
    end() + Duration::hours(6)
}

fn fitter() -> Fitter {
    Fitter {
        version: "0.19.0".to_string(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
    }
}

/// Deterministic noise in `[0, 1)`.
fn u(i: u32, k: u32) -> f64 {
    let x = (u64::from(i) * 2_654_435_761 + u64::from(k) * 40_503 + 12_345) % 1_000_003;
    x as f64 / 1_000_003.0
}

fn hours(x: f64) -> i64 {
    (x * 3600.0).round() as i64
}

fn secs(at: DateTime<Utc>) -> i64 {
    (at - t(0)).num_seconds()
}

/// One synthetic PR from `start` (seconds): review, sometimes a Doctor round,
/// approval, sometimes an operator hold, then merged (rarely closed). Some are
/// starred, some get a merge conflict.
fn synthetic(number: u32, start: i64) -> PrHistory {
    let mut at = start;
    let mut events = vec![labeled(REVIEW_REQUESTED, at)];
    if u(number, 9) < 0.2 {
        events.push(labeled(STAR, at));
    }
    at += hours(0.5 + 4.0 * u(number, 1));
    if u(number, 2) < 0.3 {
        events.push(unlabeled(REVIEW_REQUESTED, at));
        events.push(labeled(CHANGES_REQUESTED, at));
        at += hours(1.0 + 3.0 * u(number, 3));
        events.push(unlabeled(CHANGES_REQUESTED, at));
        events.push(labeled(REVIEW_REQUESTED, at));
        at += hours(0.5 + 2.0 * u(number, 4));
    }
    events.push(unlabeled(REVIEW_REQUESTED, at));
    events.push(labeled(APPROVED, at));
    at += hours(0.3 + 3.0 * u(number, 5));
    if u(number, 6) < 0.15 {
        events.push(labeled(OPERATOR, at));
        at += hours(1.0 + 5.0 * u(number, 7));
        events.push(unlabeled(OPERATOR, at));
        at += hours(0.2 + u(number, 8));
    }
    if u(number, 10) < 0.1 {
        events.push(labeled("loom:merge-conflict", at - 600));
    }
    if u(number, 11) < 0.07 {
        PrHistory::new(number, t(start), PrState::Closed, None, Vec::new(), events, true)
            .with_closed_at(Some(t(at)))
    } else {
        events.push(PrEvent::Merged { at: t(at) });
        PrHistory::new(number, t(start), PrState::Merged, Some(t(at)), Vec::new(), events, true)
    }
}

/// PRs `base…` opened every `every_h` hours from day 7 until 12 h before
/// the cutoff.
fn stream(base: u32, every_h: f64) -> Vec<PrHistory> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let start = hours(7.0 * 24.0 + f64::from(i) * every_h + u(base + i, 0));
        if t(start) > end() - Duration::hours(12) {
            return out;
        }
        out.push(synthetic(base + i, start));
        i += 1;
    }
}

/// Review at `T − 20 h`, approved at `T − 13 h`, merged at `merged`.
fn control(merged: DateTime<Utc>) -> PrHistory {
    let mut events = approve(end() - Duration::hours(20), end() - Duration::hours(13));
    events.push(PrEvent::Merged { at: merged });
    PrHistory::new(9000, t(0), PrState::Merged, Some(merged), Vec::new(), events, true)
}

/// Open across the cutoff: review at `T − 8 h`, approved at `T − 2 h`.
fn across_events() -> Vec<PrEvent> {
    approve(end() - Duration::hours(8), end() - Duration::hours(2))
}

/// Its baseline outcome: merged at `T + 3 h`.
fn across() -> PrHistory {
    let merged = end() + Duration::hours(3);
    let mut events = across_events();
    events.push(PrEvent::Merged { at: merged });
    PrHistory::new(9001, t(0), PrState::Merged, Some(merged), Vec::new(), events, true)
}

#[derive(Clone)]
struct Fleet {
    a: Vec<PrHistory>,
    b: Vec<PrHistory>,
}

impl Fleet {
    fn baseline() -> Self {
        let mut a = stream(1000, 3.0);
        a.push(control(end() - Duration::hours(3)));
        a.push(across());
        Fleet {
            a,
            b: stream(5000, 5.0),
        }
    }

    fn snapshots(&self) -> Vec<FleetSnapshot> {
        vec![
            snapshot(REPO, &self.a, as_of()),
            snapshot(OTHER, &self.b, as_of()),
        ]
    }

    fn replace(&mut self, pr: PrHistory) {
        for h in self.a.iter_mut().chain(self.b.iter_mut()) {
            if h.number == pr.number {
                *h = pr.clone();
            }
        }
    }

    fn each(&mut self, f: impl Fn(&PrHistory) -> PrHistory) {
        for h in self.a.iter_mut().chain(self.b.iter_mut()) {
            *h = f(h);
        }
    }
}

/// `h` with `extra` events.
fn with_events(h: &PrHistory, extra: &[PrEvent]) -> PrHistory {
    let mut events = h.events.clone();
    events.extend_from_slice(extra);
    PrHistory::new(
        h.number,
        h.created_at,
        h.state,
        h.merged_at,
        h.current_labels.clone(),
        events,
        h.timeline_complete,
    )
    .with_closed_at(h.closed_at)
}

/// The file's bytes for `snapshots` at the cutoff.
/// The v1 file's bytes followed by the `eta-fit/v2` file's (#10508): every
/// perturbation below must leave both byte-identical.
fn bytes(snapshots: &[FleetSnapshot]) -> String {
    let (file, assembled) = run::fit_snapshots(snapshots, end(), &fitter());
    let v2 = run::fit_v2_of(&assembled, end(), &fitter());
    format!("{}{}", coeffs::to_json(&file), coeffs::to_json(&v2))
}

fn baseline_bytes() -> String {
    bytes(&Fleet::baseline().snapshots())
}

// -- the baseline is not vacuous ----------------------------------------------

#[test]
fn the_baseline_fits_and_the_positive_control_moves_the_file() {
    let snapshots = Fleet::baseline().snapshots();
    let (file, assembled) = run::fit_snapshots(&snapshots, end(), &fitter());
    assert!(!file.hazard.is_empty(), "at least one hazard stage: {:?}", file.hazard_skipped);
    assert!(file.aft.is_some(), "the direct model fits");
    assert_eq!(file.window.data_through, Some(end() - Duration::seconds(120)));
    assert_eq!(assembled.stats.rows_dropped_no_flags, 0);
    let inputs: Vec<_> = assembled.rows.iter().map(|r| r.inputs).collect();
    assert!(inputs.iter().any(|i| i.starred), "flags reach the rows");
    assert!(inputs.iter().any(|i| i.op_hold));
    assert!(inputs.iter().any(|i| i.conflict));
    assert!(inputs.iter().any(|i| i.rework >= 1));
    assert!(assembled.rows.iter().any(|r| r.exit.is_none()), "rows near the horizon");
    let baseline = bytes(&snapshots);
    assert!(baseline.starts_with(&coeffs::to_json(&file)), "the v1 file leads");
    assert_eq!(bytes(&snapshots), baseline, "deterministic");

    // Positive control: the same merge an hour later, still before T − 120 s.
    let mut moved = Fleet::baseline();
    moved.replace(control(end() - Duration::hours(2)));
    assert_ne!(bytes(&moved.snapshots()), baseline, "a knowable change must move the file");
}

// -- perturbations knowable only at or after T − 120 s ------------------------

#[test]
fn label_events_at_or_after_t_minus_120s_leave_the_file_byte_identical() {
    let lag = end() - Duration::seconds(120);
    let mut fleet = Fleet::baseline();
    fleet.each(|h| {
        with_events(
            h,
            &[
                labeled("loom:blocked", secs(lag)),
                labeled(CHANGES_REQUESTED, secs(end())),
                unlabeled(APPROVED, secs(end() + Duration::hours(2))),
                labeled(REVIEW_REQUESTED, secs(end() + Duration::hours(3))),
            ],
        )
    });
    assert_ne!(fleet.snapshots(), Fleet::baseline().snapshots(), "the perturbation took");
    assert_eq!(bytes(&fleet.snapshots()), baseline_bytes());
}

#[test]
fn the_outcome_of_a_pr_open_across_t_does_not_move_the_file() {
    let baseline = baseline_bytes();
    let closed =
        PrHistory::new(9001, t(0), PrState::Closed, None, Vec::new(), across_events(), true)
            .with_closed_at(Some(end() + Duration::hours(1)));
    let still_open = open(9001, across_events());
    for (name, outcome) in [("closed at T + 1 h", closed), ("still open", still_open)] {
        let mut fleet = Fleet::baseline();
        fleet.replace(outcome);
        assert_eq!(bytes(&fleet.snapshots()), baseline, "{name}");
    }
}

/// A Judge rejection on #9001 (open across `T`) at `T + 30 min`, before its
/// `T + 3 h` merge: a post-cutoff `doctor` episode must not count toward
/// `rework` on its pre-cutoff rows (#10276 review).
#[test]
fn a_judge_rejection_after_t_does_not_count_toward_rework() {
    let rejected = end() + Duration::minutes(30);
    let mut fleet = Fleet::baseline();
    fleet.replace(with_events(
        &across(),
        &[
            unlabeled(APPROVED, secs(rejected)),
            labeled(CHANGES_REQUESTED, secs(rejected)),
        ],
    ));
    let doctor = |s: &[FleetSnapshot]| {
        s[0].episodes
            .iter()
            .filter(|e| e.pr_number == 9001 && e.stage == Stage::Doctor)
            .count()
    };
    let changed = fleet.snapshots();
    assert_eq!(
        doctor(&changed),
        doctor(&Fleet::baseline().snapshots()) + 1,
        "the perturbation took"
    );
    assert_eq!(bytes(&changed), baseline_bytes());
}

#[test]
fn prs_opened_after_t_do_not_move_the_file() {
    let mut fleet = Fleet::baseline();
    let mut landed_after = approve(end() + Duration::minutes(10), end() + Duration::hours(1));
    landed_after.push(PrEvent::Merged {
        at: end() + Duration::hours(2),
    });
    fleet.a.push(PrHistory::new(
        9100,
        end(),
        PrState::Merged,
        Some(end() + Duration::hours(2)),
        Vec::new(),
        landed_after,
        true,
    ));
    fleet
        .a
        .push(open(9101, vec![labeled(REVIEW_REQUESTED, secs(end()))]));
    // Opened 60 s before T: inside [T − 120 s, T), not knowable either.
    fleet.a.push(open(
        9102,
        vec![
            labeled(REVIEW_REQUESTED, secs(end()) - 60),
            labeled(STAR, secs(end()) - 60),
        ],
    ));
    fleet
        .b
        .push(open(9200, approve(end() + Duration::minutes(5), end() + Duration::hours(4))));
    assert_eq!(bytes(&fleet.snapshots()), baseline_bytes());
}

#[test]
fn a_flag_label_at_or_after_t_minus_120s_does_not_move_the_file() {
    let lag = end() - Duration::seconds(120);
    let mut fleet = Fleet::baseline();
    fleet.each(|h| {
        with_events(
            h,
            &[
                labeled(STAR, secs(lag)),
                labeled("loom:ci-failure", secs(lag) + 1),
                labeled("loom:sequenced", secs(end() + Duration::minutes(30))),
                labeled("loom:merge-conflict", secs(end() + Duration::hours(1))),
            ],
        )
    });
    let changed = fleet.snapshots();
    let base = Fleet::baseline().snapshots();
    assert_ne!(changed[0].flag_changes, base[0].flag_changes, "the perturbation took");
    assert_eq!(bytes(&changed), baseline_bytes());
}

#[test]
fn permuting_snapshots_episodes_and_flag_changes_does_not_move_the_file() {
    let mut snapshots = Fleet::baseline().snapshots();
    snapshots.reverse();
    for s in &mut snapshots {
        s.episodes.reverse();
        s.flag_changes.reverse();
        // An interleaving, not just a reversal.
        let n = s.episodes.len();
        s.episodes.rotate_left(n / 3);
        let m = s.flag_changes.len();
        s.flag_changes.rotate_left(m / 2);
    }
    assert_eq!(bytes(&snapshots), baseline_bytes());
}

// -- the runner: writes, retention, the daily trigger --------------------------

/// A small fleet under a temp root (fast: too few rows to fit any model).
fn small_root(snapshot_as_of: DateTime<Utc>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let prs = vec![
        control(end() - Duration::hours(3)),
        open(2, approve(end() - Duration::hours(30), end() - Duration::hours(25))),
    ];
    let s = snapshot(REPO, &prs, snapshot_as_of);
    fleet::write(&fleet::snapshot_path(dir.path(), REPO), &s).unwrap();
    dir
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn a_repeat_run_writes_the_same_bytes_and_a_dry_run_writes_nothing() {
    let root = small_root(as_of());
    let dry = run::fit_and_write(root.path(), end(), None, true, &fitter()).unwrap();
    assert!(!dry.written);
    assert!(!fit_dir(root.path()).exists(), "a dry run writes nothing");

    let first = run::fit_and_write(root.path(), end(), None, false, &fitter()).unwrap();
    let path = fit_dir(root.path()).join(path_for(end()));
    assert_eq!(first.path, path);
    assert_eq!(first.data_through, end() - Duration::seconds(120));
    assert_eq!(first.id, dry.id, "a dry run fits the same file");
    let bytes = std::fs::read(&path).unwrap();
    let second = run::fit_and_write(root.path(), end(), None, false, &fitter()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "byte-identical");
    assert_eq!(second.id, first.id);
    assert_eq!(coeffs::read(&path).unwrap().id, first.id);

    // The report is what `--json` prints.
    let json = serde_json::to_value(&first).unwrap();
    for key in [
        "id",
        "data_through",
        "stages",
        "dwells",
        "rows_dropped_missing",
        "rows_dropped_no_flags",
        "path",
    ] {
        assert!(json.get(key).is_some(), "{key}: {json}");
    }
    assert!(json["stages"].get("review_wait").is_some(), "{json}");
}

#[test]
fn no_snapshot_is_an_error_that_names_the_backfill() {
    let root = tempfile::tempdir().unwrap();
    let err = run::fit_and_write(root.path(), end(), None, false, &fitter()).unwrap_err();
    assert!(format!("{err:#}").contains("eta fleet backfill"), "{err:#}");
    assert!(run::refit_if_due(root.path(), end() + Duration::hours(12), &fitter()).is_none());
}

#[test]
fn writing_a_fifteenth_file_keeps_the_newest_fourteen() {
    let root = small_root(as_of());
    let dir = fit_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    let mut old: Vec<String> = (1..=15).map(|d| path_for(t(d * 86_400))).collect();
    for name in &old {
        std::fs::write(dir.join(name), "{}\n").unwrap();
    }
    std::fs::write(dir.join("notes.txt"), "kept\n").unwrap();

    // An explicit --out prunes nothing.
    let out = root.path().join("elsewhere.json");
    let report = run::fit_and_write(root.path(), end(), Some(&out), false, &fitter()).unwrap();
    assert_eq!(report.pruned, 0);
    assert!(out.exists());
    // The v2 file (#10508) goes beside an explicit --out.
    assert_eq!(report.v2_path, root.path().join("elsewhere.v2.json"));
    assert!(report.v2_path.exists());
    // So does the v3 file (#10521).
    assert_eq!(report.v3_path, root.path().join("elsewhere.v3.json"));
    assert!(report.v3_path.exists());
    assert_eq!(names(&dir).len(), 16);

    let report = run::fit_and_write(root.path(), end(), None, false, &fitter()).unwrap();
    assert_eq!(report.pruned, 2);
    old.drain(..2);
    old.push(path_for(end()));
    old.push("notes.txt".to_string());
    // The v2 files' own directory, which v1 retention never touches.
    old.push("v2".to_string());
    old.push("v3".to_string());
    old.sort();
    assert_eq!(names(&dir), old);
    assert_eq!(names(&dir.join("v2")), [path_for(end())]);
    assert_eq!(report.v2_path, dir.join("v2").join(path_for(end())));
    assert_eq!(names(&dir.join("v3")), [path_for(end())]);
    assert_eq!(report.v3_path, dir.join("v3").join(path_for(end())));
}

#[test]
fn the_daily_refit_fits_once_per_day() {
    // Fresh snapshots (as of after midnight): due at once, then not again.
    let root = small_root(as_of());
    let now = end() + Duration::hours(1);
    let report = run::refit_if_due(root.path(), now, &fitter())
        .expect("due")
        .unwrap();
    assert_eq!(report.as_of, end());
    assert!(fit_dir(root.path()).join(path_for(end())).exists());
    assert!(run::refit_if_due(root.path(), now + Duration::hours(1), &fitter()).is_none());

    // Stale snapshots wait out the grace period, then fit what is there.
    let stale = small_root(end() - Duration::hours(1));
    assert!(run::refit_if_due(stale.path(), end() + Duration::hours(5), &fitter()).is_none());
    let report = run::refit_if_due(stale.path(), end() + Duration::hours(6), &fitter())
        .expect("due after the grace")
        .unwrap();
    assert_eq!(report.data_through, end() - Duration::hours(1));
    assert_eq!(
        rows::data_horizon(&fleet::load_all(stale.path()), end()),
        end() - Duration::hours(1)
    );
}

// -- the PR-or-issue star (#10372) ---------------------------------------------

fn raw(
    item: u32,
    kind_item: ItemKind,
    kind: EventKind,
    label: Option<&str>,
    at: DateTime<Utc>,
) -> RawEvent {
    RawEvent::new(REPO, item, kind_item, kind, label.map(str::to_string), at, SOURCE_FORGE, 1, at)
}

fn link(pr: u32, issue: u32, at: DateTime<Utc>) -> RawEvent {
    raw(pr, ItemKind::Pr, EventKind::ClosingRef, Some("closes"), at).with_target(Some(issue))
}

/// The star scenario: #9001 (open across `T`) links issue 77, starred at
/// `T − 7 h`; #9002 is open across `T` too and, in the baseline, links nothing.
fn star_fleet() -> Fleet {
    let mut fleet = Fleet::baseline();
    fleet.a.push(open(9002, across_events()));
    fleet
}

fn star_events(extra: &[RawEvent]) -> Vec<RawEvent> {
    let mut events = vec![
        raw(77, ItemKind::Issue, EventKind::Opened, None, t(0)),
        link(9001, 77, end() - Duration::hours(9)),
        raw(
            77,
            ItemKind::Issue,
            EventKind::LabelAdded,
            Some(STAR),
            end() - Duration::hours(7),
        ),
    ];
    events.extend_from_slice(extra);
    events
}

/// The rows (every column, as `Debug`) with the star inputs read from `events`.
fn star_rows(events: &[RawEvent]) -> (String, rows::Assembled) {
    let mut inputs = StarInputs::default();
    inputs
        .repos
        .insert(REPO.to_string(), RepoStar::from_events(events));
    let a = rows::build_with_star(&star_fleet().snapshots(), end(), Some(&inputs));
    (format!("{:?}", a.rows), a)
}

#[test]
fn the_star_baseline_is_not_vacuous_and_the_file_is_unchanged() {
    let (_, a) = star_rows(&star_events(&[]));
    let starred = |pr: u32| {
        a.rows
            .iter()
            .zip(&a.row_keys)
            .filter(|(_, k)| k.pr == pr && k.repo == REPO)
            .filter_map(|(r, _)| r.starred_any)
            .collect::<Vec<_>>()
    };
    assert!(starred(9001).iter().any(|s| *s), "the issue star reaches 9001's rows");
    assert!(starred(9002).iter().all(|s| !*s) && !starred(9002).is_empty());
    // Recording the star moves no coefficient.
    let snapshots = star_fleet().snapshots();
    let mut inputs = StarInputs::default();
    inputs
        .repos
        .insert(REPO.to_string(), RepoStar::from_events(&star_events(&[])));
    let with = run::fit_snapshots_with_star(&snapshots, end(), &fitter(), Some(&inputs)).0;
    assert!(bytes(&snapshots).starts_with(&coeffs::to_json(&with)));
}

#[test]
fn an_issue_starred_at_t_minus_lag_plus_1s_leaves_every_row_byte_identical() {
    // #9002 links issue 78 at `T − 9 h`; 78 is NOT starred in this baseline, so
    // a leaked late star would flip 9002's rows (a star on the already-starred
    // 77 would be a no-op and could not detect the leak).
    let linked = link(9002, 78, end() - Duration::hours(9));
    let star78 = |at| raw(78, ItemKind::Issue, EventKind::LabelAdded, Some(STAR), at);
    let starred_9002 = |a: &rows::Assembled| {
        a.rows
            .iter()
            .zip(&a.row_keys)
            .any(|(r, k)| k.pr == 9002 && r.starred_any == Some(true))
    };
    let (baseline, a) = star_rows(&star_events(std::slice::from_ref(&linked)));
    assert!(!starred_9002(&a), "78 is unstarred in the baseline");
    let late = end() - Duration::seconds(119);
    let (rows, _) = star_rows(&star_events(&[linked.clone(), star78(late)]));
    assert_eq!(rows, baseline);
    // Positive control: the same star on the same linked issue, an hour
    // earlier, changes the later rows.
    let (rows, a) = star_rows(&star_events(&[linked, star78(end() - Duration::hours(1))]));
    assert_ne!(rows, baseline);
    assert!(starred_9002(&a));
}

#[test]
fn a_link_from_a_pr_created_at_t_minus_lag_plus_1s_leaves_every_row_byte_identical() {
    let (baseline, _) = star_rows(&star_events(&[]));
    let late = end() - Duration::seconds(119);
    let (rows, _) = star_rows(&star_events(&[link(9002, 77, late)]));
    assert_eq!(rows, baseline);
}

#[test]
fn the_same_link_an_hour_earlier_changes_the_later_rows() {
    let (baseline, _) = star_rows(&star_events(&[]));
    let (rows, a) = star_rows(&star_events(&[link(9002, 77, end() - Duration::hours(1))]));
    assert_ne!(rows, baseline, "positive control");
    assert!(a
        .rows
        .iter()
        .zip(&a.row_keys)
        .any(|(r, k)| k.pr == 9002 && r.starred_any == Some(true)));
}
