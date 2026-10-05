//! Priority-aware queue features (#10333): dispatch-order position, star
//! dynamics, levels, knowability, and fit/serving parity.

use super::fit_rows::{cutoff, h, landed, open, secs, snapshot, REPO, STAR};
use crate::eta::fit::rows::{self, Assembled};
use crate::eta::flag_timeline::FlagChange;
use crate::eta::labels::FLAG_STARRED;
use crate::eta::priority_features::{
    priority_features, PriorityEntry, PriorityFeatures, PriorityState,
};
use crate::eta::queue_features::QueueSubject;
use crate::eta::star::{IssueStarChange, RepoStar, StarInputs, StarLink};
use crate::eta::Stage;
use crate::operator_levels::OPERATOR_HIGH_PRIORITY_LABEL;
use crate::pr_latency::history::fixtures::labeled;
use crate::pr_latency::history::PrHistory;
use crate::pr_latency::REVIEW_REQUESTED;
use crate::work_finder::ready_queue::key_of;
use crate::work_finder::ready_queue::{ETA_IGNORED_KEYS, ETA_POSITION_KEYS};
use crate::work_finder::{candidate_cmp, PriorityCandidate, WorkItem};
use chrono::{DateTime, Duration, SecondsFormat, Utc};

fn now() -> DateTime<Utc> {
    h(100.0)
}

fn ago_h(x: i64) -> DateTime<Utc> {
    now() - Duration::hours(x)
}

fn scope() -> Vec<String> {
    vec![REPO.to_string(), "rjwalters/other".to_string()]
}

fn star(at_h_ago: i64) -> PriorityState {
    leveled(1, at_h_ago)
}

/// Unstarred until `at_h_ago`, then at `level`.
fn leveled(level: u8, at_h_ago: i64) -> PriorityState {
    PriorityState {
        initial_level: 0,
        changes: vec![(ago_h(at_h_ago), level)],
        ..PriorityState::default()
    }
}

fn entry(repo: &str, pr: u32, entered_h_ago: i64, star: PriorityState) -> PriorityEntry {
    PriorityEntry {
        repo: repo.to_string(),
        pr,
        stage: Some(Stage::ReviewWait),
        entered_at: ago_h(entered_h_ago),
        known_at: ago_h(entered_h_ago) + Duration::seconds(120),
        star,
    }
}

fn subject(pr: u32, entered_h_ago: i64) -> QueueSubject {
    QueueSubject {
        repo: REPO.to_string(),
        pr: Some(pr),
        current: Some((Stage::ReviewWait, ago_h(entered_h_ago))),
    }
}

fn compute(
    pr: u32,
    entered: i64,
    st: &PriorityState,
    roster: &[PriorityEntry],
) -> PriorityFeatures {
    priority_features(&subject(pr, entered), st, roster, &scope(), now())
}

#[test]
fn a_starred_pr_behind_ten_older_unstarred_is_at_the_front() {
    let mut roster: Vec<PriorityEntry> = (1..=10)
        .map(|i| entry(REPO, i, 50 + i64::from(i), PriorityState::default()))
        .collect();
    roster.push(entry(REPO, 99, 5, star(2)));
    let f = compute(99, 5, &star(2), &roster);
    assert_eq!(f.ahead_dispatch, Some(0), "FIFO `ahead` would say 10");
    assert_eq!(f.priority_level, 1);
    assert_eq!(f.n_starred_repo, Some(0));
}

#[test]
fn an_unstarred_pr_counts_a_newly_starred_one_that_passed_it() {
    let roster = vec![
        entry(REPO, 1, 40, PriorityState::default()),
        entry(REPO, 2, 3, star(1)),
    ];
    let f = compute(1, 40, &PriorityState::default(), &roster);
    assert_eq!(f.ahead_dispatch, Some(1));
    assert_eq!(f.ahead_starred, Some(1));
    assert_eq!(f.n_starred_repo, Some(1));
    assert_eq!(f.n_starred_fleet, Some(1));
    assert_eq!(f.starred_age_sec, None);
}

#[test]
fn starred_at_orders_among_starred_and_age_among_unstarred() {
    // 1 starred 10 h ago (entered late), 2 starred 5 h ago (entered early).
    let roster = vec![entry(REPO, 1, 6, star(10)), entry(REPO, 2, 60, star(5))];
    assert_eq!(compute(1, 6, &star(10), &roster).ahead_dispatch, Some(0));
    assert_eq!(compute(2, 60, &star(5), &roster).ahead_dispatch, Some(1));
    assert_eq!(compute(2, 60, &star(5), &roster).starred_age_sec, Some(5 * 3600));
}

/// The parity the issue asks for: sort the same items with the work finder's
/// own `candidate_cmp` and the ETA `ahead_dispatch` is the dispatch position.
/// The parity the issue asks for: the same items as the work finder's own
/// [`WorkItem`]s (labels, starred-at, `createdAt`), keyed by its `key_of` and
/// sorted by `candidate_cmp`; each one's ETA `ahead_dispatch` is its
/// position. Covers the level (2 before 1), the star bucket, starred-at
/// among starred, a starred item with an unknown starred-at (age fallback),
/// and age among unstarred.
#[test]
fn ahead_dispatch_equals_the_position_candidate_cmp_gives() {
    let stamp = |at: DateTime<Utc>| at.to_rfc3339_opts(SecondsFormat::Millis, true);
    let items: Vec<(u32, i64, PriorityState)> = vec![
        (1, 90, PriorityState::default()),
        (2, 80, star(4)),
        (3, 70, PriorityState::default()),
        (4, 20, star(9)),
        (5, 10, star(9)),
        (6, 95, PriorityState::default()),
        (7, 3, leveled(2, 1)),
        (8, 60, PriorityState::at_level(1)),
        (9, 2, leveled(2, 2)),
    ];
    let roster: Vec<PriorityEntry> = items
        .iter()
        .map(|(pr, e, s)| entry(REPO, *pr, *e, s.clone()))
        .collect();
    let cands: Vec<PriorityCandidate> = items
        .iter()
        .map(|(pr, e, s)| {
            let labels = match s.level() {
                0 => vec![],
                1 => vec![STAR.to_string()],
                _ => vec![OPERATOR_HIGH_PRIORITY_LABEL.to_string()],
            };
            let mut item = WorkItem::with_created_at(*pr, labels, Some(stamp(ago_h(*e))));
            item.operator_priority_at = s.starred_at().map(stamp);
            key_of(0, 100, &item, false)
        })
        .collect();
    let mut order: Vec<usize> = (0..cands.len()).collect();
    order.sort_by(|a, b| candidate_cmp(&cands[*a], &cands[*b]));
    let prs: Vec<u32> = order.iter().map(|i| items[*i].0).collect();
    assert_eq!(prs, vec![9, 7, 8, 4, 5, 2, 6, 1, 3], "dispatch order");
    for (pos, idx) in order.iter().enumerate() {
        let (pr, e, s) = &items[*idx];
        let f = compute(*pr, *e, s, &roster);
        assert_eq!(f.ahead_dispatch, Some(pos as u32), "PR {pr}");
    }
}

#[test]
fn the_eta_subset_names_real_keys_and_ignores_the_rest() {
    let names = crate::work_finder::ready_queue::ordering_names();
    for k in ETA_POSITION_KEYS.iter().chain(ETA_IGNORED_KEYS.iter()) {
        assert!(names.iter().any(|n| n == k), "{k}");
    }
    assert_eq!(ETA_POSITION_KEYS.len() + ETA_IGNORED_KEYS.len(), names.len());
}

#[test]
fn a_higher_level_changes_no_existing_column_and_an_absent_level_is_zero() {
    let roster = vec![entry(REPO, 1, 40, PriorityState::default())];
    let one = compute(7, 5, &star(2), &roster);
    let two = compute(7, 5, &leveled(2, 2), &roster);
    assert_eq!(two.priority_level, 2);
    assert_eq!(
        PriorityFeatures {
            priority_level: 1,
            ..two.clone()
        },
        one
    );
    assert_eq!(compute(7, 5, &PriorityState::default(), &roster).priority_level, 0);
}

#[test]
fn star_changed_in_stage_needs_a_change_since_stage_entry() {
    let mut s = star(2);
    assert_eq!(compute(7, 5, &s, &[]).star_changed_in_stage, Some(true));
    s.changes = vec![(ago_h(30), 1)];
    assert_eq!(compute(7, 5, &s, &[]).star_changed_in_stage, Some(false));
}

#[test]
fn a_star_at_or_after_as_of_is_not_applied() {
    let late = PriorityState {
        initial_level: 0,
        changes: vec![(now(), 1)],
        ..PriorityState::default()
    };
    let roster = vec![entry(REPO, 1, 40, PriorityState::default())];
    let f = compute(7, 5, &late, &roster);
    assert_eq!(f.priority_level, 0);
    assert_eq!(f.star_changed_in_stage, Some(false));
    // And a roster entry starred only after as_of does not count.
    let roster = vec![entry(REPO, 1, 40, late)];
    assert_eq!(compute(7, 5, &PriorityState::default(), &roster).n_starred_repo, Some(0));
}

#[test]
fn not_a_pr_stage_or_no_pr_leaves_everything_but_the_level_empty() {
    let mut s = subject(7, 5);
    s.pr = None;
    let f = priority_features(&s, &star(1), &[], &scope(), now());
    assert_eq!(
        f,
        PriorityFeatures {
            priority_level: 1,
            ..PriorityFeatures::default()
        }
    );
}

// -- fit rows ---------------------------------------------------------------

fn fleet(extra_star: &[(u32, i64)]) -> Vec<crate::eta::fleet::FleetSnapshot> {
    let mut prs: Vec<PrHistory> = vec![landed(90, h(1.0), h(2.0), h(3.0))];
    for (n, at) in [(1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0)] {
        let mut ev = vec![labeled(REVIEW_REQUESTED, secs(h(at)))];
        if n == 4 {
            ev.push(labeled(STAR, secs(h(5.0))));
        }
        for (pr, s) in extra_star {
            if *pr == n {
                ev.push(labeled(STAR, *s));
            }
        }
        prs.push(open(n, ev));
    }
    vec![snapshot(REPO, &prs, cutoff() + Duration::hours(1))]
}

fn at_row(a: &Assembled, pr: u32, at: DateTime<Utc>) -> &PriorityFeatures {
    let i = a
        .row_keys
        .iter()
        .position(|k| k.at == at && k.pr == pr && k.repo == REPO)
        .unwrap_or_else(|| panic!("row for #{pr}"));
    &a.priority[i]
}

#[test]
fn fit_rows_carry_priority_features_equal_to_a_direct_call() {
    let a = rows::build(&fleet(&[]), cutoff());
    assert_eq!(a.priority.len(), a.rows.len());
    let t = h(10.0);
    // PR 4 starred at 5 h: at the front despite entering last.
    let four = at_row(&a, 4, t);
    assert_eq!(four.ahead_dispatch, Some(0));
    assert_eq!(four.priority_level, 1);
    assert_eq!(four.starred_age_sec, Some(5 * 3600));
    assert_eq!(four.star_changed_in_stage, Some(true));
    let one = at_row(&a, 1, t);
    assert_eq!(one.ahead_dispatch, Some(1), "passed by the starred PR 4");
    assert_eq!(one.ahead_starred, Some(1));

    // Direct call with the same inputs.
    let lag = Duration::seconds(120);
    let flags = |star: Option<DateTime<Utc>>, pr| -> Vec<FlagChange> {
        let mut v = vec![FlagChange {
            pr_number: pr,
            at: h(4.0),
            flags: 0,
        }];
        if let Some(at) = star {
            v.push(FlagChange {
                pr_number: pr,
                at,
                flags: FLAG_STARRED,
            });
        }
        v
    };
    let roster: Vec<PriorityEntry> = (1..=4)
        .map(|pr| {
            let entered = h(f64::from(pr));
            let st = flags((pr == 4).then(|| h(5.0)), pr);
            PriorityEntry {
                repo: REPO.to_string(),
                pr,
                stage: Some(Stage::ReviewWait),
                entered_at: entered,
                known_at: entered + lag,
                star: PriorityState::from_flags(&st, t - lag).unwrap(),
            }
        })
        .collect();
    let subj = QueueSubject {
        repo: REPO.to_string(),
        pr: Some(4),
        current: Some((Stage::ReviewWait, h(4.0))),
    };
    let direct = priority_features(&subj, &roster[3].star, &roster, &[REPO.to_string()], t);
    assert_eq!(&direct, four);
}

/// Every row at or before `t` (training row, priority features and key),
/// serialized: what "byte-identical" compares.
fn rows_through(a: &Assembled, t: DateTime<Utc>) -> Vec<String> {
    a.row_keys
        .iter()
        .zip(&a.rows)
        .zip(&a.priority)
        .filter(|((k, _), _)| k.at <= t)
        .map(|((k, r), p)| {
            let k = (k.at, &k.repo, k.pr);
            serde_json::to_string(&(k, r, p)).expect("serializable row")
        })
        .collect()
}

#[test]
fn a_star_after_the_cutoff_t_changes_no_row_at_t() {
    let t = h(10.0);
    let base = rows::build(&fleet(&[]), cutoff());
    let before = rows_through(&base, t);
    assert!(!before.is_empty());
    // Starred inside the knowability lag (t - 60 s), at t, and after t.
    for late in [secs(t) - 60, secs(t), secs(t) + 3600] {
        let perturbed = rows::build(&fleet(&[(1, late)]), cutoff());
        assert_eq!(before, rows_through(&perturbed, t), "late={late}");
    }
    // Positive control: a star well before t does move PR 1's row.
    let moved = rows::build(&fleet(&[(1, secs(t) - 7200)]), cutoff());
    assert_ne!(at_row(&base, 1, t), at_row(&moved, 1, t));
}

/// A double star (#10307) on the flag timeline: level 2 sorts ahead of an
/// earlier plain star, while the twin-otter row sees only "starred", exactly
/// as with a plain star.
#[test]
fn a_level_two_row_leads_dispatch_and_leaves_the_model_inputs_alone() {
    use crate::eta::labels::{level_from_flags, pr_flags, FLAG_LEVEL_2};
    let high = vec![OPERATOR_HIGH_PRIORITY_LABEL.to_string()];
    assert_eq!(pr_flags(&high), FLAG_STARRED | FLAG_LEVEL_2);
    assert_eq!(level_from_flags(pr_flags(&high)), 2);
    assert_eq!(level_from_flags(pr_flags(&[STAR.to_string()])), 1);
    assert_eq!(level_from_flags(0), 0);

    let with = |label: &'static str| {
        let mut prs: Vec<PrHistory> = vec![landed(90, h(1.0), h(2.0), h(3.0))];
        for (n, at) in [(1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0)] {
            let mut ev = vec![labeled(REVIEW_REQUESTED, secs(h(at)))];
            if n == 4 {
                ev.push(labeled(STAR, secs(h(5.0))));
            }
            if n == 3 {
                ev.push(labeled(label, secs(h(6.0))));
            }
            prs.push(open(n, ev));
        }
        rows::build(&[snapshot(REPO, &prs, cutoff() + Duration::hours(1))], cutoff())
    };
    let t = h(10.0);
    let two = with(OPERATOR_HIGH_PRIORITY_LABEL);
    let one = with(STAR);
    let three = at_row(&two, 3, t);
    assert_eq!(three.priority_level, 2);
    assert_eq!(three.ahead_dispatch, Some(0), "level 2 before 4's earlier star");
    assert_eq!(at_row(&two, 4, t).ahead_dispatch, Some(1));
    assert_eq!(at_row(&one, 3, t).ahead_dispatch, Some(1), "plain: 4 starred first");
    assert_eq!(two.rows, one.rows, "twin-otter rows are level-blind");
}

#[test]
fn a_linked_issue_star_raises_the_level_and_dates_the_star() {
    let linked = PriorityState::default().with_linked(Some(ago_h(3)));
    assert_eq!((linked.level(), linked.starred_at()), (1, Some(ago_h(3))));
    // The earlier of the PR's own star and the issue's.
    assert_eq!(star(6).with_linked(Some(ago_h(3))).starred_at(), Some(ago_h(6)));
    // A double star keeps its level and its own instant.
    let high = leveled(2, 1).with_linked(Some(ago_h(3)));
    assert_eq!((high.level(), high.starred_at()), (2, Some(ago_h(1))));
    // Not knowable at or after `as_of`.
    let late = PriorityState::default().with_linked(Some(now()));
    assert_eq!(late.known(now()).level(), 0);
    let roster = vec![entry(REPO, 1, 40, PriorityState::default())];
    let f = compute(7, 5, &linked, &roster);
    assert_eq!(f.ahead_dispatch, Some(0));
    assert_eq!(f.star_changed_in_stage, Some(true));
    assert_eq!(f.starred_age_sec, Some(3 * 3600));
}

/// Star inputs linking PR 1 to issue 501, starred at `starred` (seconds).
fn issue_star(starred: Option<i64>) -> StarInputs {
    let mut repo = RepoStar {
        links_from: Some(h(0.0)),
        issue_events_from: Some(h(0.0)),
        ..RepoStar::default()
    };
    repo.links.insert(
        1,
        vec![StarLink {
            issue: 501,
            known_at: h(1.0),
        }],
    );
    if let Some(at) = starred {
        repo.issue_stars.push(IssueStarChange {
            issue: 501,
            at: h(0.0) + Duration::seconds(at),
            starred: true,
        });
    }
    let mut inputs = StarInputs::default();
    inputs.repos.insert(REPO.to_string(), repo);
    inputs
}

#[test]
fn a_linked_issue_star_after_the_cutoff_t_changes_no_row_at_t() {
    let t = h(10.0);
    let build = |starred| rows::build_with_star(&fleet(&[]), cutoff(), Some(&issue_star(starred)));
    let base = build(None);
    let before = rows_through(&base, t);
    for late in [secs(t) - 60, secs(t), secs(t) + 3600] {
        assert_eq!(before, rows_through(&build(Some(late)), t), "late={late}");
    }
    // Positive control: starred 2 h before t, PR 1 is starred (after PR 4's
    // own 5 h star) and passes PRs 2 and 3.
    let moved = build(Some(secs(t) - 7200));
    let one = at_row(&moved, 1, t);
    assert_eq!((one.priority_level, one.ahead_dispatch), (1, Some(1)));
    assert_eq!(at_row(&moved, 2, t).ahead_dispatch, Some(2));
}

#[test]
fn current_model_inputs_are_unchanged_by_stars() {
    // `ahead` stays FIFO: PR 4 is last in, so 3 ahead, starred or not.
    let a = rows::build(&fleet(&[]), cutoff());
    let i = a
        .row_keys
        .iter()
        .position(|k| k.at == h(10.0) && k.pr == 4)
        .unwrap();
    assert_eq!(a.rows[i].inputs.ahead, 3);
}

#[test]
fn the_candidate_features_are_not_in_the_fit_schema() {
    use crate::eta::fit::{FEATURES, SCHEMA};
    use crate::eta::priority_features::PRIORITY_FEATURES;
    assert_eq!(SCHEMA, "eta-fit/v1");
    for f in PRIORITY_FEATURES {
        assert!(!FEATURES.contains(&f), "{f} is a twin-otter input");
    }
    let names: Vec<String> = serde_json::to_value(PriorityFeatures::default())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let want: Vec<String> = PRIORITY_FEATURES.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(names, want);
}

#[test]
fn a_starred_pr_with_no_star_instant_orders_by_age_as_dispatch_does() {
    let unknown = PriorityState::at_level(1);
    // 1 starred at an unknown instant, entered 30 h ago; 2 starred 10 h ago.
    // Dispatch falls back to `createdAt` (30 h ago) for 1: it goes first.
    let roster = vec![
        entry(REPO, 1, 30, unknown.clone()),
        entry(REPO, 2, 5, star(10)),
    ];
    assert_eq!(compute(1, 30, &unknown, &roster).ahead_dispatch, Some(0));
    assert_eq!(compute(1, 30, &unknown, &roster).starred_age_sec, None);
    assert_eq!(compute(2, 5, &star(10), &roster).ahead_dispatch, Some(1));
}

#[test]
fn known_replays_a_star_removed_after_as_of() {
    // Starred 10 h ago, unstarred 1 h after `now`: starred at `now`.
    let s = PriorityState {
        initial_level: 0,
        changes: vec![(ago_h(10), 1), (now() + Duration::hours(1), 0)],
        ..PriorityState::default()
    };
    let k = s.known(now());
    assert_eq!(k.level(), 1);
    assert_eq!(k.starred_at(), Some(ago_h(10)));
    assert_eq!(k.changes, vec![(ago_h(10), 1)]);
}

#[test]
fn serving_observes_star_changes_at_its_passes() {
    let rr = vec![REVIEW_REQUESTED.to_string()];
    let starred = vec![REVIEW_REQUESTED.to_string(), STAR.to_string()];
    // First pass of the repo: an already-starred PR has no star instant.
    let first = PriorityState::observe(None, &starred, ago_h(10), false);
    assert_eq!((first.level(), first.starred_at()), (1, None));
    assert!(first.changes.is_empty());
    // A PR new to a listed repo, starred: starred about now.
    let fresh = PriorityState::observe(None, &starred, ago_h(10), true);
    assert_eq!(fresh.starred_at(), Some(ago_h(10)));
    // Starred, unstarred, starred again across passes.
    let a = PriorityState::observe(None, &rr, ago_h(9), true);
    let b = PriorityState::observe(Some(&a), &starred, ago_h(8), true);
    let c = PriorityState::observe(Some(&b), &rr, ago_h(7), true);
    let d = PriorityState::observe(Some(&c), &starred, ago_h(6), true);
    assert_eq!(b.starred_at(), Some(ago_h(8)));
    assert_eq!((c.level(), c.starred_at()), (0, None));
    assert_eq!(d.starred_at(), Some(ago_h(6)));
    assert_eq!(d.changes.len(), 3);
    // A double star on a starred PR is a level change: its instant becomes
    // the starred-at, as the work finder re-reads it on a level change.
    let high = vec![
        REVIEW_REQUESTED.to_string(),
        OPERATOR_HIGH_PRIORITY_LABEL.to_string(),
    ];
    let e = PriorityState::observe(Some(&d), &high, ago_h(5), true);
    assert_eq!((e.level(), e.starred_at()), (2, Some(ago_h(5))));
}

// -- train/serve parity (#10284-style) --------------------------------------

mod serving {
    use super::super::fit_rows::{cutoff, h, OTHER, REPO, STAR};
    use super::super::hold_parity::{serve_with, snapshots, Spec, AT};
    use crate::eta::fit::rows;
    use crate::eta::priority_features::PriorityFeatures;
    use crate::eta::star::{IssueStarChange, RepoStar, StarInputs, StarLink};
    use crate::pr_latency::{APPROVED, REVIEW_REQUESTED as RR};

    /// [`REPO`], all in `review_wait`:
    ///
    /// - 55: review 9 h, starred 11 h, **unstarred** 14 h;
    /// - 51: review 10 h;
    /// - 52: review 11 h, linking issue [`ISSUE`], which is **starred** at
    ///   16 h (the PR's own labels never are, #10372);
    /// - 53: review 12 h, **starred** 15 h (mid-stage);
    /// - 54: review **and starred** 13 h (opened starred).
    ///
    /// [`OTHER`]: 62 review 9 h; 61 review and starred 12 h.
    ///
    /// 50 ([`REPO`]) and 60 ([`OTHER`]) merge at 2 h, so every repo has a
    /// merge to count `since_merge_h` from and the rows are complete.
    fn specs() -> Vec<Spec> {
        let spec = |repo, pr, steps| Spec {
            repo,
            pr,
            steps,
            merged: None,
            touched: None,
        };
        let merged = |repo, pr| Spec {
            repo,
            pr,
            steps: &[(1.0, &[RR]), (1.5, &[APPROVED])],
            merged: Some(2.0),
            touched: None,
        };
        vec![
            merged(REPO, 50),
            merged(OTHER, 60),
            spec(REPO, 55, &[(9.0, &[RR]), (11.0, &[RR, STAR]), (14.0, &[RR])]),
            spec(REPO, 51, &[(10.0, &[RR])]),
            spec(REPO, 52, &[(11.0, &[RR])]),
            spec(REPO, 53, &[(12.0, &[RR]), (15.0, &[RR, STAR])]),
            spec(REPO, 54, &[(13.0, &[RR, STAR])]),
            spec(OTHER, 62, &[(9.0, &[RR])]),
            spec(OTHER, 61, &[(12.0, &[RR, STAR])]),
        ]
    }

    /// The issue PR 52 links.
    const ISSUE: u32 = 952;
    /// When [`ISSUE`] is starred, in hours.
    const ISSUE_STARRED: f64 = 16.0;

    /// Training's star inputs: the link known from PR 52's opening, the
    /// issue's star from its `labeled` event.
    fn star_inputs() -> StarInputs {
        let mut repo = RepoStar {
            links_from: Some(h(0.0)),
            issue_events_from: Some(h(0.0)),
            ..RepoStar::default()
        };
        repo.links.insert(
            52,
            vec![StarLink {
                issue: ISSUE,
                known_at: h(11.0),
            }],
        );
        repo.issue_stars.push(IssueStarChange {
            issue: ISSUE,
            at: h(ISSUE_STARRED),
            starred: true,
        });
        let mut inputs = StarInputs::default();
        inputs.repos.insert(REPO.to_string(), repo);
        inputs
    }

    fn want(
        ahead: u32,
        ahead_starred: u32,
        n_starred_repo: u32,
        starred_age_h: Option<i64>,
        changed: bool,
    ) -> PriorityFeatures {
        PriorityFeatures {
            ahead_dispatch: Some(ahead),
            ahead_starred: Some(ahead_starred),
            n_starred_repo: Some(n_starred_repo),
            n_starred_fleet: Some(n_starred_repo + 1),
            starred_age_sec: starred_age_h.map(|x| x * 3600),
            star_changed_in_stage: Some(changed),
            priority_level: u8::from(starred_age_h.is_some()),
        }
    }

    /// At 20 h, dispatch order in [`REPO`] is 54 (starred 13 h), 53 (starred
    /// 15 h), 52 (its issue starred 16 h), then by age 55, 51. FIFO would put
    /// 54 and 53 last.
    #[test]
    fn serving_priority_features_equal_the_training_row() {
        let specs = specs();
        let trained = rows::build_with_star(&snapshots(&specs), cutoff(), Some(&star_inputs()));
        // Serving sees the link from the first pass listing 52, and the
        // issue in the star listing from the pass at 16 h.
        let tracker = serve_with(&specs, &[ISSUE_STARRED], |tracker, at| {
            let links: Vec<(u32, Vec<u32>)> = if at >= 11.0 {
                vec![(52, vec![ISSUE])]
            } else {
                Vec::new()
            };
            let starred: Vec<u32> = if at >= ISSUE_STARRED {
                vec![ISSUE]
            } else {
                Vec::new()
            };
            tracker.on_star_context(REPO, &links, Some(&starred), h(at));
            tracker.on_star_context(OTHER, &[], Some(&[]), h(at));
        });
        let by_hand = [
            (54, want(0, 0, 2, Some(7), false)),
            (53, want(1, 1, 2, Some(5), true)),
            (52, want(2, 2, 2, Some(4), true)),
            (55, want(3, 3, 3, None, true)),
            (51, want(4, 3, 3, None, false)),
        ];
        for (pr, expected) in by_hand {
            let i = trained
                .row_keys
                .iter()
                .position(|k| k.at == h(AT) && k.pr == pr && k.repo == REPO)
                .unwrap_or_else(|| panic!("row for #{pr}"));
            assert_eq!(trained.priority[i], expected, "PR {pr} trained");
            let served = tracker
                .priority_features_of(REPO, pr, h(AT))
                .unwrap_or_else(|| panic!("PR {pr} not listed"));
            assert_eq!(served, trained.priority[i], "PR {pr}: serving differs from training");
        }
    }
}
