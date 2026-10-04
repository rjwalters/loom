//! Stage episodes (#10218): the deterministic replay, its exits, its cut, and
//! the fleet snapshot that carries it.

use crate::eta::episodes::{
    derive, episodes_from_pr_history, EpisodeEnd, EpisodeInput, EpisodeNext, LabelEvent, PrEnd,
    StageEpisode,
};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::history::SampleSource;
use crate::eta::Stage;
use crate::pr_latency::history::fixtures::{labeled, pushed, t, unlabeled};
use crate::pr_latency::history::{PrEvent, PrHistory, PrState};
use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use chrono::{DateTime, Utc};

const REPO: &str = "rjwalters/loom";
const OPERATOR: &str = "loom:operator";

fn merged(number: u32, merged_secs: i64, events: Vec<PrEvent>) -> PrHistory {
    let mut events = events;
    events.push(PrEvent::Merged { at: t(merged_secs) });
    PrHistory::new(number, t(0), PrState::Merged, Some(t(merged_secs)), Vec::new(), events, true)
}

fn open(number: u32, events: Vec<PrEvent>) -> PrHistory {
    PrHistory::new(number, t(0), PrState::Open, None, Vec::new(), events, true)
}

/// `(stage, entered, end)` of each episode, for compact expectations.
fn shape(episodes: &[StageEpisode]) -> Vec<(Stage, i64, EpisodeEnd)> {
    episodes
        .iter()
        .map(|e| (e.stage, (e.entered_at - t(0)).num_seconds(), e.end))
        .collect()
}

fn left(secs: i64, next: EpisodeNext) -> EpisodeEnd {
    EpisodeEnd::Left { at: t(secs), next }
}

fn to(stage: Stage) -> EpisodeNext {
    EpisodeNext::Stage(stage)
}

/// Approved at 200 (the request removed in the same edit), held 300–500,
/// merged at 900.
fn held_then_merged() -> PrHistory {
    merged(
        1,
        900,
        vec![
            labeled(REVIEW_REQUESTED, 100),
            unlabeled(REVIEW_REQUESTED, 200),
            labeled(APPROVED, 200),
            labeled(OPERATOR, 300),
            unlabeled(OPERATOR, 500),
        ],
    )
}

// -- every exit #10223's path_stats.next names --------------------------------

#[test]
fn a_held_pr_reads_merge_wait_merge_hold_merge_wait_merged() {
    let episodes = episodes_from_pr_history(&held_then_merged(), REPO, t(10_000));
    assert_eq!(
        shape(&episodes),
        vec![
            (Stage::ReviewWait, 100, left(200, to(Stage::MergeWait))),
            (Stage::MergeWait, 200, left(300, to(Stage::MergeHold))),
            (Stage::MergeHold, 300, left(500, to(Stage::MergeWait))),
            (Stage::MergeWait, 500, left(900, EpisodeNext::Merged)),
        ]
    );
    assert!(episodes.iter().all(|e| e.repo == REPO && e.pr_number == 1));
    assert!(episodes.iter().all(StageEpisode::completed));
    assert_eq!(episodes[2].duration_sec(), 200);
}

#[test]
fn merge_wait_and_merge_hold_both_exit_to_doctor() {
    // merge_wait → doctor: the approval swapped for a rejection.
    let rejected = merged(
        2,
        5000,
        vec![
            labeled(APPROVED, 200),
            unlabeled(APPROVED, 400),
            labeled(CHANGES_REQUESTED, 400),
            pushed(600),
            unlabeled(CHANGES_REQUESTED, 700),
            labeled(REVIEW_REQUESTED, 700),
            unlabeled(REVIEW_REQUESTED, 900),
            labeled(APPROVED, 900),
        ],
    );
    assert_eq!(
        shape(&episodes_from_pr_history(&rejected, REPO, t(10_000))),
        vec![
            (Stage::MergeWait, 200, left(400, to(Stage::Doctor))),
            // The push does not end `doctor`: labels alone decide episodes.
            (Stage::Doctor, 400, left(700, to(Stage::ReviewWait))),
            (Stage::ReviewWait, 700, left(900, to(Stage::MergeWait))),
            (Stage::MergeWait, 900, left(5000, EpisodeNext::Merged)),
        ]
    );

    // merge_hold → doctor: hold and approval removed in one edit.
    let held_rejected = open(
        3,
        vec![
            labeled(APPROVED, 200),
            labeled(OPERATOR, 300),
            unlabeled(APPROVED, 800),
            unlabeled(OPERATOR, 800),
            labeled(CHANGES_REQUESTED, 800),
        ],
    );
    assert_eq!(
        shape(&episodes_from_pr_history(&held_rejected, REPO, t(1000))),
        vec![
            (Stage::MergeWait, 200, left(300, to(Stage::MergeHold))),
            (Stage::MergeHold, 300, left(800, to(Stage::Doctor))),
            (Stage::Doctor, 800, EpisodeEnd::Open { at: t(1000) }),
        ]
    );
}

#[test]
fn a_hold_that_stops_resolving_ends_unstaged_and_closes_end_the_last_episode() {
    // Rejected while the operator hold stays on: an operator hold off an
    // approved PR is a refusal, so the hold ends unstaged.
    let h = open(
        4,
        vec![
            labeled(APPROVED, 200),
            labeled(OPERATOR, 300),
            unlabeled(APPROVED, 800),
            labeled(CHANGES_REQUESTED, 800),
        ],
    );
    assert_eq!(
        shape(&episodes_from_pr_history(&h, REPO, t(1000))),
        vec![
            (Stage::MergeWait, 200, left(300, to(Stage::MergeHold))),
            (Stage::MergeHold, 300, EpisodeEnd::Unstaged { at: t(800) }),
        ]
    );

    // Closed unmerged, with the list's `closedAt`.
    let closed = PrHistory::new(
        5,
        t(0),
        PrState::Closed,
        None,
        Vec::new(),
        vec![labeled(APPROVED, 200), labeled(OPERATOR, 300)],
        true,
    );
    let with_close = closed.clone().with_closed_at(Some(t(700)));
    let episodes = episodes_from_pr_history(&with_close, REPO, t(1000));
    assert_eq!(episodes[1].end, left(700, EpisodeNext::Closed));
    assert!(!episodes[1].completed(), "a close is a lower bound");
    // Without it: unstaged at the last label event (documented fallback).
    let episodes = episodes_from_pr_history(&closed, REPO, t(1000));
    assert_eq!(episodes[1].end, EpisodeEnd::Unstaged { at: t(300) });
}

// -- determinism --------------------------------------------------------------

fn event(secs: i64, seq: u64, label: &str, added: bool) -> LabelEvent {
    LabelEvent {
        at: t(secs),
        seq,
        label: label.to_string(),
        added,
    }
}

fn input(events: Vec<LabelEvent>) -> EpisodeInput {
    EpisodeInput {
        repo: REPO.to_string(),
        pr_number: 9,
        events,
        end: PrEnd::Open,
    }
}

#[test]
fn same_instant_events_resolve_once_and_seq_orders_only_the_same_label() {
    // Two labels in one edit, in either order: one transition, never a
    // zero-length or unstaged gap between them.
    let a = derive(
        &input(vec![
            event(100, 0, REVIEW_REQUESTED, true),
            event(200, 1, REVIEW_REQUESTED, false),
            event(200, 2, APPROVED, true),
        ]),
        t(1000),
    );
    let b = derive(
        &input(vec![
            event(100, 0, REVIEW_REQUESTED, true),
            event(200, 1, APPROVED, true),
            event(200, 2, REVIEW_REQUESTED, false),
        ]),
        t(1000),
    );
    assert_eq!(a, b);
    assert_eq!(
        shape(&a),
        vec![
            (Stage::ReviewWait, 100, left(200, to(Stage::MergeWait))),
            (Stage::MergeWait, 200, EpisodeEnd::Open { at: t(1000) }),
        ]
    );

    // The same label twice in one instant: `seq` decides. Removed-then-applied
    // leaves it on (a hold), applied-then-removed leaves it off (no hold).
    let base = || vec![event(100, 0, APPROVED, true), event(200, 1, OPERATOR, true)];
    let mut on = base();
    on.extend([
        event(300, 2, OPERATOR, false),
        event(300, 3, OPERATOR, true),
    ]);
    let mut off = base();
    off.extend([
        event(300, 2, OPERATOR, true),
        event(300, 3, OPERATOR, false),
    ]);
    assert_eq!(
        shape(&derive(&input(on), t(1000)))[1],
        (Stage::MergeHold, 200, EpisodeEnd::Open { at: t(1000) })
    );
    assert_eq!(
        shape(&derive(&input(off), t(1000)))[1],
        (Stage::MergeHold, 200, left(300, to(Stage::MergeWait)))
    );
}

#[test]
fn the_input_order_does_not_matter_and_a_reapplied_label_opens_nothing() {
    let events = vec![
        event(100, 0, APPROVED, true),
        event(150, 1, APPROVED, true),
        event(200, 2, OPERATOR, true),
        event(250, 3, OPERATOR, true),
        event(400, 4, OPERATOR, false),
    ];
    let forward = derive(&input(events.clone()), t(1000));
    let mut reversed = events;
    reversed.reverse();
    assert_eq!(derive(&input(reversed), t(1000)), forward);
    assert_eq!(
        shape(&forward),
        vec![
            (Stage::MergeWait, 100, left(200, to(Stage::MergeHold))),
            (Stage::MergeHold, 200, left(400, to(Stage::MergeWait))),
            (Stage::MergeWait, 400, EpisodeEnd::Open { at: t(1000) }),
        ]
    );
}

#[test]
fn episode_exits_serialize_as_names() {
    let episodes = episodes_from_pr_history(&held_then_merged(), REPO, t(10_000));
    let json = serde_json::to_value(&episodes).unwrap();
    assert_eq!(json[1]["end"]["kind"], "left");
    assert_eq!(json[1]["end"]["next"], "merge_hold");
    assert_eq!(json[3]["end"]["next"], "merged");
    let back: Vec<StageEpisode> = serde_json::from_value(json).unwrap();
    assert_eq!(back, episodes);
}

// -- the cut: leak-freedom ------------------------------------------------------

fn histories() -> Vec<PrHistory> {
    vec![
        held_then_merged(),
        merged(
            6,
            4000,
            vec![
                labeled(REVIEW_REQUESTED, 50),
                labeled(CHANGES_REQUESTED, 600),
                unlabeled(REVIEW_REQUESTED, 600),
                unlabeled(CHANGES_REQUESTED, 1200),
                labeled(REVIEW_REQUESTED, 1200),
                unlabeled(REVIEW_REQUESTED, 1500),
                labeled(APPROVED, 1500),
                labeled("loom:operator-only", 1600),
                labeled("loom:operator-mechanical", 1600),
                unlabeled("loom:operator-only", 3000),
                unlabeled("loom:operator-mechanical", 3000),
            ],
        ),
        open(
            7,
            vec![
                labeled(APPROVED, 300),
                labeled(OPERATOR, 900),
                labeled("loom:blocked", 2000),
            ],
        ),
        // Closed unmerged while in review: episodes, but no sample at all.
        PrHistory::new(
            8,
            t(0),
            PrState::Closed,
            None,
            Vec::new(),
            vec![labeled(REVIEW_REQUESTED, 100)],
            true,
        )
        .with_closed_at(Some(t(400))),
    ]
}

#[test]
fn a_derivation_cut_at_t_agrees_with_the_full_one_before_t() {
    let full_at = t(10_000);
    for h in histories() {
        let full = episodes_from_pr_history(&h, REPO, full_at);
        for cut_secs in [
            0, 50, 51, 200, 250, 300, 500, 600, 900, 1200, 1600, 2000, 3500, 4000,
        ] {
            let cut = t(cut_secs);
            let truncated = episodes_from_pr_history(&h, REPO, cut);
            // Every episode that ended before T is identical in both…
            for e in full
                .iter()
                .filter(|e| e.ended_at().is_some_and(|at| at < cut))
            {
                assert!(truncated.contains(e), "#{} cut {cut_secs}: {e:?}", h.number);
            }
            // …an episode open at T has the same entry in both…
            for e in truncated.iter().filter(|e| e.ended_at().is_none()) {
                assert!(
                    full.iter()
                        .any(|f| f.stage == e.stage && f.entered_at == e.entered_at),
                    "#{} cut {cut_secs}: {e:?}",
                    h.number
                );
            }
            // …and the stored record's own view at T is exactly the cut.
            let viewed: Vec<StageEpisode> = full.iter().filter_map(|e| e.view_at(cut)).collect();
            assert_eq!(viewed, truncated, "#{} cut {cut_secs}", h.number);
        }
    }
}

fn snapshot_at(at: DateTime<Utc>) -> FleetSnapshot {
    let mut snapshot = FleetSnapshot::empty(REPO);
    snapshot.merge(&histories(), at);
    snapshot
}

#[test]
fn a_snapshot_built_at_t_and_one_built_later_agree_on_everything_before_t() {
    let later = snapshot_at(t(10_000)).stage_samples();
    for cut_secs in [250, 600, 1000, 1600, 2500, 3500] {
        let cut = t(cut_secs);
        let at_t = snapshot_at(cut).stage_samples();
        for stage in [
            Stage::ReviewWait,
            Stage::Doctor,
            Stage::MergeWait,
            Stage::MergeHold,
        ] {
            assert_eq!(
                at_t.select_episodes(REPO, stage, cut),
                later.select_episodes(REPO, stage, cut),
                "{stage} at {cut_secs}"
            );
            let sources = [SampleSource::ForgeTimeline];
            assert_eq!(
                at_t.select_at(REPO, stage, cut, &sources, 1),
                later.select_at(REPO, stage, cut, &sources, 1),
                "{stage} samples at {cut_secs}"
            );
        }
    }
    // A running episode comes back censored at the cut, never with its exit.
    let viewed = later.select_episodes(REPO, Stage::MergeHold, t(400));
    assert_eq!(viewed.len(), 1);
    assert_eq!(viewed[0].end, EpisodeEnd::Open { at: t(400) });
}

// -- the snapshot -----------------------------------------------------------------

#[test]
fn a_snapshot_carries_episodes_and_merge_hold_samples_but_no_merge_hold_fleet_sample() {
    let snapshot = snapshot_at(t(10_000));
    assert!(snapshot.samples.iter().all(|s| s.stage != Stage::MergeHold));
    let counts = snapshot.episode_counts_by_stage();
    assert_eq!(counts["merge_hold"], (2, 1), "two released, one still held then blocked");
    let history = snapshot.stage_samples();
    assert_eq!(history.episodes, snapshot.episodes);
    let held: Vec<i64> = history
        .stages
        .iter()
        .filter(|s| s.stage == Stage::MergeHold)
        .map(|s| s.duration_sec)
        .collect();
    assert_eq!(held, vec![200, 1400]);
    // #7: held at 900, blocked at 2000 → a 1100 s lower bound.
    let censored: Vec<i64> = history
        .censored
        .iter()
        .filter(|s| s.stage == Stage::MergeHold)
        .map(|s| s.duration_sec)
        .collect();
    assert_eq!(censored, vec![1100]);
    // The pooled `merge_wait` sample still runs from the approval to the
    // merge, hold included.
    let pooled = snapshot
        .samples
        .iter()
        .find(|s| s.pr_number == 1 && s.stage == Stage::MergeWait)
        .unwrap();
    assert_eq!(pooled.duration_sec, 700);
}

#[test]
fn a_pre_episode_snapshot_file_still_parses_and_keeps_its_id() {
    let mut snapshot = snapshot_at(t(10_000));
    snapshot.episodes.clear();
    // #10245's flag timeline is cleared too: a pre-#10218 file had neither.
    snapshot.flag_changes.clear();
    // Re-seal without episodes: the id is the pre-#10218 formula over the
    // samples alone.
    snapshot.merge(&[], snapshot.as_of);
    let instant = crate::telemetry::trace::instant;
    let lines: Vec<String> = snapshot
        .samples
        .iter()
        .map(|s| {
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}",
                s.repo.to_ascii_lowercase(),
                s.stage.as_str(),
                s.pr_number,
                instant(s.entered_at),
                instant(s.observed_at),
                s.duration_sec,
                u8::from(s.censored),
                s.verdict.as_deref().unwrap_or(""),
                s.attempt.map(|a| a.to_string()).unwrap_or_default(),
            )
        })
        .collect();
    let at = instant(snapshot.as_of);
    let mut parts: Vec<&str> = vec!["loom.eta.fleet.snapshot", REPO, &at];
    parts.extend(lines.iter().map(String::as_str));
    assert_eq!(snapshot.snapshot_id, crate::telemetry::trace::derived_hex(&parts, 16));

    // No `episodes` key is written, and a file without one parses.
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains("\"episodes\""), "{json}");
    let parsed: FleetSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, snapshot);
    assert!(parsed.episodes.is_empty());

    // With episodes the id moves, and a re-read PR replaces its episodes.
    let with = snapshot_at(t(10_000));
    assert_ne!(with.snapshot_id, snapshot.snapshot_id);
    let mut reread = with.clone();
    reread.merge(&histories(), t(10_000));
    assert_eq!(reread, with, "idempotent per PR");
    assert!(with.samples.iter().all(|s| s.pr_number != 8));
    assert!(with.prs.contains(&8), "an episode-only PR is in the census");
}
