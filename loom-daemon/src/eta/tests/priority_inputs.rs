//! The `eta-fit/v2` priority inputs (#10508): the versioned contract, the
//! historic repo rank and its knowability, the shared builder's star and
//! dispatch-position rules, and train/serve parity through the one builder.

use super::fit_rows::{cutoff, h, REPO, STAR};
use super::history_a;
use super::hold_parity::{fitted, serve, snapshots, Spec, AT};
use crate::eta::fit::features_v2::{
    model_features_v2, ModelInputsV2, PriorityCoverage, PriorityInputs, FEATURES_V2, N_FEATURES_V2,
    SCHEMA_V2, STARRED_INDEX,
};
use crate::eta::fit::rows::{self, Assembled};
use crate::eta::fit::v2;
use crate::eta::fit::{model_features, ModelInputs, FEATURES, N_FEATURES, SCHEMA};
use crate::eta::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};
use crate::eta::priority_features::{PriorityEntry, PriorityState};
use crate::eta::priority_inputs::{priority_inputs, PriorityContext};
use crate::eta::queue_features::QueueSubject;
use crate::eta::repo_priority::{repo_rank, revision_at, FleetMember, KnowBasis, RosterRevision};
use crate::eta::star::{LinkedStar, RepoStar, StarInputs};
use crate::eta::tracker::{EstimateContext, Tracker};
use crate::eta::Stage;
use crate::fleet_store::roster;
use crate::operator_levels::{HIGH_PRIORITY_INHERITED_LABEL, OPERATOR_HIGH_PRIORITY_LABEL};
use crate::pr_latency::REVIEW_REQUESTED as RR;
use crate::work_finder::ready_queue::{key_of, ordering_names, ETA_FLEET_POSITION_KEYS};
use crate::work_finder::{candidate_cmp, PriorityCandidate, WorkItem};
use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use std::collections::BTreeMap;
use std::path::Path;

const A: &str = "acme/a";
const B: &str = "acme/b";

fn member(repo: &str, priority: u32) -> FleetMember {
    FleetMember {
        repo: Some(repo.to_string()),
        priority,
    }
}

fn revision(
    committed: DateTime<Utc>,
    observed: Option<DateTime<Utc>>,
    members: Vec<FleetMember>,
) -> RosterRevision {
    RosterRevision {
        committed_at: committed,
        observed_at: observed,
        members,
    }
}

// ---- The versioned contract ------------------------------------------------

#[test]
fn v1_is_unchanged_and_v2_is_a_separate_schema() {
    assert_eq!(SCHEMA, "eta-fit/v1");
    assert_ne!(SCHEMA_V2, SCHEMA);
    assert_eq!(N_FEATURES, 20);
    assert_eq!(N_FEATURES_V2, 26);
    assert_eq!(FEATURES[STARRED_INDEX], "starred");
    assert_eq!(FEATURES_V2[STARRED_INDEX], "starred_any");
    for i in 0..N_FEATURES {
        if i != STARRED_INDEX {
            assert_eq!(FEATURES_V2[i], FEATURES[i], "position {i} keeps its v1 meaning");
        }
    }
}

#[test]
fn v2_reuses_the_v1_transform_and_swaps_the_star() {
    let base = ModelInputs {
        age_h: 3.0,
        ahead: 2,
        starred: true,
        hour_utc: 7.5,
        rework: 1,
        ..ModelInputs::default()
    };
    let v1 = model_features(&base);
    let m = ModelInputsV2 {
        base,
        priority: PriorityInputs {
            starred_any: Some(false),
            priority_level: Some(0),
            repo_rank: Some(0.25),
            ahead_dispatch_fleet: Some(3),
        },
    };
    let v2 = model_features_v2(&m);
    for i in 0..N_FEATURES {
        if i != STARRED_INDEX {
            assert_eq!(v2[i].to_bits(), v1[i].to_bits(), "position {i}");
        }
    }
    // v2 reads `starred_any`, never the PR-only flag.
    assert_eq!(v2[STARRED_INDEX], 0.0);
    assert_eq!(&v2[N_FEATURES..], &[0.0, 0.0, 0.25, 0.0, 3f64.ln_1p(), 0.0]);
}

#[test]
fn unknown_priority_inputs_set_their_indicators() {
    let v2 = model_features_v2(&ModelInputsV2::default());
    assert_eq!(&v2[N_FEATURES..], &[1.0, 0.0, 0.0, 1.0, 0.0, 1.0]);
    let known = model_features_v2(&ModelInputsV2 {
        priority: PriorityInputs {
            starred_any: Some(true),
            priority_level: Some(2),
            repo_rank: Some(0.0),
            ahead_dispatch_fleet: Some(0),
        },
        ..ModelInputsV2::default()
    });
    // A known zero is not an unknown: the indicators tell them apart.
    assert_eq!(&known[N_FEATURES..], &[0.0, 2.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(known[STARRED_INDEX], 1.0);
}

#[test]
fn coverage_counts_known_inputs() {
    let rows = [
        PriorityInputs::default(),
        PriorityInputs {
            starred_any: Some(false),
            priority_level: Some(0),
            repo_rank: Some(1.0),
            ahead_dispatch_fleet: None,
        },
    ];
    let c = PriorityCoverage::of(&rows);
    assert_eq!(
        (c.rows, c.star_known, c.repo_rank_known, c.ahead_dispatch_fleet_known),
        (2, 1, 1, 0)
    );
}

// ---- Historic repo rank ----------------------------------------------------

#[test]
fn rank_follows_dispatch_direction_and_reverses_with_the_priorities() {
    let r = revision(h(0.0), None, vec![member(A, 10), member(B, 100)]);
    assert_eq!(repo_rank(&r, A), Some(0.0), "lower priority dispatches first");
    assert_eq!(repo_rank(&r, B), Some(1.0));
    let reversed = revision(h(0.0), None, vec![member(A, 100), member(B, 10)]);
    assert_eq!(repo_rank(&reversed, A), Some(1.0));
    assert_eq!(repo_rank(&reversed, B), Some(0.0));
}

#[test]
fn ties_take_the_mid_rank_over_the_membership_at_the_revision() {
    let r = revision(
        h(0.0),
        None,
        vec![
            member(A, 50),
            member(B, 50),
            member("acme/c", 10),
            member("acme/d", 200),
        ],
    );
    // c first (0), a and b tied at positions 1-2 (mid 1.5 / 3), d last.
    assert_eq!(repo_rank(&r, "acme/c"), Some(0.0));
    assert_eq!(repo_rank(&r, A), Some(0.5));
    assert_eq!(repo_rank(&r, "ACME/B"), Some(0.5), "case-insensitive");
    assert_eq!(repo_rank(&r, "acme/d"), Some(1.0));
    assert_eq!(repo_rank(&r, "acme/zz"), None, "not a member: unknown");
    let alone = revision(h(0.0), None, vec![member(A, 7)]);
    assert_eq!(repo_rank(&alone, A), Some(0.0));
    // A slugless member still counts in the denominator.
    let slugless = revision(
        h(0.0),
        None,
        vec![
            member(A, 1),
            FleetMember {
                repo: None,
                priority: 2,
            },
            member(B, 3),
        ],
    );
    assert_eq!(repo_rank(&slugless, B), Some(1.0));
    assert_eq!(repo_rank(&slugless, A), Some(0.0));
}

#[test]
fn a_revision_from_repos_yml_uses_the_desired_set_and_the_default_priority() {
    let text = "root: ~/GitHub\nrepos:\n  - name: a\n    remote: git@github.com:Acme/A.git\n    fleet: true\n    fleet_priority: 10\n  - name: b\n    remote: https://github.com/acme/b\n    fleet: true\n  - name: c\n    remote: git@github.com:acme/c.git\n    fleet: false\n  - name: d\n    remote: git@github.com:acme/d.git\n    firewall: true\n";
    let parsed = roster::parse(text, Path::new("/home/x")).expect("parse");
    let r = RosterRevision::from_roster(&parsed, h(1.0), None);
    assert_eq!(r.members, vec![member(A, 10), member(B, DEFAULT_WORKSPACE_PRIORITY)]);
    assert_eq!(r.basis(), KnowBasis::CommitDate);
    assert_eq!(r.priority_of("acme/c"), DEFAULT_WORKSPACE_PRIORITY, "non-member");
}

#[test]
fn a_future_roster_edit_is_not_read() {
    let history = [
        revision(h(0.0), None, vec![member(A, 100), member(B, 10)]),
        revision(h(10.0), None, vec![member(A, 10), member(B, 100)]),
    ];
    assert_eq!(revision_at(&history, h(10.0)).unwrap().committed_at, h(0.0), "strict");
    assert_eq!(revision_at(&history, h(10.5)).unwrap().committed_at, h(10.0));
}

#[test]
fn a_backdated_commit_counts_from_its_observation() {
    // Committed "at 1 h" but first observed at 30 h.
    let history = [
        revision(h(0.0), Some(h(0.0)), vec![member(A, 100), member(B, 10)]),
        revision(h(1.0), Some(h(30.0)), vec![member(A, 10), member(B, 100)]),
    ];
    let at20 = revision_at(&history, h(20.0)).unwrap();
    assert_eq!(repo_rank(at20, A), Some(1.0), "the backdated edit is not knowable yet");
    assert_eq!(at20.basis(), KnowBasis::Observed);
    let at31 = revision_at(&history, h(31.0)).unwrap();
    assert_eq!(repo_rank(at31, A), Some(0.0));
    // An observation before the commit date does not make it knowable early.
    let early = revision(h(5.0), Some(h(2.0)), vec![member(A, 1)]);
    assert_eq!(early.knowable_at(), h(5.0));
}

#[test]
fn unavailable_history_is_unknown_never_todays_file() {
    let history = [revision(h(50.0), None, vec![member(A, 10)])];
    assert!(revision_at(&history, h(20.0)).is_none());
    assert!(revision_at(&[], h(20.0)).is_none());
}

// ---- The shared builder ----------------------------------------------------

fn now() -> DateTime<Utc> {
    h(100.0)
}

fn ago(x: f64) -> DateTime<Utc> {
    now() - Duration::seconds((x * 3600.0) as i64)
}

fn entry(repo: &str, pr: u32, entered_ago: f64, star: PriorityState) -> PriorityEntry {
    PriorityEntry {
        repo: repo.to_string(),
        pr,
        stage: Some(Stage::ReviewWait),
        entered_at: ago(entered_ago),
        known_at: ago(entered_ago) + Duration::seconds(120),
        star,
    }
}

fn subject(repo: &str, pr: u32, entered_ago: f64) -> QueueSubject {
    QueueSubject {
        repo: repo.to_string(),
        pr: Some(pr),
        current: Some((Stage::ReviewWait, ago(entered_ago))),
    }
}

fn labels(l: &[&str]) -> PriorityState {
    let l: Vec<String> = l.iter().map(|s| (*s).to_string()).collect();
    PriorityState::from_labels(&l)
}

fn linked_from(at: DateTime<Utc>) -> LinkedStar {
    LinkedStar {
        since: Some(at),
        changes: vec![at],
    }
}

fn scope() -> Vec<String> {
    vec![A.to_string(), B.to_string()]
}

fn star_only(own: &PriorityState, linked: Option<&LinkedStar>) -> PriorityInputs {
    let ctx = PriorityContext {
        roster: &[],
        scope: &scope(),
        fleet_history: None,
    };
    priority_inputs(&subject(A, 1, 1.0), own, linked, &ctx, now())
}

/// One `star_sources_levels_and_unknowns` case: label, own state, linked star,
/// expected star flag, expected level.
type StarCase<'a> = (&'a str, PriorityState, Option<&'a LinkedStar>, Option<bool>, Option<u8>);

#[test]
fn star_sources_levels_and_unknowns() {
    let none = LinkedStar::default();
    let linked = linked_from(ago(2.0));
    let cases: Vec<StarCase<'_>> = vec![
        ("PR-only star", labels(&[STAR]), Some(&none), Some(true), Some(1)),
        ("linked-issue-only star", labels(&[]), Some(&linked), Some(true), Some(1)),
        ("both", labels(&[STAR]), Some(&linked), Some(true), Some(1)),
        (
            "level 2",
            labels(&[OPERATOR_HIGH_PRIORITY_LABEL]),
            Some(&none),
            Some(true),
            Some(2),
        ),
        (
            "level 2 + linked",
            labels(&[OPERATOR_HIGH_PRIORITY_LABEL]),
            Some(&linked),
            Some(true),
            Some(2),
        ),
        (
            "inherited level 2",
            labels(&[HIGH_PRIORITY_INHERITED_LABEL]),
            Some(&none),
            Some(true),
            Some(2),
        ),
        ("level 0", labels(&[]), Some(&none), Some(false), Some(0)),
        // Linked history unknown: an own star is still known, nothing else is.
        ("own star, linked unknown", labels(&[STAR]), None, Some(true), Some(1)),
        ("unstarred, linked unknown", labels(&[]), None, None, None),
    ];
    for (name, own, linked, any, level) in cases {
        let p = star_only(&own, linked);
        assert_eq!((p.starred_any, p.priority_level), (any, level), "{name}");
        assert_eq!(p.repo_rank, None, "{name}: no history");
        assert_eq!(p.ahead_dispatch_fleet, None, "{name}: no history");
    }
}

#[test]
fn a_linked_star_on_the_own_state_is_ignored() {
    // `own` must be the PR's labels alone; a linked star smuggled onto it does
    // not make an unknown linked history known.
    let own = labels(&[]).with_linked(linked_from(ago(2.0)));
    assert_eq!(star_only(&own, None).starred_any, None);
}

#[test]
fn a_star_at_or_after_as_of_is_not_applied() {
    let future = PriorityState {
        changes: vec![(now(), 1)],
        ..PriorityState::default()
    };
    let p = star_only(&future, Some(&LinkedStar::default()));
    assert_eq!(p.starred_any, Some(false));
    let p = star_only(&labels(&[]), Some(&linked_from(now())));
    assert_eq!(p.starred_any, Some(false), "a linked star at as_of is not known");
}

/// The fleet position must be the real dispatch comparator's (minus
/// `main_red_fix`), with each repo's historic `fleet_priority`.
#[test]
fn fleet_position_matches_cross_repo_dispatch_order() {
    let stamp = |at: DateTime<Utc>| at.to_rfc3339_opts(SecondsFormat::Millis, true);
    let starred_at = |x: f64| PriorityState {
        changes: vec![(ago(x), 1)],
        ..PriorityState::default()
    };
    // (repo, pr, entered hours ago, state)
    let items: Vec<(&str, u32, f64, PriorityState)> = vec![
        (A, 1, 90.0, PriorityState::default()),
        (B, 2, 10.0, PriorityState::default()),
        (A, 3, 5.0, starred_at(4.0)),
        (B, 4, 50.0, starred_at(4.0)),
        (B, 5, 80.0, PriorityState::default()),
        (A, 6, 80.0, PriorityState::default()),
        (A, 7, 3.0, labels(&[OPERATOR_HIGH_PRIORITY_LABEL])),
    ];
    for (prio_a, prio_b) in [(100, 10), (10, 100), (50, 50)] {
        let history = [revision(
            ago(200.0),
            None,
            vec![member(A, prio_a), member(B, prio_b)],
        )];
        let roster: Vec<PriorityEntry> = items
            .iter()
            .map(|(repo, pr, e, s)| entry(repo, *pr, *e, s.clone()))
            .collect();
        let cands: Vec<PriorityCandidate> = items
            .iter()
            .map(|(repo, pr, e, s)| {
                let labels = match s.level() {
                    0 => vec![],
                    1 => vec![STAR.to_string()],
                    _ => vec![OPERATOR_HIGH_PRIORITY_LABEL.to_string()],
                };
                let mut item = WorkItem::with_created_at(*pr, labels, Some(stamp(ago(*e))));
                item.operator_priority_at = s.starred_at().map(stamp);
                let prio = if *repo == A { prio_a } else { prio_b };
                key_of(0, prio, &item, false)
            })
            .collect();
        let mut order: Vec<usize> = (0..cands.len()).collect();
        order.sort_by(|a, b| candidate_cmp(&cands[*a], &cands[*b]));
        for (pos, idx) in order.iter().enumerate() {
            let (repo, pr, e, s) = &items[*idx];
            let ctx = PriorityContext {
                roster: &roster,
                scope: &scope(),
                fleet_history: Some(&history),
            };
            let p = priority_inputs(
                &subject(repo, *pr, *e),
                s,
                Some(&LinkedStar::default()),
                &ctx,
                now(),
            );
            assert_eq!(
                p.ahead_dispatch_fleet,
                Some(pos as u32),
                "PR {repo}#{pr}, priorities ({prio_a}, {prio_b})"
            );
        }
        // Reversed priorities reverse the unstarred repos' relative order.
        let first_unstarred = order.iter().find(|i| items[**i].3.level() == 0).unwrap();
        let expect = match prio_a.cmp(&prio_b) {
            std::cmp::Ordering::Less => A,
            std::cmp::Ordering::Greater => B,
            // Equal priority: age decides (A#1 entered 90 h ago).
            std::cmp::Ordering::Equal => A,
        };
        assert_eq!(items[*first_unstarred].0, expect, "({prio_a}, {prio_b})");
    }
}

#[test]
fn the_fleet_keys_are_every_real_key_but_the_red_main_fix() {
    let names = ordering_names();
    assert_eq!(ETA_FLEET_POSITION_KEYS.len() + 1, names.len());
    for k in ETA_FLEET_POSITION_KEYS {
        assert!(names.iter().any(|n| n == k), "{k}");
    }
    assert!(!ETA_FLEET_POSITION_KEYS.contains(&"main_red_fix"));
}

#[test]
fn position_is_unknown_without_history_star_or_scope() {
    let roster = vec![entry(B, 2, 10.0, PriorityState::default())];
    let history = [revision(
        ago(200.0),
        None,
        vec![member(A, 10), member(B, 10)],
    )];
    let run = |own: &PriorityState,
               linked: Option<&LinkedStar>,
               hist: Option<&[RosterRevision]>,
               scope: &[String]| {
        let ctx = PriorityContext {
            roster: &roster,
            scope,
            fleet_history: hist,
        };
        priority_inputs(&subject(A, 1, 1.0), own, linked, &ctx, now())
    };
    let known = run(&labels(&[]), Some(&LinkedStar::default()), Some(&history), &scope());
    assert_eq!(known.ahead_dispatch_fleet, Some(1));
    assert_eq!(known.repo_rank, Some(0.5));
    let no_hist = run(&labels(&[]), Some(&LinkedStar::default()), None, &scope());
    assert_eq!((no_hist.ahead_dispatch_fleet, no_hist.repo_rank), (None, None));
    let no_star = run(&labels(&[]), None, Some(&history), &scope());
    assert_eq!(no_star.ahead_dispatch_fleet, None);
    assert_eq!(no_star.repo_rank, Some(0.5), "the rank does not need the star");
    let out_of_scope = run(&labels(&[]), Some(&LinkedStar::default()), Some(&history), &[]);
    assert_eq!(out_of_scope.ahead_dispatch_fleet, None);
    // A roster edit knowable only within LAG of as_of is not read yet.
    let late = [revision(
        now() - Duration::seconds(60),
        None,
        vec![member(A, 10)],
    )];
    let p = run(&labels(&[]), Some(&LinkedStar::default()), Some(&late), &scope());
    assert_eq!(p.repo_rank, None);
}

// ---- Train/serve parity ----------------------------------------------------

const OTHER: &str = super::fit_rows::OTHER;

fn spec(repo: &'static str, pr: u32, steps: &'static [(f64, &'static [&'static str])]) -> Spec {
    Spec {
        repo,
        pr,
        steps,
        merged: None,
        touched: None,
    }
}

/// `REPO`: 51 issue-only star, 52 PR-only, 53 both, 54 star removed, 55
/// none, 56 level 2, 57 inherited level 2. `OTHER` (no star coverage): 61
/// unstarred, 62 PR-starred.
fn specs() -> Vec<Spec> {
    vec![
        spec(REPO, 51, &[(10.0, &[RR])]),
        spec(REPO, 52, &[(10.0, &[RR, STAR])]),
        spec(REPO, 53, &[(10.0, &[RR, STAR])]),
        spec(REPO, 54, &[(10.0, &[RR])]),
        spec(REPO, 55, &[(10.0, &[RR])]),
        spec(REPO, 56, &[(11.0, &[RR, OPERATOR_HIGH_PRIORITY_LABEL])]),
        spec(REPO, 57, &[(11.0, &[RR, HIGH_PRIORITY_INHERITED_LABEL])]),
        spec(OTHER, 61, &[(9.0, &[RR])]),
        spec(OTHER, 62, &[(10.0, &[RR, STAR])]),
        Spec {
            merged: Some(3.0),
            ..spec(REPO, 59, &[(1.0, &[RR]), (2.0, &[crate::pr_latency::APPROVED])])
        },
        Spec {
            merged: Some(3.0),
            ..spec(OTHER, 69, &[(1.0, &[RR]), (2.0, &[crate::pr_latency::APPROVED])])
        },
    ]
}

fn raw(
    item: u32,
    kind_item: ItemKind,
    kind: EventKind,
    label: Option<&str>,
    at: DateTime<Utc>,
) -> RawEvent {
    RawEvent::new(REPO, item, kind_item, kind, label.map(str::to_string), at, SOURCE_FORGE, 1, at)
}

/// `REPO`'s raw cache: each PR links issue `pr + 1000` from creation; 1051
/// and 1053 starred at 12 h, 1054 starred at 12 h and unstarred at 15 h.
fn events(specs: &[Spec]) -> Vec<RawEvent> {
    let mut out = Vec::new();
    for s in specs.iter().filter(|s| s.repo == REPO) {
        out.push(raw(s.issue(), ItemKind::Issue, EventKind::Opened, None, h(1.0)));
        out.push(
            raw(s.pr, ItemKind::Pr, EventKind::ClosingRef, Some("closes"), h(10.0))
                .with_target(Some(s.issue())),
        );
    }
    for issue in [1051, 1053, 1054] {
        out.push(raw(issue, ItemKind::Issue, EventKind::LabelAdded, Some(STAR), h(12.0)));
    }
    out.push(raw(1054, ItemKind::Issue, EventKind::LabelRemoved, Some(STAR), h(15.0)));
    out
}

/// The fleet roster's history: equal at first; at 5 h `OTHER` moves to the
/// front; a backdated edit (committed at 1 h, observed at 30 h, after
/// [`AT`]) that would put `REPO` first must not be read.
fn fleet_history() -> Vec<RosterRevision> {
    vec![
        revision(h(0.0), Some(h(0.0)), vec![member(REPO, 100), member(OTHER, 100)]),
        revision(h(5.0), Some(h(5.0)), vec![member(REPO, 100), member(OTHER, 10)]),
        revision(h(1.0), Some(h(30.0)), vec![member(REPO, 0), member(OTHER, 100)]),
    ]
}

fn trained(specs: &[Spec], history: Option<&[RosterRevision]>) -> Assembled {
    let mut inputs = StarInputs::default();
    inputs
        .repos
        .insert(REPO.to_string(), RepoStar::from_events(&events(specs)));
    rows::build_with_context(&snapshots(specs), cutoff(), Some(&inputs), history)
}

fn trained_at(a: &Assembled, repo: &str, pr: u32) -> PriorityInputs {
    let i = a
        .row_keys
        .iter()
        .position(|k| k.at == h(AT) && k.repo.eq_ignore_ascii_case(repo) && k.pr == pr)
        .unwrap_or_else(|| panic!("no row for {repo}#{pr}"));
    a.priority_inputs[i]
}

fn served_tracker(specs: &[Spec]) -> Tracker {
    let mut tracker = serve(specs);
    let links: Vec<(u32, Vec<u32>)> = specs
        .iter()
        .filter(|s| s.repo == REPO)
        .map(|s| (s.pr, vec![s.issue()]))
        .collect();
    tracker.on_star_context(REPO, &links, Some(&[]), h(10.5));
    tracker.on_star_context(REPO, &links, Some(&[1051, 1053, 1054]), h(12.5));
    tracker.on_star_context(REPO, &links, Some(&[1051, 1053]), h(15.5));
    tracker.on_star_context(REPO, &links, Some(&[1051, 1053]), h(19.5));
    // Exercise the estimate path once, as the twin-otter parity does.
    let registry = fitted();
    let history = history_a();
    let repo_ids = BTreeMap::new();
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
        stalls: &super::NO_STALLS,
    };
    let _ = tracker.estimate(None, &ctx, h(AT));
    tracker
}

#[test]
fn serving_builds_the_priority_inputs_training_built() {
    let specs = specs();
    let history = fleet_history();
    let a = trained(&specs, Some(&history));
    let tracker = served_tracker(&specs);
    let prs: [(&str, u32); 9] = [
        (REPO, 51),
        (REPO, 52),
        (REPO, 53),
        (REPO, 54),
        (REPO, 55),
        (REPO, 56),
        (REPO, 57),
        (OTHER, 61),
        (OTHER, 62),
    ];
    for (repo, pr) in prs {
        let training = trained_at(&a, repo, pr);
        let serving = tracker
            .priority_inputs_of(repo, pr, h(AT), Some(&history))
            .unwrap_or_else(|| panic!("{repo}#{pr} not listed"));
        assert_eq!(serving, training, "{repo}#{pr}: serving differs from training");
    }
    // The fixture's expectations, so the parity is not vacuous.
    let at = |repo, pr| trained_at(&a, repo, pr);
    assert_eq!(at(REPO, 51).starred_any, Some(true), "issue-only star");
    assert_eq!(at(REPO, 54).starred_any, Some(false), "star removed before t");
    assert_eq!(at(REPO, 56).priority_level, Some(2));
    assert_eq!(at(REPO, 57).priority_level, Some(2), "inherited level");
    assert_eq!(at(OTHER, 61).starred_any, None, "no star coverage: unknown");
    assert_eq!(at(OTHER, 61).ahead_dispatch_fleet, None);
    assert_eq!(at(OTHER, 62).starred_any, Some(true), "own star needs no coverage");
    // OTHER dispatches first at AT (the 5 h revision); the backdated edit is
    // not read.
    assert_eq!(at(OTHER, 61).repo_rank, Some(0.0));
    assert_eq!(at(REPO, 55).repo_rank, Some(1.0));
    // 55 (unstarred, REPO) waits behind the 6 starred PRs (56, 57, 62, 52,
    // 53, 51), OTHER's unstarred 61 (repo priority) and 54 (same entry,
    // lower number).
    assert_eq!(at(REPO, 55).ahead_dispatch_fleet, Some(8));
    // Among the stars set at 10 h, OTHER's 62 goes first on repo priority.
    assert_eq!(at(OTHER, 62).ahead_dispatch_fleet, Some(2));
}

#[test]
fn without_roster_history_both_sides_leave_the_rank_unknown() {
    let specs = specs();
    let a = trained(&specs, None);
    let tracker = served_tracker(&specs);
    let training = trained_at(&a, REPO, 55);
    let serving = tracker.priority_inputs_of(REPO, 55, h(AT), None).unwrap();
    assert_eq!(serving, training);
    assert_eq!((training.repo_rank, training.ahead_dispatch_fleet), (None, None));
    assert_eq!(training.starred_any, Some(false));
    let coverage = PriorityCoverage::of(&a.priority_inputs);
    assert_eq!(coverage.repo_rank_known, 0);
    assert!(coverage.star_known > 0 && coverage.star_known < coverage.rows);
}

/// Self-check: training with a history that differs only after `AT`
/// (another future edit) gives the same inputs at `AT`.
#[test]
fn a_roster_edit_after_the_row_changes_no_row_at_t() {
    let specs = specs();
    let mut later = fleet_history();
    later.push(revision(h(25.0), Some(h(25.0)), vec![member(REPO, 1), member(OTHER, 999)]));
    let base = trained(&specs, Some(&fleet_history()));
    let with_edit = trained(&specs, Some(&later));
    for pr in [51, 55, 56] {
        assert_eq!(trained_at(&base, REPO, pr), trained_at(&with_edit, REPO, pr), "PR {pr}");
    }
}

/// The leak test at the fit's input (#10508): a roster edit knowable only at
/// or after the cutoff (committed before it but first observed at it, or
/// committed after it) leaves every row's `eta-fit/v2` feature vector, and
/// so the v2 fit, unchanged. The same edit observed early does move them,
/// so the test cannot pass vacuously.
#[test]
fn the_v2_features_ignore_roster_edits_knowable_at_or_after_the_cutoff() {
    let specs = specs();
    let base = trained(&specs, Some(&fleet_history()));
    let xs = |a: &Assembled| v2::features_v2(&a.rows, &a.priority_inputs);
    let edit = vec![member(REPO, 0), member(OTHER, 500)];
    let mut late = fleet_history();
    late.push(revision(h(2.0), Some(cutoff()), edit.clone()));
    late.push(revision(cutoff() + Duration::hours(1), None, edit.clone()));
    assert_eq!(xs(&trained(&specs, Some(&late))), xs(&base));
    let mut early = fleet_history();
    early.push(revision(h(2.0), Some(h(2.0)), edit));
    assert_ne!(xs(&trained(&specs, Some(&early))), xs(&base));
}
