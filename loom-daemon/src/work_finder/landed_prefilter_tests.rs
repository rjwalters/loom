//! Issue #11853: the landed pre-filter — the closes-graph verdict, the
//! Curator routing, the verdict cache, and the tick seam.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::json;

use super::*;

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
// The tick seam
// ---------------------------------------------------------------------------
