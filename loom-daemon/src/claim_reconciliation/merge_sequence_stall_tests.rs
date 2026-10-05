//! Fixtures for #10060: direct-overlap edges, star-first ordering, and the
//! stalled-head release + escalation.

use std::collections::BTreeMap;

use super::super::{
    hold_action, plan_group, plan_repo_with, EdgeReason, SequenceGroup, SEQUENCE_LABEL,
};
use super::*;
use crate::work_finder::OPERATOR_PRIORITY_LABEL;

const NOW: &str = "2026-10-03T12:00:00Z";
const BOUND: f64 = 12.0;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(NOW)
        .unwrap()
        .with_timezone(&Utc)
}

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

/// An open PR created `created`, last active `updated`.
fn pr(number: u32, created: &str, updated: &str, labels: &[&str]) -> SequencePr {
    SequencePr {
        number,
        created_at: created.to_string(),
        updated_at: updated.to_string(),
        head_sha: Some(sha(number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

/// A fresh PR (active one hour before [`NOW`]).
fn fresh(number: u32, created: &str, labels: &[&str]) -> SequencePr {
    pr(number, created, "2026-10-03T11:00:00Z", labels)
}

fn files(entries: &[(u32, &[&str])]) -> BTreeMap<u32, BTreeSet<String>> {
    entries
        .iter()
        .map(|(n, paths)| (*n, paths.iter().map(|p| (*p).to_string()).collect()))
        .collect()
}

fn edges(g: &SequenceGroup) -> Vec<(u32, u32)> {
    g.edges.iter().map(|e| (e.follower, e.after)).collect()
}

const NONE: BTreeMap<u32, SequenceMarker> = BTreeMap::new();

// --- Slice 1: direct-overlap DAG ----------------------------------------

#[test]
fn transitive_only_members_get_no_edge() {
    // A–B share x.rs, B–C share y.rs, A and C share nothing. One component,
    // but C waits only for B, and nothing links A and C directly.
    let prs = [
        fresh(1, "2026-10-01T00:00:00Z", &[]),
        fresh(2, "2026-10-01T01:00:00Z", &[]),
        fresh(3, "2026-10-01T02:00:00Z", &[]),
    ];
    let f = files(&[(1, &["x.rs"]), (2, &["x.rs", "y.rs"]), (3, &["y.rs"])]);
    let groups = plan_repo_with(&prs, &f, &NONE, &BTreeSet::new());
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0].order, vec![1, 2, 3]);
    assert_eq!(edges(&groups[0]), vec![(2, 1), (3, 2)]);
}

#[test]
fn disjoint_members_of_one_component_are_never_ordered_against_each_other() {
    // The #10053/#10056 shape: a hub PR shares one file with each, the two
    // share none. #56 must wait for the hub, not for #53, and no
    // `SharedFiles` edge may join two PRs with disjoint file sets.
    let prs = [
        fresh(10, "2026-10-01T00:00:00Z", &[]),
        fresh(53, "2026-10-02T00:00:00Z", &[]),
        fresh(56, "2026-10-02T01:00:00Z", &[]),
    ];
    let f = files(&[
        (
            10,
            &[
                "defaults/docs/label-state-machine.md",
                "loom-daemon/src/forge_check_branch.rs",
            ],
        ),
        (
            53,
            &[
                "defaults/docs/label-state-machine.md",
                "defaults/scripts/check-labels-drift.sh",
            ],
        ),
        (
            56,
            &[
                "defaults/scripts/sweep-lease-fence.sh",
                "loom-daemon/src/forge_check_branch.rs",
            ],
        ),
    ]);
    let g = &plan_repo_with(&prs, &f, &NONE, &BTreeSet::new())[0];
    assert_eq!(g.order, vec![10, 53, 56]);
    assert_eq!(edges(g), vec![(53, 10), (56, 10)]);
    for e in &g.edges {
        assert_eq!(e.reason, EdgeReason::SharedFiles);
        let (a, b) = (&f[&e.after], &f[&e.follower]);
        assert!(a.iter().any(|p| b.contains(p)), "edge {e:?} claims overlap that does not exist");
    }
}

#[test]
fn a_follower_waits_for_its_nearest_direct_overlap() {
    // #4 overlaps #1 and #2 (not #3): it waits for #2, the latest of them.
    let order = [1, 2, 3, 4];
    let by: BTreeMap<u32, SequencePr> = order
        .iter()
        .map(|n| (*n, fresh(*n, "2026-10-01T00:00:00Z", &[])))
        .collect();
    let f = files(&[
        (1, &["a", "z"]),
        (2, &["a", "b"]),
        (3, &["b"]),
        (4, &["z", "b2", "a"]),
    ]);
    let g = plan_group("seq-t", &order, &by, &f);
    assert_eq!(edges(&g), vec![(2, 1), (3, 2), (4, 2)]);
}

/// Golden: the pre-#10060 planner chained `order.windows(2)` and ordered the
/// placeable set oldest-first. A group where every pair overlaps, nothing is
/// starred and nothing is stalled must plan exactly that.
#[test]
fn a_ready_group_with_no_stalled_head_is_unchanged() {
    let prs = [
        fresh(4, "2026-10-01T03:00:00Z", &["loom:pr"]),
        fresh(2, "2026-10-01T01:00:00Z", &["loom:pr"]),
        fresh(1, "2026-10-01T00:00:00Z", &["loom:pr"]),
        fresh(3, "2026-10-01T02:00:00Z", &["loom:pr"]),
    ];
    let f = files(&[(1, &["s"]), (2, &["s"]), (3, &["s"]), (4, &["s"])]);
    let stalled = stalled_for_ordering(&prs, now(), BOUND);
    assert!(stalled.is_empty());
    let groups = plan_repo_with(&prs, &f, &NONE, &stalled);
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert_eq!(g.order, vec![1, 2, 3, 4], "oldest-first, as before");
    let legacy: Vec<(u32, u32)> = g.order.windows(2).map(|w| (w[1], w[0])).collect();
    assert_eq!(edges(g), legacy, "chain edges identical to the windows(2) planner");
    assert!(g.edges.iter().all(|e| e.reason == EdgeReason::SharedFiles));

    // Hold actions: with no stalled head, the stall-aware decision is the
    // old decision for every input shape.
    let soft = marker(1, Some("pass"), "seq-x");
    let hard = marker(1, None, "manual");
    let quiet = pred(1, Some("2026-01-01T00:00:00Z"));
    let busy = pred(1, Some(&Utc::now().to_rfc3339()));
    let moved = sha(4242);
    for m in [&soft, &hard] {
        for p in [Some(&quiet), Some(&busy), None] {
            for fh in [Some(m.follower_head.as_str()), Some(moved.as_str()), None] {
                for approved in [true, false] {
                    assert_eq!(
                        hold_action_with_stall(m, p, fh, approved, 72.0, None),
                        hold_action(m, p, fh, approved, 72.0),
                        "{m:?} {p:?} {fh:?} {approved}"
                    );
                }
            }
        }
    }
}

// --- Slice 2: star-first ordering ---------------------------------------

#[test]
fn a_starred_pr_is_ordered_ahead_of_an_unstarred_predecessor() {
    let old = fresh(1, "2026-10-01T00:00:00Z", &[]);
    let mid = fresh(2, "2026-10-01T01:00:00Z", &[]);
    let star = fresh(3, "2026-10-01T02:00:00Z", &[OPERATOR_PRIORITY_LABEL]);
    let f = files(&[(1, &["s"]), (2, &["s"]), (3, &["s"])]);
    let prs = [old.clone(), mid.clone(), star.clone()];
    let g = &plan_repo_with(&prs, &f, &NONE, &BTreeSet::new())[0];
    assert_eq!(g.order, vec![3, 1, 2], "star first, then oldest-first");
    assert_eq!(edges(g), vec![(1, 3), (2, 1)]);

    // No star anywhere ⇒ the old oldest-first order.
    let plain = [
        old.clone(),
        mid.clone(),
        fresh(3, "2026-10-01T02:00:00Z", &[]),
    ];
    assert_eq!(plan_repo_with(&plain, &f, &NONE, &BTreeSet::new())[0].order, vec![1, 2, 3]);

    // A constraint edge beats the star: a trusted marker puts #3 after #1.
    let mut markers = NONE;
    markers.insert(3, marker_between(3, 1));
    assert_eq!(plan_repo_with(&prs, &f, &markers, &BTreeSet::new())[0].order, vec![1, 3, 2]);
}

#[test]
fn a_stacked_base_beats_the_star() {
    let base = fresh(1, "2026-10-01T00:00:00Z", &[]);
    let mut stacked = fresh(2, "2026-10-01T01:00:00Z", &[OPERATOR_PRIORITY_LABEL]);
    stacked.base_ref = base.head_ref.clone();
    let other = fresh(3, "2026-10-01T02:00:00Z", &[]);
    let f = files(&[(1, &["s"]), (2, &["s"]), (3, &["s"])]);
    let g = &plan_repo_with(&[base, stacked, other], &f, &NONE, &BTreeSet::new())[0];
    assert_eq!(g.order, vec![1, 2, 3]);
    assert_eq!(g.edges[0].reason, EdgeReason::StackedBase);
}

// --- Slice 3: stalled heads ---------------------------------------------

fn marker(after: u32, source: Option<&str>, plan: &str) -> SequenceMarker {
    SequenceMarker {
        after,
        pred_head: sha(after),
        follower_head: sha(99),
        plan: plan.into(),
        source: source.map(str::to_string),
    }
}

fn marker_between(follower: u32, after: u32) -> SequenceMarker {
    SequenceMarker {
        follower_head: sha(follower),
        ..marker(after, None, "manual")
    }
}

/// The predecessor read from the pulls API: open at its recorded head.
fn pred(n: u32, updated: Option<&str>) -> PredecessorState {
    PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(sha(n)),
        updated_at: updated.map(str::to_string),
    }
}

/// Phase 1's decision for a follower (approved) behind `head`.
fn decide(m: &SequenceMarker, head: &SequencePr, approved: bool) -> HoldAction {
    let cause = stall_cause(head, now(), BOUND);
    // The pulls read is "now" fresh so the 72 h expiry never fires here.
    let p = pred(head.number, Some(&Utc::now().to_rfc3339()));
    hold_action_with_stall(m, Some(&p), Some(&m.follower_head), approved, 72.0, cause.as_ref())
}

#[test]
fn a_human_held_head_past_the_bound_releases_soft_holds_only() {
    // (a) #9745-shaped: the head carries `loom:operator`, quiet 2 days.
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:pr", "loom:operator"]);
    assert_eq!(
        stall_cause(&head, now(), BOUND),
        Some(StallCause::HumanHold("loom:operator".into()))
    );
    let soft = marker(5, Some("pass"), "seq-a");
    assert_eq!(decide(&soft, &head, true), HoldAction::ReleaseStalled);
    // Not approved ⇒ nothing mergeable is starved: hold.
    assert_eq!(decide(&soft, &head, false), HoldAction::HoldSoft);
    // Hard (human-authored, no source=) ⇒ never released.
    assert_eq!(decide(&marker(5, None, "manual"), &head, true), HoldAction::HoldHard);
    // Consolidation reservations keep ADR-0023's 72 h contract.
    assert_eq!(
        decide(&marker(5, Some("pass"), "cons-ab12cd34"), &head, true),
        HoldAction::HoldSoft
    );
    // Within the bound ⇒ hold.
    let recent = pr(5, "2026-09-30T00:00:00Z", "2026-10-03T06:00:00Z", &["loom:operator"]);
    assert_eq!(stall_cause(&recent, now(), BOUND), None);
    assert_eq!(decide(&soft, &recent, true), HoldAction::HoldSoft);
}

#[test]
fn a_no_verdict_head_past_the_bound_releases_soft_holds() {
    // (b) #9819-shaped: a fork PR whose CI is `action_required`, so it never
    // gets a verdict — `loom:review-requested`, no `loom:pr`.
    let head = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &["loom:review-requested"]);
    assert_eq!(stall_cause(&head, now(), BOUND), Some(StallCause::NoVerdict));
    let soft = marker(7, Some("pass"), "seq-b");
    assert_eq!(decide(&soft, &head, true), HoldAction::ReleaseStalled);
    assert_eq!(decide(&marker(7, None, "manual"), &head, true), HoldAction::HoldHard);
    // Within the bound ⇒ HoldSoft.
    let recent = pr(7, "2026-09-30T00:00:00Z", "2026-10-03T08:00:00Z", &["loom:review-requested"]);
    assert_eq!(decide(&soft, &recent, true), HoldAction::HoldSoft);
    // An approved, quiet head is not stalled: Champion can merge it.
    let approved = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &["loom:pr"]);
    assert_eq!(decide(&soft, &approved, true), HoldAction::HoldSoft);
    // Unparseable freshness is never stalled.
    let unknown = pr(7, "2026-09-30T00:00:00Z", "yesterday", &[]);
    assert_eq!(stall_cause(&unknown, now(), BOUND), None);
}

#[test]
fn an_unreadable_or_moved_predecessor_is_never_stall_released() {
    let head = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &[]);
    let cause = stall_cause(&head, now(), BOUND);
    let soft = marker(7, Some("pass"), "seq-b");
    let live = Some(soft.follower_head.as_str());
    assert_eq!(
        hold_action_with_stall(&soft, None, live, true, 72.0, cause.as_ref()),
        HoldAction::HoldSoft,
        "fail closed"
    );
    let moved = PredecessorState {
        head_sha: Some(sha(777)),
        ..pred(7, None)
    };
    assert_eq!(
        hold_action_with_stall(&soft, Some(&moved), live, true, 72.0, cause.as_ref()),
        HoldAction::VoidAndReplan
    );
}

#[test]
fn a_stalled_no_verdict_member_is_planned_last_so_released_work_lands_first() {
    // After the release, the next tick re-plans: the stalled head must not
    // pull the approved follower straight back behind it.
    let head = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &["loom:review-requested"]);
    let ready = fresh(8, "2026-10-01T00:00:00Z", &["loom:pr"]);
    let other = fresh(9, "2026-10-01T01:00:00Z", &[]);
    let prs = [head, ready, other];
    let f = files(&[(7, &["s"]), (8, &["s"]), (9, &["s"])]);
    let stalled = stalled_for_ordering(&prs, now(), BOUND);
    assert_eq!(stalled, BTreeSet::from([7]));
    let g = &plan_repo_with(&prs, &f, &NONE, &stalled)[0];
    assert_eq!(g.order, vec![8, 9, 7]);
    assert_eq!(edges(g), vec![(9, 8), (7, 9)]);
}

#[test]
fn a_stalled_chain_escalates_once_naming_head_action_and_held_prs() {
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:operator"]);
    let cause = stall_cause(&head, now(), BOUND).unwrap();
    let mut ledger = StallLedger::default();
    ledger.record(&head, &cause, 36.0, 11, true);
    ledger.record(&head, &cause, 36.0, 12, false);
    ledger.record(&head, &cause, 36.0, 12, false);
    assert_eq!(ledger.chains.len(), 1, "one chain per head");
    let chain = &ledger.chains[0];
    assert_eq!((chain.released.clone(), chain.held.clone()), (vec![11], vec![12]));
    assert!(chain.needs_escalation());
    let body = chain.comment_body(BOUND);
    assert!(body.starts_with(&escalation_marker(&chain.key())), "{body}");
    assert!(body.contains("**Operator needed**"), "{body}");
    assert!(body.contains("merge decision on #5"), "{body}");
    assert!(body.contains("#12") && body.contains("#11"), "{body}");
    assert!(body.contains("36h"), "{body}");
    assert!(chain.key().starts_with("sequence-stall:5:"));

    // A no-verdict head whose soft followers were all released is re-planned
    // rather than escalated (a comment would reset its quiet clock).
    let fork = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &[]);
    let mut l2 = StallLedger::default();
    l2.record(&fork, &StallCause::NoVerdict, 36.0, 8, true);
    assert!(!l2.chains[0].needs_escalation());
    l2.record(&fork, &StallCause::NoVerdict, 36.0, 9, false);
    assert!(l2.chains[0].needs_escalation(), "a hard-held follower still escalates");
    assert!(l2.chains[0].comment_body(BOUND).contains("action_required"));
}

#[cfg(unix)]
fn fake_gh(dir: &std::path::Path, marker: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let posted = dir.join("posted.flag");
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-stall.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$1 $2\" >> \"{log}\"\n\
         if [ \"$1\" = api ]; then\n  if [ -f \"{posted}\" ]; then\n    \
         echo '[{{\"body\":\"{marker}\",\"author_association\":\"OWNER\",\"user\":{{\"login\":\"op\",\"type\":\"User\"}}}}]'\n  \
         else echo '[]'; fi\nelif [ \"$1 $2\" = \"pr comment\" ]; then touch \"{posted}\"; fi\n",
        log = log.display(),
        posted = posted.display(),
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    (bin, log)
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn the_second_tick_posts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:operator"]);
    let mut ledger = StallLedger::default();
    ledger.record(&head, &StallCause::HumanHold("loom:operator".into()), 36.0, 11, false);
    let chain = &ledger.chains[0];
    let (gh, log) = fake_gh(dir.path(), &escalation_marker(&chain.key()));
    assert!(escalate(&gh, &root, chain, BOUND).unwrap(), "first tick posts");
    assert!(!escalate(&gh, &root, chain, BOUND).unwrap(), "second tick: already_posted");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("pr comment").count(), 1, "{calls}");
    // A held PR label alone never counts as an escalation.
    assert!(!already_escalated(&[SEQUENCE_LABEL.to_string()], chain));
}
