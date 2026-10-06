//! Dependency-aware ETAs (#10510): the composition wrapper over a base.

use super::{as_of, history_a, input_at, provenance};
use crate::eta::dependency::{
    node_seed, quantile_at, recompute, DependencyBook, DependencyGraph, Edge, EdgeKind, EdgeSource,
    Node, NodeKey, ParentStatus, METHOD,
};
use crate::eta::explanation::{CurrentStageRecord, EstimateResult, Explanation};
use crate::eta::heuristics::{blank, DependencyComposition, LandTwinOtterB, LAND_TANDEM_WREN};
use crate::eta::score::EstimateSummary;
use crate::eta::shadow_fleet::DEFAULT_MAX_ACTIVE;
use crate::eta::simulate::{run_explanation, SplitMix64};
use crate::eta::{
    CurrentState, EstimateInput, Heuristic, Kind, NoEstimateReason, Registry, Stage, StageSamples,
    Subject, Tier, DRAWS,
};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;
use std::sync::Arc;

const REPO: &str = "rjwalters/loom";
const ID: &str = "land-test-dependency";
const PATH: [i64; 4] = [1_000, 2_000, 3_000, 4_000];

/// A base that answers from a table: `own[issue]` for the item as it is,
/// [`PATH`] for the remaining path from dispatch, and the resolver's
/// refusal for a refused item.
struct Table {
    own: BTreeMap<u32, [i64; 4]>,
}

fn answered(mut e: Explanation, q: [i64; 4]) -> Explanation {
    e.result = Some(EstimateResult {
        p25_sec: q[0],
        p50_sec: q[1],
        p75_sec: q[2],
        p90_sec: Some(q[3]),
        eta_p50_at: e.as_of + Duration::seconds(q[1]),
        samples_min: 10,
        stage_marks: Vec::new(),
        tail_extrapolated: false,
    });
    e
}

impl Heuristic for Table {
    fn id(&self) -> &'static str {
        "test-table"
    }
    fn kind(&self) -> Kind {
        Kind::Land
    }
    fn estimate(&self, input: &EstimateInput, _: &StageSamples) -> Explanation {
        let mut e = blank("test-table", Kind::Land, input);
        // The frame a path heuristic records, so the wrapper's keeping of
        // the base's own frame is observable.
        if let CurrentState::At(c) = &input.current {
            e.current_stage = Some(CurrentStageRecord {
                stage: c.stage,
                entered_at: c.entered_at,
                age_sec: c.age_sec,
                age_source: c.age_source,
                rework_rounds: c.rework_rounds,
            });
        }
        match &input.current {
            CurrentState::Refused(reason) => {
                e.no_estimate_reason = Some(*reason);
                e
            }
            CurrentState::At(c) if c.stage == Stage::SweepCurator && c.age_sec == 0 => {
                answered(e, PATH)
            }
            CurrentState::At(_) => match self.own.get(&input.subject.issue) {
                Some(q) => answered(e, *q),
                None => {
                    e.no_estimate_reason = Some(NoEstimateReason::InsufficientSamples);
                    e
                }
            },
        }
    }
}

fn wrapper(own: &[(u32, [i64; 4])]) -> DependencyComposition {
    DependencyComposition::new(
        ID,
        Box::new(Table {
            own: own.iter().copied().collect(),
        }),
    )
}

fn key(issue: u32) -> NodeKey {
    NodeKey::new(REPO, issue)
}

/// Issue `issue` at `current`.
fn item(issue: u32, current: CurrentState) -> EstimateInput {
    let mut input = input_at(Stage::ReviewWait, 0, 0);
    input.subject = Subject::new(REPO, None, issue);
    input.current = current;
    input
}

fn blocked(issue: u32) -> EstimateInput {
    item(issue, CurrentState::Refused(NoEstimateReason::Blocked))
}

fn in_review(issue: u32) -> EstimateInput {
    item(issue, input_at(Stage::ReviewWait, 600, 0).current)
}

fn edge(child: u32, parent: u32, source: EdgeSource) -> Edge {
    Edge {
        child: key(child),
        parent: key(parent),
        source,
        known_at: as_of() - Duration::hours(1),
    }
}

fn graph(nodes: Vec<EstimateInput>, edges: Vec<Edge>) -> Arc<DependencyGraph> {
    Arc::new(DependencyGraph {
        edges,
        nodes: nodes
            .into_iter()
            .map(|i| {
                (
                    NodeKey::of(&i.subject),
                    Node::Open {
                        input: Box::new(i),
                        modeled: None,
                    },
                )
            })
            .collect(),
    })
}

fn with_graph(mut input: EstimateInput, g: &Arc<DependencyGraph>) -> EstimateInput {
    input.dependencies = Some(g.clone());
    input
}

fn uniforms(issue: u32) -> Vec<f64> {
    let mut rng = SplitMix64::new(node_seed(ID, as_of(), &key(issue).to_string()));
    (0..DRAWS).map(|_| rng.next_f64()).collect()
}

fn nearest_rank(mut draws: Vec<f64>) -> [i64; 4] {
    draws.sort_by(f64::total_cmp);
    [25, 50, 75, 90].map(|pct: usize| {
        let rank = (pct * draws.len()).div_ceil(100).clamp(1, draws.len());
        draws[rank - 1].round() as i64
    })
}

fn q4(e: &Explanation) -> [i64; 4] {
    let r = e.result.as_ref().expect("an answer");
    [r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec.unwrap()]
}

/// Serialised, with the identity the wrapper rewrites normalised away.
fn body(mut e: Explanation) -> serde_json::Value {
    e.heuristic = String::new();
    e.estimate_id = String::new();
    serde_json::to_value(e).unwrap()
}

// ------------------------------------------------------------ bit-identity

#[test]
fn dependency_free_items_are_bit_identical_to_the_base() {
    let base = LandTwinOtterB::new(None);
    let wren = DependencyComposition::tandem_wren(None);
    let history = history_a();
    // Pre-PR (answered by land-v2's path) and PR-level (refused: no fit).
    for input in [
        input_at(Stage::SweepBuilder, 600, 0),
        input_at(Stage::SweepCurator, 0, 0),
        input_at(Stage::ReviewWait, 600, 0),
    ] {
        let own = base.estimate(&input, &history);
        // No graph; a graph with edges only between other items; a graph
        // whose edge to this item is known only after `as_of`.
        let me = input.subject.issue;
        let mut late = edge(me, 1, EdgeSource::ParkRecord);
        late.known_at = as_of() + Duration::hours(1);
        let graphs = [
            None,
            Some(graph(vec![blocked(1)], vec![edge(2, 1, EdgeSource::ParkRecord)])),
            Some(graph(vec![blocked(1)], vec![late])),
        ];
        for g in graphs {
            let mut input = input.clone();
            input.dependencies = g;
            let e = wren.estimate(&input, &history);
            assert_eq!(e.heuristic, LAND_TANDEM_WREN);
            assert_eq!(e.result, own.result);
            assert_eq!(e.no_estimate_reason, own.no_estimate_reason);
            assert!(e.dependencies.is_none());
            assert_eq!(body(e), body(own.clone()));
        }
    }
}

#[test]
fn a_start_edge_on_a_started_item_is_ignored() {
    let w = wrapper(&[(2, [100, 200, 300, 400]), (1, [10, 20, 30, 40])]);
    let g = graph(vec![in_review(1)], vec![edge(2, 1, EdgeSource::ParkRecord)]);
    let e = w.estimate(&with_graph(in_review(2), &g), &history_a());
    assert_eq!(q4(&e), [100, 200, 300, 400]);
    assert!(e.dependencies.is_none());
}

// --------------------------------------------------------------- the leak

#[test]
fn an_edge_known_at_or_after_as_of_is_ignored() {
    let w = wrapper(&[(1, [50_000; 4])]);
    for known_at in [as_of(), as_of() + Duration::seconds(1)] {
        let mut late = edge(2, 1, EdgeSource::ParkRecord);
        late.known_at = known_at;
        let g = graph(vec![in_review(1)], vec![late]);
        // The child is blocked: without the (future) edge, the base's
        // refusal stands, exactly as if the graph were absent.
        let e = w.estimate(&with_graph(blocked(2), &g), &history_a());
        assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::Blocked));
        assert!(e.dependencies.is_none(), "a future edge leaves no trace");
        assert!(g.parents(&key(2), as_of()).is_empty());
    }
}

// --------------------------------------------------------------- diamond

#[test]
fn a_diamond_shares_its_ancestor_draw_common_random_numbers() {
    // 4 waits on 2 and 3; both wait on 1 (an open PR).
    let a = [10_000, 20_000, 30_000, 40_000];
    let w = wrapper(&[(1, a)]);
    let g = graph(
        vec![in_review(1), blocked(2), blocked(3)],
        vec![
            edge(4, 2, EdgeSource::ParkRecord),
            edge(4, 3, EdgeSource::NativeDependency),
            edge(2, 1, EdgeSource::ParkRecord),
            edge(3, 1, EdgeSource::EpicPhase),
        ],
    );
    let e = w.estimate(&with_graph(blocked(4), &g), &history_a());
    assert_eq!(e.no_estimate_reason, None);
    let record = e.dependencies.as_ref().unwrap();
    assert_eq!(record.method, METHOD);
    assert_eq!(record.base, "test-table");
    // One record per node: the shared ancestor appears once.
    let names: Vec<&str> = record.nodes.iter().map(|n| n.node.as_str()).collect();
    assert_eq!(
        names,
        [
            &key(4).to_string(),
            &key(1).to_string(),
            &key(2).to_string(),
            &key(3).to_string()
        ]
    );

    // The same draw of #1 feeds both branches.
    let (u1, u2, u3, u4) = (uniforms(1), uniforms(2), uniforms(3), uniforms(4));
    let crn: Vec<f64> = (0..DRAWS)
        .map(|s| {
            let land1 = quantile_at(a, u1[s]);
            let land2 = land1 + quantile_at(PATH, u2[s]);
            let land3 = land1 + quantile_at(PATH, u3[s]);
            land2.max(land3) + quantile_at(PATH, u4[s])
        })
        .collect();
    assert_eq!(q4(&e), nearest_rank(crn));
    // Independent copies of #1 per branch would double count it: the max of
    // two independent draws sits above the shared one.
    let mut rng = SplitMix64::new(7);
    let independent: Vec<f64> = (0..DRAWS)
        .map(|s| {
            let land2 = quantile_at(a, u1[s]) + quantile_at(PATH, u2[s]);
            let land3 = quantile_at(a, rng.next_f64()) + quantile_at(PATH, u3[s]);
            land2.max(land3) + quantile_at(PATH, u4[s])
        })
        .collect();
    assert!(nearest_rank(independent)[1] > q4(&e)[1]);

    // Explanation first: the record alone recomputes the answer.
    let [p25, p50, p75, p90] = q4(&e);
    assert_eq!(recompute(record), Some((p25, p50, p75, p90)));
    assert_eq!(run_explanation(&e), Some((p25, p50, p75, p90)));

    // Every parent is estimated, and one of them binds.
    assert_eq!(record.parents.len(), 2);
    assert!(record
        .parents
        .iter()
        .all(|p| p.status == ParentStatus::Estimated));
    assert!(record.parents.iter().all(|p| p.edge == EdgeKind::Start));
    assert!(record.binding_parent.is_some());
    assert!(record.parents.iter().all(|p| p.p50_sec.unwrap() > a[1]));
}

// ----------------------------------------------------------------- cycles

#[test]
fn a_cycle_is_refused_dependency_cycle_and_propagates() {
    let w = wrapper(&[]);
    let g = graph(
        vec![blocked(1), blocked(2)],
        vec![
            edge(1, 2, EdgeSource::ParkRecord),
            edge(2, 1, EdgeSource::ParkRecord),
            edge(3, 1, EdgeSource::ParkRecord),
        ],
    );
    let e = w.estimate(&with_graph(blocked(1), &g), &history_a());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::DependencyCycle));
    assert!(e.result.is_none());
    let record = e.dependencies.unwrap();
    assert_eq!(record.blocked_by.as_deref(), Some("dependency_cycle"));
    assert!(record.nodes.is_empty());

    let e = w.estimate(&with_graph(blocked(3), &g), &history_a());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::BlockedBy));
    assert_eq!(
        e.dependencies.unwrap().blocked_by.as_deref(),
        Some("blocked_by:rjwalters/loom#1 (dependency_cycle)")
    );

    // A self-edge is a cycle too.
    let g = graph(vec![], vec![edge(5, 5, EdgeSource::ParkRecord)]);
    let e = w.estimate(&with_graph(blocked(5), &g), &history_a());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::DependencyCycle));
}

// --------------------------------------------------------------- refusals

#[test]
fn a_held_parent_propagates_its_refusal_reason() {
    let w = wrapper(&[]);
    // #1 is held for the operator: the resolver refuses it `blocked`.
    let g = graph(
        vec![blocked(1), blocked(2)],
        vec![
            edge(2, 1, EdgeSource::ParkRecord),
            edge(3, 2, EdgeSource::ParkRecord),
        ],
    );
    let e = w.estimate(&with_graph(blocked(2), &g), &history_a());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::BlockedBy));
    let record = e.dependencies.unwrap();
    assert_eq!(record.blocked_by.as_deref(), Some("blocked_by:rjwalters/loom#1 (blocked)"));
    assert_eq!(record.parents[0].status, ParentStatus::Refused);
    assert_eq!(record.parents[0].reason.as_deref(), Some("blocked"));

    // Two levels down, the chain is kept.
    let e = w.estimate(&with_graph(blocked(3), &g), &history_a());
    assert_eq!(
        e.dependencies.unwrap().blocked_by.as_deref(),
        Some("blocked_by:rjwalters/loom#2 (blocked_by:rjwalters/loom#1 (blocked))")
    );

    // A parent the graph does not know.
    let g = graph(vec![], vec![edge(2, 9, EdgeSource::ParkRecord)]);
    let e = w.estimate(&with_graph(blocked(2), &g), &history_a());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::BlockedByUnknown));
    let record = e.dependencies.unwrap();
    assert_eq!(record.blocked_by.as_deref(), Some("blocked_by_unknown:rjwalters/loom#9"));
    assert_eq!(record.parents[0].status, ParentStatus::Unknown);
}

#[test]
fn a_landed_parent_releases_the_child_onto_its_own_path() {
    let w = wrapper(&[]);
    let mut g = DependencyGraph {
        edges: vec![edge(2, 1, EdgeSource::ParkRecord)],
        nodes: BTreeMap::new(),
    };
    g.nodes.insert(
        key(1),
        Node::Landed {
            at: as_of() - Duration::hours(2),
        },
    );
    let g = Arc::new(g);
    let e = w.estimate(&with_graph(blocked(2), &g), &history_a());
    let u2 = uniforms(2);
    let expected = nearest_rank((0..DRAWS).map(|s| quantile_at(PATH, u2[s])).collect());
    assert_eq!(q4(&e), expected);
    let record = e.dependencies.as_ref().unwrap();
    assert_eq!(record.parents[0].status, ParentStatus::Landed);
    assert_eq!(record.binding_parent, None, "a landed parent binds nothing");
    assert_eq!(run_explanation(&e), Some((expected[0], expected[1], expected[2], expected[3])));
}

// ------------------------------------------------------------- sequencing

#[test]
fn a_sequencing_follower_is_bounded_at_merge_only() {
    let follower = [100, 200, 300, 400];
    let predecessor = [50_000; 4];
    let w = wrapper(&[(1, predecessor), (2, follower)]);
    let g = graph(vec![in_review(1)], vec![edge(2, 1, EdgeSource::Sequence)]);
    let e = w.estimate(&with_graph(in_review(2), &g), &history_a());
    // Merge ≥ the predecessor's merge, and nothing more: no path from
    // dispatch is added, as a start edge would.
    assert_eq!(q4(&e), predecessor);
    let record = e.dependencies.as_ref().unwrap();
    assert_eq!(record.parents[0].edge, EdgeKind::Merge);
    assert!(record.nodes[0].path_sec.is_none());
    assert_eq!(record.binding_parent.as_deref(), Some("rjwalters/loom#1"));
    assert_eq!(record.binding_share, Some(1.0));
    // The follower keeps its own frame: current stage and stages.
    assert_eq!(e.current_stage.as_ref().map(|c| c.stage), Some(Stage::ReviewWait));

    // A fast predecessor leaves the follower's own answer.
    let w = wrapper(&[(1, [1, 1, 1, 1]), (2, follower)]);
    let e = w.estimate(&with_graph(in_review(2), &g), &history_a());
    let u2 = uniforms(2);
    let own = nearest_rank(
        (0..DRAWS)
            .map(|s| quantile_at(follower, u2[s]).max(1.0))
            .collect(),
    );
    assert_eq!(q4(&e), own);
    assert_eq!(e.dependencies.unwrap().binding_parent, None);

    // A merge edge never answers an item that cannot start.
    let e = w.estimate(&with_graph(blocked(2), &g), &history_a());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::Blocked));
}

#[test]
fn a_start_edge_adds_the_path_after_the_parent_lands() {
    let parent = [50_000; 4];
    let w = wrapper(&[(1, parent)]);
    let g = graph(vec![in_review(1)], vec![edge(2, 1, EdgeSource::ParkRecord)]);
    let e = w.estimate(&with_graph(blocked(2), &g), &history_a());
    let u2 = uniforms(2);
    let expected = nearest_rank(
        (0..DRAWS)
            .map(|s| 50_000.0 + quantile_at(PATH, u2[s]))
            .collect(),
    );
    assert_eq!(q4(&e), expected);
    assert_eq!(e.combination.as_ref().unwrap().method, METHOD);
}

// ---------------------------------------------------------------- the book

#[test]
fn the_book_keeps_first_sight_as_known_at_and_drops_vanished_edges() {
    let t0: DateTime<Utc> = as_of() - Duration::hours(3);
    let t1 = t0 + Duration::hours(1);
    let mut book = DependencyBook::default();
    book.set(&key(2), EdgeSource::ParkRecord, &[key(1)], t0);
    book.set(&key(2), EdgeSource::ParkRecord, &[key(1), key(3)], t1);
    let known: Vec<_> = book
        .edges
        .iter()
        .map(|e| (e.parent.issue, e.known_at))
        .collect();
    assert_eq!(known, [(1, t0), (3, t1)]);
    book.set(&key(2), EdgeSource::Sequence, &[key(4)], t1);
    book.set(&key(2), EdgeSource::ParkRecord, &[key(3)], t1);
    let parents: Vec<_> = book.edges.iter().map(|e| e.parent.issue).collect();
    assert_eq!(parents, [4, 3]);

    // Due: never read first, then stale; at most the budget.
    let wanted = [
        (key(2), EdgeSource::ParkRecord),
        (key(7), EdgeSource::ParkRecord),
    ];
    assert_eq!(book.due(&wanted, t1, 900, 8), [(key(7), EdgeSource::ParkRecord)]);
    assert_eq!(book.due(&wanted, t1 + Duration::hours(1), 900, 1).len(), 1);

    book.set_landed(&key(3), t0);
    book.retain(&[key(9)].into_iter().collect());
    assert!(book.edges.is_empty());
    assert!(book.landed.is_empty());
}

// ------------------------------------------------------------ registration

#[test]
fn tandem_wren_is_a_registered_shadow_candidate_over_twin_otter_b() {
    let registry = Registry::builtin();
    assert!(registry.registers(Kind::Land, LAND_TANDEM_WREN));
    assert_eq!(registry.ids().last(), Some(&LAND_TANDEM_WREN));
    assert_eq!(registry.tier_of(LAND_TANDEM_WREN), Some(Tier::Candidate));
    let h = registry.get(LAND_TANDEM_WREN).unwrap();
    assert!(h.models_hold(), "reads exactly the input twin-otter-b reads");
    assert_ne!(registry.current(Kind::Land, None).id(), LAND_TANDEM_WREN);
    registry.check_budget(DEFAULT_MAX_ACTIVE).unwrap();

    // It appears as an `alternates[]` entry beside the current estimate.
    let mut input = input_at(Stage::SweepBuilder, 600, 0);
    input.provenance = provenance();
    let summary = EstimateSummary::of(&h.estimate(&input, &history_a()));
    let current: BTreeMap<Kind, String> = [(Kind::Land, "land-v1".to_string())].into();
    let registered = [(
        Kind::Land,
        registry
            .for_kind(Kind::Land)
            .map(|h| h.id().to_string())
            .collect(),
    )]
    .into();
    let alternates =
        crate::observability::eta_snapshot::select_alternates(&[summary], &current, &registered);
    let row = alternates.values().next().unwrap();
    assert_eq!(row[0].heuristic, LAND_TANDEM_WREN);
}
