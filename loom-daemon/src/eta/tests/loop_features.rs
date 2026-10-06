//! Friction predictors (#10521): the stage-age form, review-loop history,
//! the repo Judge rejection rate, file overlap and own CI; point-in-time
//! discipline (the leak test, file lists as known at `as_of`); and
//! fit/serve parity.

use super::fit_rows::{approve, cutoff, h, landed, open, secs, snapshot, REPO};
use super::provenance;
use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::fit::rows;
use crate::eta::fit::FEATURES;
use crate::eta::loop_features::{
    loop_features, loop_vector, repo_context, CiObservation, FileSnapshot, LoopFeatures,
    LoopInputs, LOOP_FEATURES,
};
use crate::eta::tracker::Tracker;
use crate::eta::Stage;
use crate::pr_latency::history::fixtures::labeled;
use crate::pr_latency::{APPROVED, REVIEW_REQUESTED};
use chrono::{DateTime, Duration, Utc};

fn at(x: f64) -> DateTime<Utc> {
    h(x)
}

fn left(pr: u32, stage: Stage, from: f64, to: f64, next: Stage) -> StageEpisode {
    StageEpisode {
        repo: REPO.to_string(),
        pr_number: pr,
        stage,
        entered_at: at(from),
        end: EpisodeEnd::Left {
            at: at(to),
            next: EpisodeNext::Stage(next),
        },
    }
}

fn running(pr: u32, stage: Stage, from: f64) -> StageEpisode {
    StageEpisode {
        repo: REPO.to_string(),
        pr_number: pr,
        stage,
        entered_at: at(from),
        end: EpisodeEnd::Open { at: at(1000.0) },
    }
}

fn compute(
    eps: &[StageEpisode],
    pr: u32,
    files: Option<&[FileSnapshot]>,
    ci: Option<&[CiObservation]>,
    t: f64,
) -> LoopFeatures {
    let refs: Vec<&StageEpisode> = eps.iter().collect();
    loop_features(
        &LoopInputs {
            repo: REPO,
            pr,
            own: &refs,
            repo_episodes: &refs,
            files,
            ci,
        },
        at(t),
    )
}

fn snap(pr: u32, known: f64, files: &[&str]) -> FileSnapshot {
    FileSnapshot {
        repo: REPO.to_string(),
        pr,
        known_at: at(known),
        files: files.iter().map(|f| (*f).to_string()).collect(),
    }
}

/// A PR on its 3rd approval: review, merge_wait, stale re-review, etc.
fn looped() -> Vec<StageEpisode> {
    vec![
        left(1, Stage::ReviewWait, 0.0, 2.0, Stage::MergeWait),
        left(1, Stage::MergeWait, 2.0, 30.0, Stage::ReviewWait),
        left(1, Stage::ReviewWait, 30.0, 31.0, Stage::MergeWait),
        left(1, Stage::MergeWait, 31.0, 60.0, Stage::ReviewWait),
        left(1, Stage::ReviewWait, 60.0, 61.0, Stage::MergeWait),
        running(1, Stage::MergeWait, 61.0),
    ]
}

#[test]
fn cumulative_stage_time_spans_loops_while_the_visit_age_restarts() {
    let f = compute(&looped(), 1, None, None, 61.5);
    // merge_wait: 28 h + 29 h + 0.5 h across three visits.
    assert!((f.cum_stage_h.unwrap() - 57.5).abs() < 1e-9);
    assert!(f.stage_looped);
    assert_eq!(f.review_requests, 3);
    assert_eq!(f.approvals_lost, 2);
    // A first-visit PR: cumulative == visit age, not looped.
    let first = compute(&[running(2, Stage::ReviewWait, 10.0)], 2, None, None, 12.0);
    assert!((first.cum_stage_h.unwrap() - 2.0).abs() < 1e-9);
    assert!(!first.stage_looped);
}

#[test]
fn nothing_after_as_of_is_counted() {
    // At 30.5 h the second approval has not happened and the first loss
    // (left merge_wait at 30) has.
    let f = compute(&looped(), 1, None, None, 30.5);
    assert_eq!(f.approvals_lost, 1);
    assert_eq!(f.review_requests, 2);
    assert!((f.cum_stage_h.unwrap() - 2.5).abs() < 1e-9, "review_wait: 2 h + 0.5 h");
    // An episode ending exactly at as_of is not ended before it.
    let g = compute(&looped(), 1, None, None, 30.0);
    assert_eq!(g.approvals_lost, 0);
}

#[test]
fn judge_rejection_rate_is_the_trailing_week_and_needs_enough_verdicts() {
    let mut eps = Vec::new();
    // 4 rejects and 4 approvals ending at 11..20 h, plus an old reject
    // ending at 0.5 h.
    for i in 0..4 {
        let s = 10.0 + f64::from(i) * 3.0;
        eps.push(left(10 + i, Stage::ReviewWait, s, s + 1.0, Stage::Doctor));
        eps.push(left(20 + i, Stage::ReviewWait, s, s + 1.0, Stage::MergeWait));
    }
    eps.push(left(30, Stage::ReviewWait, 0.0, 0.5, Stage::Doctor));
    // At 170 h the window opens at 2 h: the old reject has aged out.
    let f = compute(&eps, 10, None, None, 170.0);
    assert_eq!(f.judge_reject_rate_7d, Some(0.5));
    // While still inside the 7 d window (opening exactly at 0.5 h included),
    // the old one counts.
    for t in [100.0, 168.5] {
        let early = compute(&eps, 10, None, None, t);
        assert_eq!(early.judge_reject_rate_7d, Some(5.0 / 9.0), "at {t} h");
    }
    // Few verdicts: unknown, not a noisy ratio.
    let few = compute(&eps[..4], 10, None, None, 100.0);
    assert_eq!(few.judge_reject_rate_7d, None);
    // Verdicts after as_of are invisible.
    let before = compute(&eps, 10, None, None, 12.5);
    assert_eq!(before.judge_reject_rate_7d, None);
}

fn overlap_fleet() -> Vec<StageEpisode> {
    vec![
        running(1, Stage::ReviewWait, 0.0),
        running(2, Stage::ReviewWait, 0.0),
        running(3, Stage::ReviewWait, 0.0),
        left(4, Stage::ReviewWait, 0.0, 5.0, Stage::MergeWait), // not open later
        running(4, Stage::MergeWait, 5.0),
    ]
}

#[test]
fn file_overlap_counts_open_prs_sharing_a_path() {
    let files = [
        snap(1, 1.0, &["a.rs", "b.rs"]),
        snap(2, 1.0, &["b.rs", "c.rs"]),
        snap(3, 1.0, &["z.rs"]),
        snap(4, 1.0, &["a.rs", "b.rs"]),
    ];
    let f = compute(&overlap_fleet(), 1, Some(&files), None, 10.0);
    assert_eq!(f.overlap_prs, Some(2), "#2 shares b.rs, #4 shares a.rs and b.rs");
    assert_eq!(f.overlap_files, Some(2));
    // Subject with no list at as_of, or no file source at all: None.
    assert_eq!(compute(&overlap_fleet(), 1, Some(&files), None, 0.5).overlap_prs, None);
    assert_eq!(compute(&overlap_fleet(), 1, None, None, 10.0).overlap_prs, None);
}

#[test]
fn file_lists_are_the_ones_known_at_as_of_not_the_final_diff() {
    // #2 grows its diff to include a.rs only at 20 h; #1's own list changes
    // at 25 h. At 10 h neither change is knowable.
    let files = [
        snap(1, 1.0, &["a.rs"]),
        snap(1, 25.0, &["q.rs"]),
        snap(2, 1.0, &["c.rs"]),
        snap(2, 20.0, &["a.rs", "c.rs"]),
        snap(3, 1.0, &["z.rs"]),
        snap(4, 1.0, &["y.rs"]),
    ];
    let eps = overlap_fleet();
    assert_eq!(compute(&eps, 1, Some(&files), None, 10.0).overlap_prs, Some(0));
    assert_eq!(compute(&eps, 1, Some(&files), None, 21.0).overlap_prs, Some(1));
    assert_eq!(compute(&eps, 1, Some(&files), None, 26.0).overlap_prs, Some(0));
    // A snapshot read exactly at as_of is not yet knowable.
    assert_eq!(compute(&eps, 1, Some(&files), None, 20.0).overlap_prs, Some(0));
    // Every perturbation at or after as_of leaves the answer unchanged.
    let mut perturbed = files.to_vec();
    perturbed.push(snap(3, 10.0, &["a.rs"]));
    perturbed.push(snap(3, 50.0, &["a.rs"]));
    assert_eq!(
        compute(&eps, 1, Some(&perturbed), None, 10.0),
        compute(&eps, 1, Some(&files), None, 10.0)
    );
}

#[test]
fn an_open_peer_without_a_list_leaves_overlap_unknown() {
    // #1, #2, #4 have lists; open #3 has none at all. A partly observed
    // roster is unknown, never a confident zero.
    let files = [
        snap(1, 1.0, &["a.rs"]),
        snap(2, 1.0, &["c.rs"]),
        snap(4, 1.0, &["y.rs"]),
    ];
    let f = compute(&overlap_fleet(), 1, Some(&files), None, 10.0);
    assert_eq!((f.overlap_prs, f.overlap_files), (None, None));
    assert_eq!(loop_vector(&f)[7], 0.0, "overlap_known = 0");
    // Even when a known peer does overlap, the count stays unknown.
    let overlapping = [
        snap(1, 1.0, &["a.rs"]),
        snap(2, 1.0, &["a.rs"]),
        snap(4, 1.0, &["y.rs"]),
    ];
    let f = compute(&overlap_fleet(), 1, Some(&overlapping), None, 10.0);
    assert_eq!((f.overlap_prs, f.overlap_files), (None, None));
}

#[test]
fn a_peer_list_first_seen_at_or_after_as_of_leaves_overlap_unknown() {
    // #3's only list is read at 10 h: unknown at as_of = 10 h (boundary)
    // and before, known strictly after.
    let files = [
        snap(1, 1.0, &["a.rs"]),
        snap(2, 1.0, &["c.rs"]),
        snap(3, 10.0, &["z.rs"]),
        snap(4, 1.0, &["y.rs"]),
    ];
    let eps = overlap_fleet();
    for as_of in [5.0, 10.0] {
        let f = compute(&eps, 1, Some(&files), None, as_of);
        assert_eq!((f.overlap_prs, f.overlap_files), (None, None), "as_of {as_of}");
        assert_eq!(loop_vector(&f)[7], 0.0);
    }
    let f = compute(&eps, 1, Some(&files), None, 11.0);
    assert_eq!((f.overlap_prs, f.overlap_files), (Some(0), Some(0)));
    assert_eq!(loop_vector(&f)[7], 1.0, "fully observed zero is known");
}

#[test]
fn own_ci_is_the_last_run_before_as_of() {
    let ci = [
        CiObservation {
            pr: 1,
            at: at(5.0),
            failed: true,
        },
        CiObservation {
            pr: 1,
            at: at(8.0),
            failed: false,
        },
        CiObservation {
            pr: 2,
            at: at(9.0),
            failed: true,
        },
    ];
    let eps = looped();
    assert_eq!(compute(&eps, 1, None, Some(&ci), 6.0).own_ci_failed, Some(true));
    assert_eq!(compute(&eps, 1, None, Some(&ci), 9.0).own_ci_failed, Some(false));
    assert_eq!(compute(&eps, 1, None, Some(&ci), 4.0).own_ci_failed, None);
    assert_eq!(
        compute(&eps, 1, None, Some(&ci), 8.0).own_ci_failed,
        Some(true),
        "at as_of is future"
    );
}

#[test]
fn vector_is_in_declared_order_and_marks_unknowns() {
    let f = LoopFeatures {
        cum_stage_h: Some(2.0_f64.exp() - 1.0),
        review_requests: 1,
        ..LoopFeatures::default()
    };
    let v = loop_vector(&f);
    assert_eq!(v.len(), LOOP_FEATURES.len());
    assert!((v[0] - 2.0).abs() < 1e-12);
    assert!((v[1] - 2.0_f64.ln()).abs() < 1e-12);
    // judge, overlap and ci unknown: values 0 and their indicators 0.
    assert_eq!((v[3], v[4], v[7], v[8], v[9]), (0.0, 0.0, 0.0, 0.0, 0.0));
    let known = LoopFeatures {
        overlap_prs: Some(0),
        own_ci_failed: Some(false),
        judge_reject_rate_7d: Some(0.25),
        ..LoopFeatures::default()
    };
    let k = loop_vector(&known);
    assert_eq!((k[3], k[4], k[7], k[9]), (0.25, 1.0, 1.0, 1.0));
}

#[test]
fn candidate_names_are_disjoint_from_the_fit_schema() {
    for name in LOOP_FEATURES {
        assert!(!FEATURES.contains(&name), "{name} must not be in eta-fit/v1");
    }
}

// -- fit rows ---------------------------------------------------------------

fn fleet() -> Vec<crate::eta::fleet::FleetSnapshot> {
    // #1: approved at 3 h, loses the approval at 6 h (re-review at 6 h),
    // approved again at 8 h. #90 landed earlier. #2: waiting for review.
    let mut ev = approve(h(1.0), h(3.0));
    ev.push(labeled(REVIEW_REQUESTED, secs(h(6.0))));
    ev.push(crate::pr_latency::history::fixtures::unlabeled(APPROVED, secs(h(6.0))));
    ev.push(crate::pr_latency::history::fixtures::unlabeled(REVIEW_REQUESTED, secs(h(8.0))));
    ev.push(labeled(APPROVED, secs(h(8.0))));
    let prs = vec![
        landed(90, h(1.0), h(2.0), h(3.0)),
        open(1, ev),
        open(2, vec![labeled(REVIEW_REQUESTED, secs(h(2.0)))]),
    ];
    vec![snapshot(REPO, &prs, cutoff() + Duration::hours(1))]
}

#[test]
fn fit_rows_carry_loop_features_equal_to_a_direct_call() {
    let snaps = fleet();
    let a = rows::build(&snaps, cutoff());
    assert_eq!(a.loops.len(), a.rows.len());
    let t = h(10.0);
    let i = a
        .row_keys
        .iter()
        .position(|k| k.at == t && k.pr == 1 && k.repo == REPO)
        .expect("row for #1");
    let got = &a.loops[i];
    assert_eq!(got.review_requests, 2);
    assert_eq!(got.approvals_lost, 1);
    assert!(got.stage_looped);
    assert!(got.overlap_prs.is_none() && got.own_ci_failed.is_none());

    // Serving: a tracker holding the same snapshots as its label timeline
    // (#10500), asked at the row instant, reads them at `t - lag` too.
    let mut tracker = Tracker::new(provenance());
    tracker.on_fleet_snapshots(&snaps, t);
    let served = tracker.loop_features_of(REPO, 1, t);
    assert_eq!(&served, got, "train and serve share one builder (#10500)");
    // ...and a repo spelled differently is the same repo.
    assert_eq!(&tracker.loop_features_of(&REPO.to_ascii_uppercase(), 1, t), got);
    // Every training row, every PR and instant, equals its served value.
    for (k, l) in a.row_keys.iter().zip(&a.loops) {
        assert_eq!(&tracker.loop_features_of(&k.repo, k.pr, k.at), l, "#{} at {}", k.pr, k.at);
    }

    let as_of = t - Duration::seconds(crate::eta::fit::KNOWABLE_LAG_SEC);
    let eps: Vec<StageEpisode> = snaps[0].episodes.clone();
    // The whole repo history and the per-instant context agree.
    let refs: Vec<&StageEpisode> = eps.iter().collect();
    let own: Vec<&StageEpisode> = eps.iter().filter(|e| e.pr_number == 1).collect();
    let context = repo_context(&refs, as_of);
    let inputs = |repo_episodes| LoopInputs {
        repo: REPO,
        pr: 1,
        own: &own,
        repo_episodes,
        files: None,
        ci: None,
    };
    assert_eq!(loop_features(&inputs(&refs), as_of), loop_features(&inputs(&context), as_of));
}

#[test]
fn a_future_label_event_changes_no_row_at_t() {
    let t = h(10.0);
    let base = rows::build(&fleet(), cutoff());
    let key = |a: &rows::Assembled| -> Vec<String> {
        a.row_keys
            .iter()
            .zip(&a.loops)
            .filter(|(k, _)| k.at <= t)
            .map(|(k, l)| format!("{:?}|{}|{}", k.at, k.pr, serde_json::to_string(l).unwrap()))
            .collect()
    };
    // A third review loop for #1 starting after t must not move any row <= t.
    let mut prs = vec![
        landed(90, h(1.0), h(2.0), h(3.0)),
        open(2, vec![labeled(REVIEW_REQUESTED, secs(h(2.0)))]),
    ];
    let mut ev = approve(h(1.0), h(3.0));
    ev.push(labeled(REVIEW_REQUESTED, secs(h(6.0))));
    ev.push(crate::pr_latency::history::fixtures::unlabeled(APPROVED, secs(h(6.0))));
    ev.push(crate::pr_latency::history::fixtures::unlabeled(REVIEW_REQUESTED, secs(h(8.0))));
    ev.push(labeled(APPROVED, secs(h(8.0))));
    ev.push(labeled(REVIEW_REQUESTED, secs(h(t_plus(1.0)))));
    ev.push(crate::pr_latency::history::fixtures::unlabeled(APPROVED, secs(h(t_plus(1.0)))));
    prs.push(open(1, ev));
    let perturbed = rows::build(&[snapshot(REPO, &prs, cutoff() + Duration::hours(1))], cutoff());
    assert_eq!(key(&base), key(&perturbed));
}

fn t_plus(x: f64) -> f64 {
    10.0 + x
}
