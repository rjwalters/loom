//! Tests for the merge-sequencing planner (#9686).

use super::*;

/// An open, Judge-approved PR. Approved by default: these fixtures exercise
/// grouping and constraints, and since #10371 no hold is planned behind a
/// predecessor without `loom:pr` (`merge_sequence_ready_tests.rs` covers that).
fn pr(number: u32, created: &str) -> SequencePr {
    SequencePr {
        number,
        created_at: created.to_string(),
        updated_at: created.to_string(),
        head_sha: Some(format!("{:040x}", number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: vec!["loom:pr".to_string()],
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
        hold_action(&m, Some(&pred_open_at_head(&m.pred_head)), live(&m), true, 72.0),
        HoldAction::HoldSoft
    );
    let landed = PredecessorState {
        merged: true,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert_eq!(hold_action(&m, Some(&landed), live(&m), true, 72.0), HoldAction::Release);
    let dissolved = PredecessorState {
        merged: false,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert_eq!(
        hold_action(&m, Some(&dissolved), live(&m), true, 72.0),
        HoldAction::ReleaseDissolved
    );
    // Unreadable predecessor ⇒ fail closed (hold), never release.
    assert_eq!(hold_action(&m, None, live(&m), true, 72.0), HoldAction::HoldSoft);
    let hard = marker_after(5, None);
    assert_eq!(hold_action(&hard, None, live(&hard), true, 72.0), HoldAction::HoldHard);
}

/// The follower's live head when it has NOT moved since the marker.
fn live(m: &SequenceMarker) -> Option<&str> {
    Some(m.follower_head.as_str())
}

#[test]
fn a_moved_follower_head_voids_the_hold_for_replanning() {
    // The follower received new commits (or a force-push) while held: the
    // marker no longer describes THIS tree, so the hold is void whatever the
    // predecessor is doing — in flight, landed, or unreadable.
    let moved = format!("{:040x}", 424242);
    let soft = marker_after(5, Some("pass"));
    let hard = marker_after(5, None);
    let in_flight = pred_open_at_head(&soft.pred_head);
    assert_eq!(
        hold_action(&soft, Some(&in_flight), Some(&moved), true, 72.0),
        HoldAction::VoidAndReplan
    );
    assert_eq!(
        hold_action(&hard, Some(&in_flight), Some(&moved), true, 72.0),
        HoldAction::VoidAndReplan
    );
    let landed = PredecessorState {
        merged: true,
        open: false,
        head_sha: Some(soft.pred_head.clone()),
        updated_at: None,
    };
    assert_eq!(
        hold_action(&soft, Some(&landed), Some(&moved), true, 72.0),
        HoldAction::VoidAndReplan,
        "a landed predecessor must not release a hold pinned to an older follower tree"
    );
    assert_eq!(hold_action(&soft, None, Some(&moved), true, 72.0), HoldAction::VoidAndReplan);
}

#[test]
fn an_unknown_follower_head_fails_closed() {
    // No live head in the listing ⇒ nothing proves the pin is stale or
    // current: hold, never release or void.
    let soft = marker_after(5, Some("pass"));
    let hard = marker_after(5, None);
    let landed = PredecessorState {
        merged: true,
        open: false,
        head_sha: Some(soft.pred_head.clone()),
        updated_at: None,
    };
    assert_eq!(hold_action(&soft, Some(&landed), None, true, 72.0), HoldAction::HoldSoft);
    assert_eq!(hold_action(&soft, Some(&landed), Some(""), true, 72.0), HoldAction::HoldSoft);
    assert_eq!(hold_action(&hard, Some(&landed), None, true, 72.0), HoldAction::HoldHard);
}

#[test]
fn a_moved_predecessor_head_voids_the_hold_for_replanning() {
    let m = marker_after(5, Some("pass"));
    let moved = pred_open_at_head(&format!("{:040x}", 12345));
    assert_eq!(hold_action(&m, Some(&moved), live(&m), true, 72.0), HoldAction::VoidAndReplan);
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
    assert_eq!(hold_action(&soft, Some(&quiet), live(&soft), true, 72.0), HoldAction::Expire);
    // Soft but NOT approved ⇒ hold (nothing mergeable is starved).
    assert_eq!(hold_action(&soft, Some(&quiet), live(&soft), false, 72.0), HoldAction::HoldSoft);
    // Hard + approved + quiet ⇒ hold (a semantic dependency never expires
    // into merge permission — the #9063 non-negotiable).
    assert_eq!(hold_action(&hard, Some(&quiet), live(&hard), true, 72.0), HoldAction::HoldHard);
    // Soft + approved + quiet but within the bound ⇒ hold.
    assert_eq!(
        hold_action(&soft, Some(&quiet), live(&soft), true, f64::INFINITY),
        HoldAction::HoldSoft
    );
    // Predecessor with unknown freshness ⇒ hold.
    let freshless = PredecessorState {
        updated_at: None,
        ..quiet.clone()
    };
    assert_eq!(
        hold_action(&soft, Some(&freshless), live(&soft), true, 72.0),
        HoldAction::HoldSoft
    );
}

// --- Deferral -----------------------------------------------------------

#[test]
fn deferral_applies_only_to_clean_in_flight_ordering() {
    let m = marker_after(5, Some("pass"));
    assert!(defer_base_repair(&m, &pred_open_at_head(&m.pred_head), &m.follower_head));
    // Moved head ⇒ replan territory: flag the conflict, don't hide it.
    assert!(!defer_base_repair(
        &m,
        &pred_open_at_head(&format!("{:040x}", 777)),
        &m.follower_head
    ));
    // Landed ⇒ flag: the follower must now be repaired against the new base.
    let landed = PredecessorState {
        merged: true,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert!(!defer_base_repair(&m, &landed, &m.follower_head));
    // Dissolved ⇒ flag.
    let dissolved = PredecessorState {
        merged: false,
        open: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: None,
    };
    assert!(!defer_base_repair(&m, &dissolved, &m.follower_head));
}

#[test]
fn a_moved_follower_head_never_defers_a_repair() {
    // The review-conflict pass found the follower conflicting at its LIVE
    // head; a marker pinned to an older follower tree must not defer that
    // repair even though the predecessor is still in flight at its pin.
    let m = marker_after(5, Some("pass"));
    let in_flight = pred_open_at_head(&m.pred_head);
    assert!(defer_base_repair(&m, &in_flight, &m.follower_head));
    assert!(!defer_base_repair(&m, &in_flight, &format!("{:040x}", 424242)));
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
    assert_eq!(r.skipped_held, 0);
}

// --- Judge-requested coverage (re-review of #9707) ----------------------

#[test]
fn a_three_node_cycle_skips_the_component() {
    // 1→2→3→1 via manual markers. Kahn's detection is general; this pins
    // that a cycle longer than the trivial 2-cycle is also skipped whole.
    let prs = [
        pr(1, "2026-01-01T00:00:00Z"),
        pr(2, "2026-01-02T00:00:00Z"),
        pr(3, "2026-01-03T00:00:00Z"),
    ];
    let f = files(&[(1, &["s.rs"]), (2, &["s.rs"]), (3, &["s.rs"])]);
    let mut markers = NO_MARKERS;
    for (follower, after) in [(1, 2), (2, 3), (3, 1)] {
        markers.insert(
            follower,
            SequenceMarker {
                after,
                pred_head: format!("{:040x}", after),
                follower_head: format!("{:040x}", follower),
                plan: "manual".into(),
                source: None,
            },
        );
    }
    assert!(plan_repo(&prs, &f, &markers).is_empty(), "3-cycle ⇒ no plan");
}

#[test]
fn a_diamond_orders_both_paths_without_dropping_a_member() {
    // 4 depends on 2 AND 3; both depend on 1. Every member placed exactly
    // once, 4 last, 1 first — the two middle members in age order.
    let prs = [
        pr(1, "2026-01-01T00:00:00Z"),
        pr(2, "2026-01-02T00:00:00Z"),
        pr(3, "2026-01-03T00:00:00Z"),
        pr(4, "2026-01-04T00:00:00Z"),
    ];
    let f = files(&[
        (1, &["s.rs"]),
        (2, &["s.rs"]),
        (3, &["s.rs"]),
        (4, &["s.rs"]),
    ]);
    let mut markers = NO_MARKERS;
    for (follower, after) in [(2, 1), (3, 1), (4, 2), (4, 3)] {
        markers.insert(
            follower,
            SequenceMarker {
                after,
                pred_head: format!("{:040x}", after),
                follower_head: format!("{:040x}", follower),
                plan: "manual".into(),
                source: None,
            },
        );
    }
    let groups = plan_repo(&prs, &f, &markers);
    assert_eq!(groups.len(), 1, "{groups:?}");
    let g = &groups[0];
    assert_eq!(g.order.first(), Some(&1));
    assert_eq!(g.order.last(), Some(&4));
    assert_eq!(g.order.len(), 4, "every member placed: {:?}", g.order);
    // Edges are a CHAIN over the placed order (4 has one predecessor, not two).
    assert_eq!(g.edges.len(), 3);
}

#[test]
fn a_follower_marker_disagreeing_with_age_order_wins() {
    // Without the marker, ages put #2 before #1. The trusted marker says
    // #2 lands AFTER #1 — the constraint beats the age default.
    let prs = [pr(1, "2026-01-02T00:00:00Z"), pr(2, "2026-01-01T00:00:00Z")];
    let third = pr(3, "2026-01-03T00:00:00Z");
    let prs = [prs[0].clone(), prs[1].clone(), third];
    let f = files(&[(1, &["s.rs"]), (2, &["s.rs"]), (3, &["other.rs"])]);
    let mut markers = NO_MARKERS;
    markers.insert(
        2,
        SequenceMarker {
            after: 1,
            pred_head: format!("{:040x}", 1),
            follower_head: format!("{:040x}", 2),
            plan: "manual".into(),
            source: None,
        },
    );
    let groups = plan_repo(&prs, &f, &markers);
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0].order, vec![1, 2], "the marker overrides age order");
}

#[test]
fn a_new_pr_chains_behind_a_held_predecessor() {
    // The Phase-2 membership fix (Judge re-review of #9707): #1 is already
    // held (loom:sequenced with a plan marker), #2 arrives later touching
    // the same files. #2 must be planned AFTER #1 — the hold's existence
    // must not make the overlap invisible for the life of the hold.
    let mut held = pr(1, "2026-01-01T00:00:00Z");
    held.labels.push(SEQUENCE_LABEL.to_string());
    let fresh = pr(2, "2026-01-05T00:00:00Z");
    let prs = [held.clone(), fresh, pr(3, "2026-01-03T00:00:00Z")];
    let f = files(&[(1, &["s.rs"]), (2, &["s.rs"]), (3, &["other.rs"])]);
    let mut markers = NO_MARKERS;
    markers.insert(
        1,
        SequenceMarker {
            after: 99,
            pred_head: format!("{:040x}", 99),
            follower_head: format!("{:040x}", 1),
            plan: "seq-old".into(),
            source: Some("pass".into()),
        },
    );
    let groups = plan_repo(&prs, &f, &markers);
    assert_eq!(groups.len(), 1, "{groups:?}");
    let g = &groups[0];
    // The held #1 participates in the order (its marker points out-of-group,
    // which order_component ignores; it is still a group member and lands
    // first by age), and the fresh #2 is planned after it.
    assert_eq!(g.order, vec![1, 2], "{g:?}");
    assert_eq!(g.edges.len(), 1);
    assert_eq!((g.edges[0].follower, g.edges[0].after), (2, 1));
}

// ---- #4429 follow-up: reuse of the review-conflict pass's listing ----

/// `n` open REST rows, newest first (as the listing returns them).
fn rest_rows(n: u32) -> Vec<super::super::open_pr_listing::RestPull> {
    use super::super::open_pr_listing::test_support::{listing, row};
    let rows: Vec<_> = (1..=n).rev().map(|i| row(i, &[])).collect();
    crate::forge_pull_listing::parse_rest_pulls(&listing(&rows)).unwrap()
}

/// The planner's cut of the REST listing is the same 100 newest PRs the old
/// `--limit` listing returned, with every field mapped.
#[test]
fn sequence_prs_maps_rows_and_truncates_to_own_limit() {
    let got = sequence_prs(&rest_rows(150));
    assert_eq!(got.len(), super::super::MAX_ISSUES_PER_WORKSPACE as usize);
    assert_eq!(got[0].number, 150, "newest first");
    assert_eq!(got[0].head_sha.as_deref(), Some(format!("{:040x}", 150).as_str()));
    assert_eq!(
        (got[0].head_ref.as_str(), got[0].base_ref.as_str()),
        ("feature/issue-150", "main")
    );
    assert_eq!(got[0].created_at, "2026-10-01T00:00:00Z");
    assert!(!got[0].draft);
}

#[cfg(unix)]
fn logging_gh(dir: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("fake-gh-seq.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\necho '[]'\n",
        log.display()
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    bin
}

/// A shared listing replaces this pass's own listing read; without one the
/// pass lists for itself — through the REST listing, never `gh pr list`.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_shared_listing_saves_the_pass_its_own_listing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let prev = std::env::var(MERGE_SEQUENCE_ENABLED_ENV).ok();
    std::env::remove_var(MERGE_SEQUENCE_ENABLED_ENV);
    let one = rest_rows(1);
    let cases: [(Option<&[_]>, bool); 2] = [(Some(one.as_slice()), false), (None, true)];
    for (i, (prefetched, expect_list)) in cases.into_iter().enumerate() {
        let log = dir.path().join(format!("gh-{i}.log"));
        std::fs::write(&log, "").unwrap();
        let gh = logging_gh(dir.path(), &log);
        let stats = reconcile_merge_sequences_with(&gh, &root, prefetched);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.contains("pulls?state=open"), expect_list, "case {i}: {calls}");
        assert!(!calls.contains("pr list"), "case {i}: {calls}");
        if !expect_list {
            assert_eq!(stats.checked, 1, "case {i}");
        }
    }
    if let Some(v) = prev {
        std::env::set_var(MERGE_SEQUENCE_ENABLED_ENV, v);
    }
}

// ---- #10089: Phase 2 reuses Phase 1's holder comment reads ----

/// A listing of `n` open PRs; the numbers in `held` carry the hold label.
fn listing_with_holds(n: u32, held: &[u32]) -> Vec<super::super::open_pr_listing::RestPull> {
    use super::super::open_pr_listing::test_support::{listing, row};
    let rows: Vec<_> = (1..=n)
        .map(|i| {
            let labels: &[&str] = if held.contains(&i) {
                &[SEQUENCE_LABEL]
            } else {
                &[]
            };
            row(i, labels)
                .created(&format!("2026-10-02T00:00:0{i}Z"))
                .updated("2026-10-02T00:00:00Z")
        })
        .collect();
    crate::forge_pull_listing::parse_rest_pulls(&listing(&rows)).unwrap()
}

/// Each holder's comments are walked ONCE per tick: Phase 1 reads them to
/// evaluate the hold and Phase 2's marker scan reuses that read instead of
/// re-walking every holder (previously two paginated walks per holder).
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_tick_walks_each_holders_comments_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let prev = std::env::var(MERGE_SEQUENCE_ENABLED_ENV).ok();
    std::env::remove_var(MERGE_SEQUENCE_ENABLED_ENV);
    let log = dir.path().join("gh.log");
    std::fs::write(&log, "").unwrap();
    let gh = logging_gh(dir.path(), &log);
    let listing = listing_with_holds(3, &[2, 3]);

    let stats = reconcile_merge_sequences_with(&gh, &root, Some(&listing));

    let calls = std::fs::read_to_string(&log).unwrap();
    let walks = |n: u32| {
        calls
            .lines()
            .filter(|l| l.contains(&format!("issues/{n}/comments")))
            .count()
    };
    assert_eq!(stats.checked, 3, "{calls}");
    assert_eq!((walks(2), walks(3)), (1, 1), "one comments walk per holder:\n{calls}");
    assert_eq!(walks(1), 0, "a non-holder's comments are never walked:\n{calls}");
    assert!(
        calls
            .lines()
            .all(|l| !l.contains("/comments") || l.contains("per_page=100")),
        "every comments walk pages at 100:\n{calls}"
    );
    if let Some(v) = prev {
        std::env::set_var(MERGE_SEQUENCE_ENABLED_ENV, v);
    }
}
