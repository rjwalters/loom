//! Issue #11853: the landed pre-filter — the closes-graph verdict, the
//! Curator routing, the verdict cache, and the tick seam.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::json;

use super::super::*;
use super::*;
use crate::types::{QueueDisposition, ReadyQueueRow};

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// The repository every fixture issue lives in.
const REPO: &str = "o/r";

fn pr(number: u32, state: &str, merged_at: Option<&str>) -> Value {
    pr_in(REPO, number, state, merged_at)
}

fn pr_in(repo: &str, number: u32, state: &str, merged_at: Option<&str>) -> Value {
    json!({
        "number": number,
        "state": state,
        "mergedAt": merged_at,
        "repository": { "nameWithOwner": repo },
    })
}

fn alias(state: &str, reopened: Option<&str>, prs: &[Value], total: Option<u64>) -> Value {
    let nodes: Vec<Value> = reopened
        .map(|at| json!({ "createdAt": at }))
        .into_iter()
        .collect();
    json!({
        "state": state,
        "timelineItems": { "nodes": nodes },
        "closedByPullRequestsReferences": {
            "totalCount": total.unwrap_or(prs.len() as u64),
            "nodes": prs,
        },
    })
}

const T1: &str = "2026-10-10T10:00:00Z";
const T2: &str = "2026-10-11T10:00:00Z";

/// A PR that closes the issue (a closing keyword or a closing link puts it
/// in the closes-graph) has merged, and the issue is still open: landed.
#[test]
fn a_merged_closing_pr_on_an_open_issue_is_landed() {
    let a = alias("OPEN", None, &[pr(40, "MERGED", Some(T1))], None);
    assert_eq!(alias_verdict(&a, REPO), Some(Some(40)));
    // The latest of several merges names the landing.
    let a = alias("OPEN", None, &[pr(41, "MERGED", Some(T2)), pr(40, "MERGED", Some(T1))], None);
    assert_eq!(alias_verdict(&a, REPO), Some(Some(41)));
}

/// A merged `Part of #N` / `Refs #N` PR is not in the closes-graph, which is
/// exactly how GitHub tells it from a closing one: not landed, and the issue
/// keeps its stage. Neither is an open or closed-unmerged closing PR.
#[test]
fn a_part_of_merge_or_an_unmerged_closing_pr_is_not_landed() {
    let part_of_only = alias("OPEN", None, &[], None);
    assert_eq!(alias_verdict(&part_of_only, REPO), Some(None));
    let open = alias("OPEN", None, &[pr(40, "OPEN", None)], None);
    assert_eq!(alias_verdict(&open, REPO), Some(None));
    let abandoned = alias("OPEN", None, &[pr(40, "CLOSED", None)], None);
    assert_eq!(alias_verdict(&abandoned, REPO), Some(None));
}

/// A merge older than the issue's last reopen is a person asking for more
/// work: not landed. A merge after it is.
#[test]
fn a_merge_before_the_last_reopen_is_not_landed() {
    let before = alias("OPEN", Some(T2), &[pr(40, "MERGED", Some(T1))], None);
    assert_eq!(alias_verdict(&before, REPO), Some(None));
    let after = alias("OPEN", Some(T1), &[pr(41, "MERGED", Some(T2))], None);
    assert_eq!(alias_verdict(&after, REPO), Some(Some(41)));
}

/// No verdict on a guess: a truncated graph without a merge in it, or a
/// missing alias. A closed issue is not this pre-filter's to refuse.
#[test]
fn a_truncated_graph_or_a_missing_alias_is_no_verdict() {
    let truncated = alias("OPEN", None, &[pr(40, "OPEN", None)], Some(30));
    assert_eq!(alias_verdict(&truncated, REPO), None);
    let closed = alias("CLOSED", None, &[pr(40, "MERGED", Some(T1))], None);
    assert_eq!(alias_verdict(&closed, REPO), Some(None));

    let body = json!({ "data": { "repository": {
        "i1": alias("OPEN", None, &[pr(40, "MERGED", Some(T1))], None),
        "i2": alias("OPEN", None, &[], None),
        "i3": null,
    } } })
    .to_string();
    let parsed = parse_landed(&body, &[1, 2, 3, 4], REPO).unwrap();
    assert_eq!(parsed.get(&1), Some(&Some(40)));
    assert_eq!(parsed.get(&2), Some(&None));
    assert!(!parsed.contains_key(&3) && !parsed.contains_key(&4));
    assert_eq!(parse_landed(r#"{"errors":[{"message":"x"}]}"#, &[1], REPO), None);
}

/// A PR merged in ANOTHER repository whose body says `Closes o/r#N` is in
/// the issue's closes-graph, but anyone can write that line in a repo of
/// their own: it is not landed (#11899). Beside a same-repo merge, only the
/// same-repo PR names the landing, even when the cross-repo one is later.
/// A PR whose repository is missing is not trusted to be this one. The
/// repository match ignores case, as GitHub's names do.
#[test]
fn a_cross_repo_merged_closing_pr_is_not_landed() {
    let cross = alias("OPEN", None, &[pr_in("outsider/fork", 40, "MERGED", Some(T1))], None);
    assert_eq!(alias_verdict(&cross, REPO), Some(None));
    let both = alias(
        "OPEN",
        None,
        &[
            pr_in("outsider/fork", 42, "MERGED", Some(T2)),
            pr(41, "MERGED", Some(T1)),
        ],
        None,
    );
    assert_eq!(alias_verdict(&both, REPO), Some(Some(41)));
    let unnamed = alias(
        "OPEN",
        None,
        &[json!({ "number": 40, "state": "MERGED", "mergedAt": T1 })],
        None,
    );
    assert_eq!(alias_verdict(&unnamed, REPO), Some(None));
    let cased = alias("OPEN", None, &[pr_in("O/R", 40, "MERGED", Some(T1))], None);
    assert_eq!(alias_verdict(&cased, REPO), Some(Some(40)));

    let body = json!({ "data": { "repository": { "i1": cross } } }).to_string();
    assert_eq!(parse_landed(&body, &[1], REPO).unwrap().get(&1), Some(&None));
}

#[test]
fn the_query_asks_for_closed_prs_and_the_last_reopen() {
    let q = landed_query("o", "r", &[7, 9]);
    assert!(q.contains("i7: issue(number: 7)") && q.contains("i9: issue(number: 9)"), "{q}");
    assert!(q.contains("includeClosedPrs: true"), "{q}");
    assert!(q.contains("REOPENED_EVENT") && q.contains("mergedAt"), "{q}");
    assert!(q.contains("repository { nameWithOwner }"), "{q}");
    // #12251: the merged PRs of each issue's Builder branch, and the default
    // branch they must target.
    assert!(
        q.contains("b7: pullRequests(headRefName: \"feature/issue-7\", states: [MERGED]"),
        "{q}"
    );
    assert!(q.contains("b9: pullRequests(headRefName: \"feature/issue-9\""), "{q}");
    assert!(q.contains("defaultBranchRef { name }"), "{q}");
    assert!(
        q.contains("baseRefName") && q.contains("headRepository { nameWithOwner }"),
        "{q}"
    );
}

// ---------------------------------------------------------------------------
// The Builder-branch landing (#12251)
// ---------------------------------------------------------------------------

fn branch_pr(number: u32, merged_at: &str, base: &str, head_repo: &str) -> Value {
    json!({
        "number": number,
        "mergedAt": merged_at,
        "baseRefName": base,
        "headRepository": { "nameWithOwner": head_repo },
    })
}

fn branch_prs(prs: &[Value]) -> Value {
    json!({ "nodes": prs })
}

/// A PR built on `feature/issue-N`, merged into the default branch with no
/// closing keyword, lands issue N: it is not in the closes-graph, but it
/// finished the work. A merge into another branch, a head in another
/// repository, or a merge older than the last reopen does not count.
#[test]
fn a_merged_builder_branch_pr_lands_the_issue() {
    let open = alias("OPEN", None, &[], None);
    let prs = branch_prs(&[branch_pr(40, T1, "main", REPO)]);
    assert_eq!(branch_landing(&open, &prs, "main", REPO), Some(40));
    let latest = branch_prs(&[
        branch_pr(40, T1, "main", REPO),
        branch_pr(41, T2, "main", REPO),
    ]);
    assert_eq!(branch_landing(&open, &latest, "main", REPO), Some(41));

    let stacked = branch_prs(&[branch_pr(40, T1, "feature/issue-3", REPO)]);
    assert_eq!(branch_landing(&open, &stacked, "main", REPO), None, "not the default branch");
    let fork = branch_prs(&[branch_pr(40, T1, "main", "outsider/fork")]);
    assert_eq!(branch_landing(&open, &fork, "main", REPO), None, "a fork's branch");
    let reopened = alias("OPEN", Some(T2), &[], None);
    assert_eq!(branch_landing(&reopened, &prs, "main", REPO), None, "reopened after");
    let closed = alias("CLOSED", None, &[], None);
    assert_eq!(branch_landing(&closed, &prs, "main", REPO), None, "the 2.5 guard's");
    assert_eq!(branch_landing(&open, &prs, "", REPO), None, "no default branch read");
}

/// The answer's verdict: a closing merge names the landing; failing one, a
/// Builder-branch merge does. The branch read only adds a landing: a missing
/// branch alias keeps the closes-graph's verdict, verified or not.
#[test]
fn parse_landed_counts_a_builder_branch_merge_as_landed() {
    let body = json!({ "data": { "repository": {
        "defaultBranchRef": { "name": "main" },
        "i1": alias("OPEN", None, &[], None),
        "b1": branch_prs(&[branch_pr(40, T1, "main", REPO)]),
        "i2": alias("OPEN", None, &[pr(50, "MERGED", Some(T1))], None),
        "b2": branch_prs(&[branch_pr(51, T2, "main", REPO)]),
        "i3": alias("OPEN", None, &[], None),
        "b3": branch_prs(&[]),
        "i4": alias("OPEN", None, &[], None),
        "i5": alias("OPEN", None, &[pr(60, "OPEN", None)], Some(30)),
        "b5": branch_prs(&[branch_pr(61, T1, "main", REPO)]),
    } } })
    .to_string();
    let parsed = parse_landed(&body, &[1, 2, 3, 4, 5], REPO).unwrap();
    assert_eq!(parsed.get(&1), Some(&Some(40)), "branch-linked, no keyword");
    assert_eq!(parsed.get(&2), Some(&Some(50)), "the closing merge names it");
    assert_eq!(parsed.get(&3), Some(&None));
    assert_eq!(parsed.get(&4), Some(&None), "no branch alias: the closes-graph's verdict");
    assert_eq!(parsed.get(&5), Some(&Some(61)), "a truncated graph, but a branch merge");
}

/// #12251: the Builder spawn's check. A branch-linked landing is routed to
/// Curator and refuses the spawn, so the step draw has no Builder step on the
/// issue after it (its `loom:issue` is gone); an issue that has not landed,
/// or one Curator kept, spawns. Every spawn asks afresh.
#[test]
fn the_spawn_check_refuses_a_landed_issue_and_routes_it_to_curator() {
    let mut forge = FakeForge::default()
        .with_labels(5, &["loom:issue"])
        .with_labels(6, &["loom:issue"])
        .with_labels(7, &["loom:issue"]);
    forge.graph.insert(5, Some(40));
    forge.graph.insert(6, None);
    forge.graph.insert(7, Some(70));
    forge.marked.insert((7, 70));
    let mut cache = LandedCache::default();
    let now = Instant::now();

    assert_eq!(spawn_check(&mut forge, &mut cache, 5, now), Some(40));
    assert_eq!(forge.writes[0], "+5 loom:triage");
    assert_eq!(forge.writes[1], "-5 loom:issue");
    assert!(forge.writes[2].contains(&route_marker(40)), "{:?}", forge.writes);

    assert_eq!(spawn_check(&mut forge, &mut cache, 6, now), None, "not landed");
    assert_eq!(spawn_check(&mut forge, &mut cache, 7, now), None, "Curator kept it");
    assert_eq!(forge.writes.len(), 3, "{:?}", forge.writes);
    assert_eq!(spawn_check(&mut forge, &mut cache, 6, now), None);
    assert_eq!(forge.asked.len(), 4, "every spawn asks, fresh verdict or not");
}

/// The spawn check fails open: no answer is a spawn, and the backoff after an
/// unanswered query is honoured without asking.
#[test]
fn the_spawn_check_fails_open() {
    let mut forge = FakeForge::default().with_labels(5, &["loom:issue"]);
    let mut cache = LandedCache::default();
    let now = Instant::now();
    assert_eq!(spawn_check(&mut forge, &mut cache, 5, now), None, "no verdict");
    cache.failed_at = Some(now);
    forge.graph.insert(5, Some(40));
    assert_eq!(spawn_check(&mut forge, &mut cache, 5, now), None, "backing off");
    assert_eq!(forge.asked.len(), 1);
}

// ---------------------------------------------------------------------------
// Routing and the cache, against a fake forge
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeForge {
    /// The closes-graph answer: issue to merged closing PR (`None` = not
    /// landed). An issue absent here gets no verdict.
    graph: HashMap<u32, Option<u32>>,
    labels: HashMap<u32, Vec<String>>,
    /// Issues whose comments already carry the marker for this PR.
    marked: HashSet<(u32, u32)>,
    asked: Vec<Vec<u32>>,
    writes: Vec<String>,
    /// Every comment write fails (#11901).
    comment_fails: bool,
}

impl FakeForge {
    fn with_labels(mut self, issue: u32, labels: &[&str]) -> Self {
        self.labels
            .insert(issue, labels.iter().map(ToString::to_string).collect());
        self
    }
}

impl LandedForge for FakeForge {
    fn closing_verdicts(&mut self, issues: &[u32]) -> Option<HashMap<u32, Option<u32>>> {
        self.asked.push(issues.to_vec());
        Some(
            issues
                .iter()
                .filter_map(|n| self.graph.get(n).map(|v| (*n, *v)))
                .collect(),
        )
    }
    fn labels(&mut self, issue: u32) -> Result<Option<Vec<String>>> {
        self.labels
            .get(&issue)
            .cloned()
            .map(Some)
            .ok_or_else(|| anyhow!("no such issue"))
    }
    fn route_marked(&mut self, issue: u32, pr: u32) -> Result<bool> {
        Ok(self.marked.contains(&(issue, pr)))
    }
    fn add_label(&mut self, issue: u32, label: &str) -> bool {
        self.writes.push(format!("+{issue} {label}"));
        true
    }
    fn remove_label(&mut self, issue: u32, label: &str) -> bool {
        self.writes.push(format!("-{issue} {label}"));
        true
    }
    fn comment(&mut self, issue: u32, body: &str) -> bool {
        if self.comment_fails {
            return false;
        }
        self.writes.push(format!("comment {issue} {body}"));
        true
    }
}

/// A closing-merged PR: the issue goes to Curator (`loom:issue` swapped for
/// `loom:triage`, one comment naming the PR and carrying the marker) and is
/// reported landed. A `Part of` issue (empty closes-graph) is left alone.
#[test]
fn a_landed_issue_is_routed_to_curator_and_a_part_of_one_is_not() {
    let mut forge = FakeForge::default()
        .with_labels(5, &["loom:issue", "loom:important"])
        .with_labels(6, &["loom:issue"]);
    forge.graph.insert(5, Some(40));
    forge.graph.insert(6, None);
    let mut cache = LandedCache::default();
    let now = Instant::now();

    let landed = detect(&mut forge, &mut cache, &[5, 6], &[], now);
    assert_eq!(landed, HashMap::from([(5, 40)]));
    assert_eq!(forge.writes.len(), 3, "{:?}", forge.writes);
    assert_eq!(forge.writes[0], "+5 loom:triage", "labels first");
    assert_eq!(forge.writes[1], "-5 loom:issue");
    assert!(forge.writes[2].starts_with("comment 5 PR #40"), "{}", forge.writes[2]);
    assert!(forge.writes[2].contains(&route_marker(40)));
    assert!(!forge.writes.iter().any(|w| w.contains(" 6 ")), "#6 is untouched");
}

/// At most once: when the issue's comments already carry the marker for the
/// same PR, Curator saw this landing and put the issue back. It is not
/// landed and nothing is written; asked again later, the same landing is
/// still kept, without another comment read.
#[test]
fn an_issue_curator_kept_after_this_landing_is_dispatchable() {
    let mut forge = FakeForge::default().with_labels(5, &["loom:issue"]);
    forge.graph.insert(5, Some(40));
    forge.marked.insert((5, 40));
    let mut cache = LandedCache::default();
    let now = Instant::now();

    assert!(detect(&mut forge, &mut cache, &[5], &[], now).is_empty());
    assert!(forge.writes.is_empty(), "{:?}", forge.writes);
    assert!(detect(&mut forge, &mut cache, &[5], &[], now).is_empty());
    assert_eq!(forge.asked.len(), 1, "a fresh kept verdict is served from the cache");
    forge.marked.clear(); // the local record answers now, not the comments
    let much_later = now + LANDED_RECHECK * 4;
    assert!(detect(&mut forge, &mut cache, &[5], &[], much_later).is_empty());
    assert_eq!(forge.asked.len(), 2, "a kept verdict is re-asked once stale (#11901)");
    assert!(forge.writes.is_empty(), "{:?}", forge.writes);
}

/// #11901 case 1: a kept issue is not kept forever. A second closing PR that
/// merges while the issue stays open is a new landing: routed and landed,
/// whether it is seen on the stale re-ask or the tick its open PR merges.
#[test]
fn a_second_landing_after_a_keep_is_routed() {
    let mut forge = FakeForge::default().with_labels(5, &["loom:issue"]);
    forge.graph.insert(5, Some(40));
    forge.marked.insert((5, 40));
    let mut cache = LandedCache::default();
    let t0 = Instant::now();
    assert!(detect(&mut forge, &mut cache, &[5], &[], t0).is_empty(), "kept");

    // Stale re-ask: #41 merged meanwhile.
    forge.graph.insert(5, Some(41));
    let later = t0 + LANDED_RECHECK;
    assert_eq!(detect(&mut forge, &mut cache, &[5], &[], later), HashMap::from([(5, 41)]));
    assert!(forge.writes.iter().any(|w| w.contains(&route_marker(41))), "{:?}", forge.writes);

    // Open-PR path: a kept issue shows an open PR, which merges the next tick.
    let mut forge = FakeForge::default().with_labels(6, &["loom:issue"]);
    forge.graph.insert(6, Some(40));
    forge.marked.insert((6, 40));
    let mut cache = LandedCache::default();
    assert!(detect(&mut forge, &mut cache, &[6], &[], t0).is_empty(), "kept");
    assert!(detect(&mut forge, &mut cache, &[], &[6], t0).is_empty(), "open PR #42");
    forge.graph.insert(6, Some(42));
    assert_eq!(detect(&mut forge, &mut cache, &[6], &[], t0), HashMap::from([(6, 42)]));
    assert_eq!(forge.asked.len(), 2, "asked the tick the PR merged, not after the recheck");
}

/// #11901 case 2: a route whose comment failed leaves no marker on the forge,
/// so this host records it. When Curator puts the issue back, the same
/// landing is kept, not routed a second time.
#[test]
fn a_route_whose_comment_failed_is_not_routed_again() {
    let mut forge = FakeForge {
        comment_fails: true,
        ..FakeForge::default()
    }
    .with_labels(5, &["loom:issue"]);
    forge.graph.insert(5, Some(40));
    let mut probe = FakeForge {
        comment_fails: true,
        ..FakeForge::default()
    }
    .with_labels(5, &["loom:issue"]);
    assert_eq!(route_to_curator(&mut probe, 5, 40), Route::RoutedUnmarked);

    let mut cache = LandedCache::default();
    let t0 = Instant::now();
    assert_eq!(detect(&mut forge, &mut cache, &[5], &[], t0), HashMap::from([(5, 40)]));
    assert_eq!(forge.writes, vec!["+5 loom:triage", "-5 loom:issue"]);

    // Curator keeps it: `loom:issue` is back, and still no marker.
    forge.labels.insert(5, vec!["loom:issue".to_string()]);
    let later = t0 + LANDED_RECHECK;
    assert!(
        detect(&mut forge, &mut cache, &[5], &[], later).is_empty(),
        "kept, dispatchable"
    );
    assert_eq!(forge.writes.len(), 2, "not routed again: {:?}", forge.writes);
}

/// #11901 case 4: a listed issue under this host's landed cooldown. One this
/// host routed is asked about the first time it is listed again, and lifted
/// when Curator kept it; one reopened after the merge is lifted too; one
/// still landed (left unrouted) keeps its cooldown and is not re-asked
/// before [`LANDED_RECHECK`].
#[test]
fn a_kept_or_reopened_issue_lifts_its_landed_cooldown() {
    let mut forge = FakeForge::default()
        .with_labels(5, &["loom:issue"])
        .with_labels(6, &["loom:issue"])
        .with_labels(7, &["loom:issue", "loom:building"]);
    forge.graph.insert(5, Some(40));
    forge.graph.insert(6, Some(41));
    forge.graph.insert(7, Some(42));
    let mut cache = LandedCache::default();
    let t0 = Instant::now();
    let landed = detect(&mut forge, &mut cache, &[5, 6, 7], &[], t0);
    assert_eq!(landed.len(), 3, "all three landed; 7 is claimed, so left unrouted");

    // A minute later: Curator kept #5; #6 was reopened after #41 merged.
    forge.graph.insert(6, None);
    let t1 = t0 + Duration::from_secs(60);
    let lifted = lift(&mut forge, &mut cache, &[5, 7], t1);
    assert_eq!(lifted, vec![5], "#5 was routed here, so it is asked at once");
    assert_eq!(forge.asked.last().unwrap(), &vec![5]);

    let t2 = t0 + LANDED_RECHECK;
    let lifted = lift(&mut forge, &mut cache, &[6, 7], t2);
    assert_eq!(lifted, vec![6], "reopened after the merge: lifted; #7 still landed");
    assert_eq!(forge.asked.last().unwrap(), &vec![6, 7]);
    assert!(lift(&mut forge, &mut cache, &[7], t2).is_empty());
    assert_eq!(forge.asked.len(), 3, "#7 is not re-asked before the recheck");
}

/// `loom:frozen`, a park label or a live claim: still landed (so the
/// cooldown applies), but never relabelled.
#[test]
fn a_frozen_or_parked_issue_is_not_relabelled() {
    for labels in [
        &["loom:issue", "loom:frozen"][..],
        &["loom:issue", "loom:blocked"][..],
        &["loom:issue", "loom:building"][..],
    ] {
        let mut forge = FakeForge::default().with_labels(5, labels);
        assert!(matches!(route_to_curator(&mut forge, 5, 40), Route::Unrouted(_)), "{labels:?}");
        assert!(forge.writes.is_empty(), "{labels:?}: {:?}", forge.writes);
    }
}

/// One question per tick at most; a verdict is reused until it is
/// [`LANDED_RECHECK`] old, and forgotten as soon as the issue shows an open
/// PR, so the tick that PR merges asks again.
#[test]
fn verdicts_are_cached_until_stale_or_an_open_pr_appears() {
    let mut forge = FakeForge::default().with_labels(5, &["loom:issue"]);
    forge.graph.insert(5, None);
    forge.graph.insert(6, None);
    let mut cache = LandedCache::default();
    let t0 = Instant::now();

    assert!(detect(&mut forge, &mut cache, &[5, 6], &[], t0).is_empty());
    assert!(detect(&mut forge, &mut cache, &[5, 6], &[], t0).is_empty());
    assert_eq!(forge.asked, vec![vec![5, 6]], "the second tick is served from the cache");

    // #5 gets an open PR, which then merges and closes it.
    assert!(detect(&mut forge, &mut cache, &[6], &[5], t0).is_empty());
    forge.graph.insert(5, Some(40));
    let landed = detect(&mut forge, &mut cache, &[5, 6], &[], t0);
    assert_eq!(landed, HashMap::from([(5, 40)]));
    assert_eq!(forge.asked.last().unwrap(), &vec![5], "only the forgotten verdict is asked");

    let stale = t0 + LANDED_RECHECK;
    detect(&mut forge, &mut cache, &[6], &[], stale);
    assert_eq!(forge.asked.last().unwrap(), &vec![6], "a stale verdict is asked again");
}

/// An unanswered query is no verdict: nothing is removed.
#[test]
fn an_unanswered_query_removes_nothing() {
    struct Down;
    impl LandedForge for Down {
        fn closing_verdicts(&mut self, _: &[u32]) -> Option<HashMap<u32, Option<u32>>> {
            None
        }
        fn labels(&mut self, _: u32) -> Result<Option<Vec<String>>> {
            unreachable!()
        }
        fn route_marked(&mut self, _: u32, _: u32) -> Result<bool> {
            unreachable!()
        }
        fn add_label(&mut self, _: u32, _: &str) -> bool {
            unreachable!()
        }
        fn remove_label(&mut self, _: u32, _: &str) -> bool {
            unreachable!()
        }
        fn comment(&mut self, _: u32, _: &str) -> bool {
            unreachable!()
        }
    }
    let mut cache = LandedCache::default();
    assert!(detect(&mut Down, &mut cache, &[1, 2], &[], Instant::now()).is_empty());
}

// ---------------------------------------------------------------------------
// Cost (#12205)
// ---------------------------------------------------------------------------

/// The step gather hands every `loom:issue` item over, open PR or not. A
/// candidate with an open PR is never asked about, so a workspace whose only
/// unanswered candidate has a PR in review makes no query, tick after tick.
/// Before #12205 its verdict was dropped and it was re-asked every tick.
#[test]
fn a_candidate_with_an_open_pr_is_never_asked_about() {
    let mut forge = FakeForge::default();
    forge.graph.insert(5, None);
    forge.graph.insert(6, None);
    let mut cache = LandedCache::default();
    let t0 = Instant::now();
    for tick in 0..10u64 {
        let now = t0 + Duration::from_secs(60 * tick);
        assert!(detect(&mut forge, &mut cache, &[5, 6], &[5], now).is_empty());
    }
    assert_eq!(forge.asked, vec![vec![6]], "one query, and never for #5");
}

/// A cached verdict makes no forge call: ticks inside the verdict's life
/// cost nothing, and a candidate that turns up later brings the half-aged
/// verdicts along, so the workspace's verdicts expire together and it pays
/// one query per half [`LANDED_RECHECK`] at most, not one per tick.
#[test]
fn cached_verdicts_make_no_call_and_expiries_are_coalesced() {
    let mut forge = FakeForge::default();
    for n in [5, 6, 7] {
        forge.graph.insert(n, None);
    }
    let mut cache = LandedCache::default();
    let t0 = Instant::now();
    let minute = Duration::from_secs(60);
    detect(&mut forge, &mut cache, &[5], &[], t0);
    for m in 1..20u32 {
        detect(&mut forge, &mut cache, &[5], &[], t0 + minute * m);
    }
    assert_eq!(forge.asked, vec![vec![5]], "a cached verdict makes no forge call");

    // #6 is new at minute 20: asked, and #5 (20 minutes old) rides along.
    detect(&mut forge, &mut cache, &[5, 6], &[], t0 + minute * 20);
    assert_eq!(forge.asked.last().unwrap(), &vec![6, 5]);
    // #7 at minute 25: #5 and #6 are only 5 minutes old, so #7 goes alone.
    detect(&mut forge, &mut cache, &[5, 6, 7], &[], t0 + minute * 25);
    assert_eq!(forge.asked.last().unwrap(), &vec![7]);
    // Up to minute 45 every verdict is fresh: no call at all.
    for m in 26..45u32 {
        detect(&mut forge, &mut cache, &[5, 6, 7], &[], t0 + minute * m);
    }
    assert_eq!(forge.asked.len(), 3, "{:?}", forge.asked);
}

/// An unanswered query is not repeated every tick: nothing is asked for
/// [`LANDED_FAIL_BACKOFF`], then the question is asked again.
#[test]
fn an_unanswered_query_backs_off() {
    struct Flaky {
        up: bool,
        asked: usize,
    }
    impl LandedForge for Flaky {
        fn closing_verdicts(&mut self, issues: &[u32]) -> Option<HashMap<u32, Option<u32>>> {
            self.asked += 1;
            self.up.then(|| issues.iter().map(|n| (*n, None)).collect())
        }
        fn labels(&mut self, _: u32) -> Result<Option<Vec<String>>> {
            unreachable!()
        }
        fn route_marked(&mut self, _: u32, _: u32) -> Result<bool> {
            unreachable!()
        }
        fn add_label(&mut self, _: u32, _: &str) -> bool {
            unreachable!()
        }
        fn remove_label(&mut self, _: u32, _: &str) -> bool {
            unreachable!()
        }
        fn comment(&mut self, _: u32, _: &str) -> bool {
            unreachable!()
        }
    }
    let mut forge = Flaky {
        up: false,
        asked: 0,
    };
    let mut cache = LandedCache::default();
    let t0 = Instant::now();
    for s in (0..LANDED_FAIL_BACKOFF.as_secs()).step_by(60) {
        detect(&mut forge, &mut cache, &[1], &[], t0 + Duration::from_secs(s));
        lift(&mut forge, &mut cache, &[1], t0 + Duration::from_secs(s));
    }
    assert_eq!(forge.asked, 1, "one query per backoff, not one per tick");
    forge.up = true;
    detect(&mut forge, &mut cache, &[1], &[], t0 + LANDED_FAIL_BACKOFF);
    assert_eq!(forge.asked, 2);
    detect(&mut forge, &mut cache, &[1], &[], t0 + LANDED_FAIL_BACKOFF);
    assert_eq!(forge.asked, 2, "answered: cached again");
}

/// The pre-filter is off unless the live key says `true`.
#[test]
fn the_prefilter_is_off_unless_the_live_key_is_true() {
    assert!(!prefilter_enabled_in(&json!({})));
    assert!(!prefilter_enabled_in(&json!({ "autonomous": { "workFinder": {} } })));
    assert!(!prefilter_enabled_in(
        &json!({ "autonomous": { "workFinder": { "landedPrefilter": "yes" } } })
    ));
    assert!(!prefilter_enabled_in(
        &json!({ "autonomous": { "workFinder": { "landedPrefilter": false } } })
    ));
    assert!(prefilter_enabled_in(
        &json!({ "autonomous": { "workFinder": { "landedPrefilter": true } } })
    ));

    // Read live from the workspace's config: an edit applies on the next call.
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join(".loom/config.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    assert!(!prefilter_enabled(root.path()), "no config: off");
    std::fs::write(&config, r#"{"autonomous":{"workFinder":{"landedPrefilter":true}}}"#).unwrap();
    assert!(prefilter_enabled(root.path()));
    std::fs::write(&config, r#"{"autonomous":{"workFinder":{"landedPrefilter":false}}}"#).unwrap();
    assert!(!prefilter_enabled(root.path()));
}

/// The closes-graph read is a repo-targeted Hygiene read: the facade offers
/// it to the reader pool, and with every reader out of budget it is shed
/// (no request, no verdict) instead of spilling onto the dispatch App.
#[test]
fn the_closes_graph_read_goes_to_the_reader_pool_and_is_shed_not_spilled() {
    use crate::forge_identity::{ExhaustCause, ReadClass, RouteDecision};
    use crate::gh_invocation::test_routing::{install, seen, sheds, TestRouting};
    let root = tempfile::tempdir().unwrap();
    let _g = install(TestRouting {
        decision: RouteDecision::Exhausted {
            until: std::time::SystemTime::now() + Duration::from_secs(900),
            cause: ExhaustCause::Budget,
        },
        env: Default::default(),
        checkout: crate::gh_invocation::cwd_route::CwdAnswer::Sole("acme/shed".into()),
    });
    let mut forge = GhLandedForge {
        gh_bin: PathBuf::from("gh"),
        root: root.path().to_path_buf(),
        slug: "acme/shed".to_string(),
    };
    assert_eq!(forge.closing_verdicts(&[5, 6]), None, "a shed is no verdict");
    assert_eq!(sheds(), 1, "shed, never run on the writer");
    let seen = seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].op, "work_finder.landed_closing_refs");
    assert_eq!(seen[0].class, ReadClass::Hygiene);
    let inv = closing_refs_invocation(std::path::Path::new("gh"), root.path(), "acme/shed", &[5])
        .unwrap();
    assert_eq!(inv.target().slug().as_deref(), Some("acme/shed"));
    assert!(
        closing_refs_invocation(std::path::Path::new("gh"), root.path(), "acme/x\"", &[5])
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// The tick seam
// ---------------------------------------------------------------------------

struct OneShotSource(Option<Vec<WorkItem>>);

impl WorkSource for OneShotSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

/// A dispatcher whose forge knows open linked PRs and merged closing PRs.
#[derive(Default)]
struct Disp {
    open_pr: HashMap<u32, u32>,
    landed: HashMap<u32, u32>,
    in_flight: HashSet<u32>,
    asked: RefCell<Vec<(Vec<u32>, Vec<u32>)>>,
    attempts: Vec<u32>,
}

impl WorkDispatcher for Disp {
    fn in_flight(&self) -> HashSet<u32> {
        self.in_flight.clone()
    }
    fn occupancy(&self) -> usize {
        self.in_flight.len()
    }
    fn open_pr_blocked(&self, candidates: &[u32]) -> HashMap<u32, u32> {
        candidates
            .iter()
            .filter_map(|n| self.open_pr.get(n).map(|p| (*n, *p)))
            .collect()
    }
    fn landed_closing_prs(&self, candidates: &[u32], open_pr: &[u32]) -> HashMap<u32, u32> {
        let mut c = candidates.to_vec();
        let mut o = open_pr.to_vec();
        c.sort_unstable();
        o.sort_unstable();
        self.asked.borrow_mut().push((c, o));
        candidates
            .iter()
            .filter_map(|n| self.landed.get(n).map(|p| (*n, *p)))
            .collect()
    }
    fn dispatch(&mut self, issue: u32) -> Result<bool> {
        self.attempts.push(issue);
        self.in_flight.insert(issue);
        Ok(true)
    }
}

fn items(numbers: &[u32]) -> Vec<WorkItem> {
    numbers
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let created = format!("2026-09-{:02}T00:00:00Z", i + 1);
            WorkItem::with_created_at(*n, vec!["loom:issue".to_string()], Some(created))
        })
        .collect()
}

fn rows(report: &TickReport) -> Vec<ReadyQueueRow> {
    ready_queue::finish(&report.queue, &[PathBuf::from("/repo/a")])
}

/// A landed candidate is never offered to `dispatch()`: it is resolved
/// `AlreadyLanded` with the PR named, the next candidate takes the slot, and
/// the workspace is asked once, without its open-PR candidates.
#[test]
fn a_landed_candidate_is_skipped_without_a_dispatch_attempt() {
    let disp = Disp {
        open_pr: HashMap::from([(1, 801)]),
        landed: HashMap::from([(2, 900)]),
        ..Disp::default()
    };
    let mut multi = vec![(OneShotSource(Some(items(&[1, 2, 3, 4]))), disp)];
    let report = tick_multi(&mut multi, &[1], 1, &[false]);

    let disp = &multi[0].1;
    assert_eq!(disp.attempts, vec![3], "2 is landed, 1 has an open PR");
    assert_eq!(disp.asked.borrow().clone(), vec![(vec![2, 3, 4], vec![1])]);
    assert_eq!(report.skipped_already_landed, 1);
    assert_eq!(report.skipped_pr_open, 1);
    let row = rows(&report).into_iter().find(|r| r.issue == 2).unwrap();
    assert_eq!(row.disposition, QueueDisposition::AlreadyLanded);
    assert_eq!(row.detail.as_deref(), Some("merged closing PR #900"));
}

/// #11901: a listed candidate whose landed cooldown the dispatcher lifts is
/// no longer skipped as backed off; one it does not lift still is.
#[test]
fn a_lifted_landed_cooldown_dispatches_this_tick() {
    struct Lifting(Disp, HashSet<u32>);
    impl WorkDispatcher for Lifting {
        fn in_flight(&self) -> HashSet<u32> {
            self.0.in_flight()
        }
        fn occupancy(&self) -> usize {
            self.0.occupancy()
        }
        fn backed_off(&self) -> HashSet<u32> {
            HashSet::from([1, 2])
        }
        fn lift_landed_cooldowns(&self, held: &[u32]) -> HashSet<u32> {
            held.iter()
                .copied()
                .filter(|n| self.1.contains(n))
                .collect()
        }
        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            self.0.dispatch(issue)
        }
    }
    let disp = Lifting(Disp::default(), HashSet::from([1]));
    let mut multi = vec![(OneShotSource(Some(items(&[1, 2]))), disp)];
    let report = tick_multi(&mut multi, &[1], 2, &[false]);
    assert_eq!(multi[0].1 .0.attempts, vec![1], "#1 lifted, #2 still backed off");
    assert_eq!(report.skipped_backoff, 1);
}
