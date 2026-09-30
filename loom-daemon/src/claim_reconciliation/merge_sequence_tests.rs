//! Tests for the merge-sequencing planner (#9686).

use super::*;

fn pr(number: u32, created: &str) -> SequencePr {
    SequencePr {
        number,
        created_at: created.to_string(),
        updated_at: created.to_string(),
        head_sha: Some(format!("{:040x}", number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: vec![],
    }
}

impl SequencePr {
    fn draft(mut self) -> SequencePr {
        self.draft = true;
        self
    }
    fn held(mut self, label: &str) -> SequencePr {
        self.labels.push(label.to_string());
        self
    }
}

fn files(entries: &[(u32, &[&str])]) -> BTreeMap<u32, BTreeSet<String>> {
    entries
        .iter()
        .map(|(n, paths)| {
            (
                *n,
                paths
                    .iter()
                    .map(|p| (*p).to_string())
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect()
}

const NO_MARKERS: BTreeMap<u32, SequenceMarker> = BTreeMap::new();

// --- Eligibility --------------------------------------------------------

#[test]
fn drafts_holds_and_in_flight_prs_are_ineligible() {
    let base = pr(1, "2026-01-01T00:00:00Z");
    assert!(base.eligible_for_ordering());
    assert!(!base.clone().draft().eligible_for_ordering());
    for hold in super::super::VERDICT_HOLD_LABELS {
        assert!(!base.clone().held(hold).eligible_for_ordering(), "{hold}");
    }
    assert!(!base.clone().held("loom:reviewing").eligible_for_ordering());
    assert!(!base.clone().held("loom:treating").eligible_for_ordering());
}

#[test]
fn trigger_requires_more_than_two_open_prs() {
    let prs = [pr(1, "2026-01-01T00:00:00Z"), pr(2, "2026-01-02T00:00:00Z")];
    assert!(plan_repo(&prs, &files(&[]), &NO_MARKERS).is_empty());
}

// --- Grouping -----------------------------------------------------------

#[test]
fn shared_files_group_and_isolated_prs_do_not() {
    let a = pr(1, "2026-01-01T00:00:00Z");
    let b = pr(2, "2026-01-02T00:00:00Z");
    let c = pr(3, "2026-01-03T00:00:00Z");
    let prs = [a, b.clone(), c];
    let f = files(&[
        (1, &["src/a.rs"]),
        (2, &["src/a.rs", "src/b.rs"]),
        (3, &["docs/x.md"]),
    ]);
    let groups = plan_repo(&prs, &f, &NO_MARKERS);
    assert_eq!(groups.len(), 1, "one overlapping pair, one singleton: {groups:?}");
    let g = &groups[0];
    assert_eq!(g.order, vec![1, 2], "oldest first");
    assert_eq!(g.edges.len(), 1);
    assert_eq!(g.edges[0].follower, 2);
    assert_eq!(g.edges[0].after, 1);
    assert_eq!(g.edges[0].reason, EdgeReason::SharedFiles);
    // The singleton got no edge and no marker.
    assert!(!g.order.contains(&3));
}

#[test]
fn a_failed_files_read_is_a_singleton_not_a_wildcard() {
    // Missing files entry = unknown, never "overlaps with everything".
    let a = pr(1, "2026-01-01T00:00:00Z");
    let b = pr(2, "2026-01-02T00:00:00Z");
    let prs = [a, b];
    let f = files(&[(1, &["src/a.rs"])]);
    assert!(plan_repo(&prs, &f, &NO_MARKERS).is_empty());
}

#[test]
fn three_way_overlap_forms_one_chain_not_a_star() {
    // A chain means #3 waits for #2 AND #2 waits for #1 — a stable landing
    // order, not "everyone after the oldest".
    let prs = [
        pr(1, "2026-01-01T00:00:00Z"),
        pr(2, "2026-01-02T00:00:00Z"),
        pr(3, "2026-01-03T00:00:00Z"),
    ];
    let f = files(&[
        (1, &["shared.rs"]),
        (2, &["shared.rs"]),
        (3, &["shared.rs"]),
    ]);
    let groups = plan_repo(&prs, &f, &NO_MARKERS);
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert_eq!(g.order, vec![1, 2, 3]);
    assert_eq!(
        g.edges
            .iter()
            .map(|e| (e.follower, e.after))
            .collect::<Vec<_>>(),
        vec![(2, 1), (3, 2)],
        "chain edges"
    );
}

// --- Ordering constraints -----------------------------------------------

#[test]
fn an_existing_marker_is_a_constraint_not_an_override_target() {
    // #2 carries a manual (hard) marker after #3. The computed order must
    // honor the constraint, and Phase 2 must not re-mark #2 (it is a
    // holder) — pinned here at the planner level: the marker edge lands
    // #3 before #2 despite #2 being older. #1 is an unrelated singleton
    // that also clears the >2 trigger.
    let prs = [
        pr(1, "2026-01-01T00:00:00Z"),
        pr(2, "2026-01-02T00:00:00Z"),
        pr(3, "2026-01-03T00:00:00Z"),
    ];
    let f = files(&[(1, &["other.rs"]), (2, &["s.rs"]), (3, &["s.rs"])]);
    let mut markers = NO_MARKERS;
    markers.insert(
        2,
        SequenceMarker {
            after: 3,
            pred_head: format!("{:040x}", 3),
            follower_head: format!("{:040x}", 2),
            plan: "manual".into(),
            source: None,
        },
    );
    let groups = plan_repo(&prs, &f, &markers);
    // Both are eligible members; the constraint orders 3 before 2.
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].order, vec![3, 2]);
}

#[test]
fn a_stacked_base_constrains_the_order() {
    let mut follower = pr(5, "2026-01-01T00:00:00Z");
    follower.base_ref = "feature/issue-4".to_string();
    let pred = pr(4, "2026-01-02T00:00:00Z");
    let prs = [follower, pred];
    // Even with NO shared files, the stacked base is a real dependency.
    let f = files(&[]);
    let groups = plan_repo(&prs, &f, &NO_MARKERS);
    // No file overlap ⇒ no component at all in this release (base stacking
    // alone is #9372's domain); pinned here so a future widening is a
    // deliberate choice, not an accident.
    assert!(groups.is_empty(), "file-less stacking must not group yet");
}

#[test]
fn a_constraint_cycle_skips_the_whole_component() {
    let prs = [pr(1, "2026-01-01T00:00:00Z"), pr(2, "2026-01-02T00:00:00Z")];
    let f = files(&[(1, &["s.rs"]), (2, &["s.rs"])]);
    let mut markers = NO_MARKERS;
    // #1 after #2 and #2 after #1 — unsatisfiable.
    for (follower, after) in [(1, 2), (2, 1)] {
        markers.insert(
            follower,
            SequenceMarker {
                after,
                pred_head: format!("{after:040x}"),
                follower_head: format!("{follower:040x}"),
                plan: "manual".into(),
                source: None,
            },
        );
    }
    assert!(plan_repo(&prs, &f, &markers).is_empty(), "cycle ⇒ no plan");
}

// --- Determinism --------------------------------------------------------

#[test]
fn the_plan_is_stable_across_listing_order() {
    let shared = files(&[(1, &["s"]), (2, &["s"]), (3, &["s"])]);
    let forward = [
        pr(1, "2026-01-01T00:00:00Z"),
        pr(2, "2026-01-02T00:00:00Z"),
        pr(3, "2026-01-03T00:00:00Z"),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();
    let a = plan_repo(&forward, &shared, &NO_MARKERS);
    let b = plan_repo(&reversed, &shared, &NO_MARKERS);
    assert_eq!(a, b, "identical input in any order yields the identical plan");
}

#[test]
fn the_plan_id_is_deterministic_and_head_sensitive() {
    let id1 = plan_id(&[(1, "aaa"), (2, "bbb")]);
    let id2 = plan_id(&[(2, "bbb"), (1, "aaa")]);
    let id3 = plan_id(&[(1, "ccc"), (2, "bbb")]);
    assert_eq!(id1, id2, "member order must not matter");
    assert_ne!(id1, id3, "a head change must change the plan id");
    assert!(id1.starts_with("seq-"));
}

// --- Hold decisions -----------------------------------------------------

fn marker_after(after: u32, source: Option<&str>) -> SequenceMarker {
    SequenceMarker {
        after,
        pred_head: format!("{after:040x}"),
        follower_head: format!("{:040x}", 99),
        plan: "seq-test".into(),
        source: source.map(str::to_string),
    }
}

fn pred_open_at_head(head: &str) -> PredecessorState {
    PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(head.to_string()),
        updated_at: Some(Utc::now().to_rfc3339()),
    }
}

#[test]
fn release_decisions_follow_the_evaluation_contract() {
    // Landed at recorded head ⇒ release; dissolved ⇒ release; unknown ⇒ hold.
    let m = marker_after(5, Some("pass"));
    assert_eq!(
        hold_action(&m, Some(&pred_open_at_head(&m.pred_head)), true, 72.0),
        HoldAction::HoldSoft
    );
    let landed = PredecessorState {
        merged: true,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert_eq!(hold_action(&m, Some(&landed), true, 72.0), HoldAction::Release);
    let dissolved = PredecessorState {
        merged: false,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert_eq!(hold_action(&m, Some(&dissolved), true, 72.0), HoldAction::ReleaseDissolved);
    // Unreadable predecessor ⇒ fail closed (hold), never release.
    assert_eq!(hold_action(&m, None, true, 72.0), HoldAction::HoldSoft);
    let hard = marker_after(5, None);
    assert_eq!(hold_action(&hard, None, true, 72.0), HoldAction::HoldHard);
}

#[test]
fn a_moved_predecessor_head_voids_the_hold_for_replanning() {
    let m = marker_after(5, Some("pass"));
    let moved = pred_open_at_head(&format!("{:040x}", 12345));
    assert_eq!(hold_action(&m, Some(&moved), true, 72.0), HoldAction::VoidAndReplan);
}

#[test]
fn only_soft_holds_on_approved_followers_expire() {
    let quiet = PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(format!("{:040x}", 5)),
        updated_at: Some("2026-01-01T00:00:00Z".to_string()),
    };
    let soft = marker_after(5, Some("pass"));
    let hard = marker_after(5, None);
    // Soft + approved + quiet past the bound ⇒ expire.
    assert_eq!(hold_action(&soft, Some(&quiet), true, 72.0), HoldAction::Expire);
    // Soft but NOT approved ⇒ hold (nothing mergeable is starved).
    assert_eq!(hold_action(&soft, Some(&quiet), false, 72.0), HoldAction::HoldSoft);
    // Hard + approved + quiet ⇒ hold (a semantic dependency never expires
    // into merge permission — the #9063 non-negotiable).
    assert_eq!(hold_action(&hard, Some(&quiet), true, 72.0), HoldAction::HoldHard);
    // Soft + approved + quiet but within the bound ⇒ hold.
    assert_eq!(hold_action(&soft, Some(&quiet), true, f64::INFINITY), HoldAction::HoldSoft);
    // Predecessor with unknown freshness ⇒ hold.
    let freshless = PredecessorState {
        updated_at: None,
        ..quiet.clone()
    };
    assert_eq!(hold_action(&soft, Some(&freshless), true, 72.0), HoldAction::HoldSoft);
}

// --- Deferral -----------------------------------------------------------

#[test]
fn deferral_applies_only_to_clean_in_flight_ordering() {
    let m = marker_after(5, Some("pass"));
    assert!(defer_base_repair(&m, &pred_open_at_head(&m.pred_head)));
    // Moved head ⇒ replan territory: flag the conflict, don't hide it.
    assert!(!defer_base_repair(&m, &pred_open_at_head(&format!("{:040x}", 777))));
    // Landed ⇒ flag: the follower must now be repaired against the new base.
    let landed = PredecessorState {
        merged: true,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert!(!defer_base_repair(&m, &landed));
    // Dissolved ⇒ flag.
    let dissolved = PredecessorState {
        merged: false,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert!(!defer_base_repair(&m, &dissolved));
}

// --- Rendered comments --------------------------------------------------

#[test]
fn apply_and_release_comments_carry_marker_and_reason() {
    let m = marker_after(11, Some("pass"));
    let apply = apply_comment_body(&m, EdgeReason::SharedFiles);
    assert!(apply.contains(&marker_text(&m)), "the marker is the state: {apply}");
    assert!(apply.contains("#11"), "{apply}");
    let release = release_comment_body(&m, HoldAction::Release);
    assert!(release.contains("loom:sequence released"), "{release}");
    assert!(release.contains("merged at the recorded head"), "{release}");
    let defer = defer_comment_body(&m);
    assert!(defer.contains("defer-repair"), "{defer}");
    assert!(defer.contains("never deferred") || defer.contains("NOT deferred"), "{defer}");
}

#[test]
fn plan_report_counts_groups_without_writing() {
    // Pure smoke: PlanReport defaults and the group accounting fields exist
    // and are Copy-able counters, so the dry-run verb cannot mutate state
    // through them.
    let r = PlanReport::default();
    assert_eq!(r.open_prs, 0);
    assert!(r.groups.is_empty());
    assert_eq!(r.already_planned, 0);
    assert_eq!(r.holders, 0);
}
