//! The forge reads behind dependency-aware ETAs (#10510); the graph, the
//! point-in-time rule and the composition are [`crate::eta::dependency`]'s.
//!
//! Per pass, before the pass's `as_of` is taken (so every edge is knowable
//! at the estimates it feeds), at most [`READ_BUDGET`] `(item, source)`
//! reads, never-read and oldest first, each at most every
//! [`REFRESH_SECS`]:
//!
//! - **Parked** (an unstarted item refused `blocked` or `no_dispatch_plan`):
//!   `issues/{n}` for its park records (`Blocked by: #N`, same repo), and
//!   `issues/{n}/dependencies/blocked_by` for the forge-native "blocked by"
//!   issues, any repo, each with its state. Each park-record parent the
//!   tracker does not hold open is read once (`issues/{N}`) for whether, and
//!   when, it closed.
//! - **Sequenced** (a PR under `loom:sequenced`): its trusted comment bodies
//!   for the newest live `<!-- loom:sequence after=N -->` marker; the
//!   predecessor PR maps to the tracked issue it closes. A predecessor the
//!   tracker does not hold is skipped: the sequencing pass releases a hold
//!   whose predecessor merged.
//!
//! The `issues/{n}` read of a parked item also yields, at no extra call:
//!
//! - **Dependency phrases** (#10526): on a `loom:blocked` issue, `Blocked
//!   by #N` / `Depends on owner/repo#N` prose outside HTML comments joins
//!   the park-record parents (source `ParkRecord`). A qualified reference
//!   keeps its repository identity.
//! - **Epic phase** (#10526): its `<!-- loom:epic:P:phase:n -->` marker. A
//!   phase waits on every member of the nearest lower phase read for the
//!   same epic ([`DependencyBook::phase_parents`]); only issues whose body
//!   was read have a marker, so an unread phase is skipped, not guessed.
//! - **Sub-issues** (#10526): `sub_issues_summary.total > 0` marks the item
//!   for a `(item, SubIssue)` read of `issues/{n}/sub_issues`. That read is
//!   an ordinary due read, inside [`READ_BUDGET`]. Direction: the **parent
//!   waits on its sub-issues** (the aggregate lands last); membership never
//!   links siblings, and a sub-issue that itself waits on the parent
//!   (a reverse edge from any source) is not recorded, so no cycle.
//!
//! **Stacked PRs** (#10526) need no read: the feature pass already fetched
//! each tracked PR's `pulls/{n}` (head and base branch), and a PR whose base
//! branch is exactly one other tracked PR's head waits to merge after it
//! ([`stacked_parents`]). A base no tracked PR owns, an ambiguous owner
//! (several PRs or issues share the head), or a PR the feature pass has not
//! read yet records nothing and keeps whatever was observed.
//!
//! A failed read changes nothing: the edges already observed stand until a
//! read succeeds, and a successful read replaces only its own source. An
//! edge's `known_at` is the instant it was first read, never an earlier body
//! edit. Parents the tracker does not hold open are read once for their
//! closure ([`Stats::closure`], outside the due-read budget) and, once known
//! closed, not re-read.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::eta::dependency::{DependencyBook, EdgeSource, NodeKey};
use crate::eta::tracker::{DependencyCandidate, Tracker};
use crate::forge_call_stats::{ops, ForgeOp};
use regex::Regex;
use std::sync::OnceLock;

/// Seconds between reads of one `(item, source)`.
pub const REFRESH_SECS: i64 = 900;

/// `(item, source)` reads per pass, across all repos.
pub const READ_BUDGET: usize = 8;

/// Accounting row for the native dependency listing (no inventory row yet).
const BLOCKED_BY: ForgeOp =
    ForgeOp::uninventoried("issue blocked_by dependency listing has no inventory row");

/// Accounting row for the native sub-issue listing (no inventory row yet).
const SUB_ISSUES: ForgeOp = ForgeOp::uninventoried("issue sub_issues listing has no inventory row");

/// The `owner/repo` an API `repository_url` names.
fn repo_of_url(url: &str) -> Option<String> {
    let rest = url.split("/repos/").nth(1)?;
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

fn closed_at(issue: &Value) -> Option<DateTime<Utc>> {
    (issue["state"].as_str() == Some("closed"))
        .then(|| issue["closed_at"].as_str()?.parse().ok())
        .flatten()
}

/// Parse a `dependencies/blocked_by` page into `(parent, closed_at)`.
#[must_use]
pub fn native_blockers(page: &Value) -> Option<Vec<(NodeKey, Option<DateTime<Utc>>)>> {
    page.as_array().map(|issues| {
        issues
            .iter()
            .filter_map(|issue| {
                let number = u32::try_from(issue["number"].as_u64()?).ok()?;
                let repo = repo_of_url(issue["repository_url"].as_str()?)?;
                Some((NodeKey::new(&repo, number), closed_at(issue)))
            })
            .collect()
    })
}

/// The same-repo park-record parents an issue body declares. A blocker
/// qualified with another repository (#10443) is not a same-repo edge and
/// is skipped.
#[must_use]
pub fn park_blockers(slug: &str, body: &str) -> Vec<NodeKey> {
    let mut out: Vec<NodeKey> = crate::park_record::parse(body)
        .into_iter()
        .filter_map(|r| r.blocker.filter(|b| b.is_local(Some(slug))))
        .filter_map(|b| u32::try_from(b.number).ok())
        .map(|n| NodeKey::new(slug, n))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The prose references a `loom:blocked` body makes: `Blocked by` /
/// `Depends on` followed by `#N` or `owner/repo#N`, comma or "and"
/// separated, to the end of the line. HTML comments (park records, markers)
/// are not prose and are skipped; a self-reference is dropped.
#[must_use]
pub fn phrase_blockers(slug: &str, own: u32, body: &str) -> Vec<NodeKey> {
    static PHRASE: OnceLock<Regex> = OnceLock::new();
    static REF: OnceLock<Regex> = OnceLock::new();
    let phrase = PHRASE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:blocked[ -]by|depends[ -]on)\b[:\s]*([^\n]*)").expect("phrase regex")
    });
    let reference = REF.get_or_init(|| {
        Regex::new(r"(?:([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+))?#(\d+)").expect("ref regex")
    });
    let mut prose = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find("<!--") {
        prose.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            Some(j) => rest = &rest[i + j + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    prose.push_str(rest);
    let mut out = Vec::new();
    for cap in phrase.captures_iter(&prose) {
        // Only the leading run of references: stop at the first word that
        // is not a separator, so trailing prose cannot add edges.
        let tail = &cap[1];
        let mut cursor = 0;
        while let Some(m) = reference.captures(&tail[cursor..]) {
            let whole = m.get(0).map_or(0..0, |w| w.range());
            let gap = tail[cursor..cursor + whole.start].trim();
            if !gap.is_empty() && !matches!(gap, "," | "and" | "&" | ", and") {
                break;
            }
            let repo = m.get(1).map_or(slug, |r| r.as_str());
            if let Ok(n) = m[2].parse::<u32>() {
                let key = NodeKey::new(repo, n);
                if key != NodeKey::new(slug, own) {
                    out.push(key);
                }
            }
            cursor += whole.end;
        }
    }
    out.sort();
    out.dedup();
    out
}

fn has_label(issue: &Value, name: &str) -> bool {
    issue["labels"].as_array().is_some_and(|l| {
        l.iter()
            .any(|x| x["name"].as_str().or_else(|| x.as_str()) == Some(name))
    })
}

/// The `(parent PR's tracked issue)` each tracked PR is stacked on, as
/// `(child, parents)`; `None` parents means "no information, change
/// nothing". See the module docs for the conservative rules.
#[must_use]
pub fn stacked_parents(candidates: &[DependencyCandidate]) -> Vec<(NodeKey, Option<Vec<NodeKey>>)> {
    // repo -> head branch -> (pr, issue) pairs
    type Heads = BTreeMap<String, BTreeMap<String, BTreeSet<(u32, u32)>>>;
    let mut heads: Heads = BTreeMap::new();
    let mut all_known: BTreeMap<&str, bool> = BTreeMap::new();
    for c in candidates {
        let Some(pr) = c.pr_number else { continue };
        let known = all_known.entry(c.key.repo.as_str()).or_insert(true);
        match &c.head_ref {
            Some(head) => {
                heads
                    .entry(c.key.repo.clone())
                    .or_default()
                    .entry(head.clone())
                    .or_default()
                    .insert((pr, c.key.issue));
            }
            None => *known = false,
        }
    }
    let mut out = Vec::new();
    for c in candidates {
        let (Some(pr), Some(base)) = (c.pr_number, &c.base_ref) else {
            continue;
        };
        let child = NodeKey::new(&c.key.repo, c.key.issue);
        let owners: Vec<&(u32, u32)> = heads
            .get(&c.key.repo)
            .and_then(|h| h.get(base))
            .map(|set| set.iter().filter(|(p, _)| *p != pr).collect())
            .unwrap_or_default();
        let parents = match owners.as_slice() {
            [(_, issue)] => Some(vec![NodeKey::new(&c.key.repo, *issue)]),
            [] if all_known.get(c.key.repo.as_str()).copied().unwrap_or(false) => Some(Vec::new()),
            _ => None,
        };
        out.push((child, parents));
    }
    out
}

/// Every `(item, source)` worth reading, by the candidates' state.
fn wanted(candidates: &[DependencyCandidate], book: &DependencyBook) -> Vec<(NodeKey, EdgeSource)> {
    let mut out = Vec::new();
    for c in candidates {
        let key = NodeKey::new(&c.key.repo, c.key.issue);
        if c.parked {
            out.push((key.clone(), EdgeSource::ParkRecord));
            out.push((key.clone(), EdgeSource::NativeDependency));
            if book.has_subs(&key) {
                out.push((key.clone(), EdgeSource::SubIssue));
            }
        }
        if c.sequenced {
            out.push((key, EdgeSource::Sequence));
        }
    }
    out
}

/// The forge reads one pass makes; every method is `None` on failure.
pub(super) trait Source {
    /// `repos/{repo}/issues/{n}`.
    fn issue(&self, repo: &str, n: u32) -> Option<Value>;
    /// `issues/{n}/dependencies/blocked_by`.
    fn blocked_by(&self, repo: &str, n: u32) -> Option<Value>;
    /// `issues/{n}/sub_issues`.
    fn sub_issues(&self, repo: &str, n: u32) -> Option<Value>;
    /// The trusted comment bodies of PR `pr`.
    fn sequence_bodies(&self, repo: &str, pr: u32) -> Option<Vec<String>>;
}

/// The forge, through the ETag-cached reads.
struct Live {
    roots: BTreeMap<String, PathBuf>,
}

impl Live {
    fn root(&self, repo: &str) -> Option<&PathBuf> {
        self.roots.get(repo)
    }
}

impl Source for Live {
    fn issue(&self, repo: &str, n: u32) -> Option<Value> {
        // A parent in a repo the pass does not check out is read through
        // any checkout: the url names the repo.
        let root = self.root(repo).or_else(|| self.roots.values().next())?;
        super::eta_friction::cached_get(
            root,
            repo,
            &format!("repos/{repo}/issues/{n}"),
            ops::ISSUE_VIEW_STATE,
        )
    }
    fn blocked_by(&self, repo: &str, n: u32) -> Option<Value> {
        let url = format!("repos/{repo}/issues/{n}/dependencies/blocked_by?per_page=100");
        super::eta_friction::cached_get(self.root(repo)?, repo, &url, BLOCKED_BY)
    }
    fn sub_issues(&self, repo: &str, n: u32) -> Option<Value> {
        let url = format!("repos/{repo}/issues/{n}/sub_issues?per_page=100");
        super::eta_friction::cached_get(self.root(repo)?, repo, &url, SUB_ISSUES)
    }
    fn sequence_bodies(&self, repo: &str, pr: u32) -> Option<Vec<String>> {
        let gh = crate::gh_invocation::gh_bin();
        crate::merge_pr::sequence::fetch_trusted_bodies(&gh, self.root(repo)?, repo, pr)
    }
}

/// What a pass spent: due `(item, source)` reads against [`READ_BUDGET`],
/// and parent-closure reads, accounted apart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stats {
    pub due: usize,
    pub closure: usize,
}

type Parsed = (Vec<NodeKey>, Vec<(NodeKey, DateTime<Utc>)>);

/// The parents of a native listing page and those it shows closed.
fn native_edges(page: &Value) -> Option<Parsed> {
    let blockers = native_blockers(page)?;
    Some((
        blockers.iter().map(|(k, _)| k.clone()).collect(),
        blockers
            .into_iter()
            .filter_map(|(k, at)| Some((k, at?)))
            .collect(),
    ))
}

/// One pass over `book`: the due reads, then the read-free sources
/// (phases, stacked PRs), then the prune to the open items.
pub(super) fn run(
    source: &dyn Source,
    mut book: DependencyBook,
    candidates: &[DependencyCandidate],
    at: DateTime<Utc>,
) -> (DependencyBook, Stats) {
    let mut stats = Stats::default();
    let mut pr_issues: BTreeMap<String, BTreeMap<u32, u32>> = BTreeMap::new();
    let mut prs: BTreeMap<NodeKey, u32> = BTreeMap::new();
    for c in candidates {
        if let Some(pr) = c.pr_number {
            pr_issues
                .entry(c.key.repo.clone())
                .or_default()
                .insert(pr, c.key.issue);
            prs.insert(NodeKey::new(&c.key.repo, c.key.issue), pr);
        }
    }
    let open: BTreeSet<NodeKey> = candidates
        .iter()
        .map(|c| NodeKey::new(&c.key.repo, c.key.issue))
        .collect();
    let due = book.due(&wanted(candidates, &book), at, REFRESH_SECS, READ_BUDGET);
    for (child, edge_source) in due {
        stats.due += 1;
        let slug = child.repo.clone();
        let read_at = Utc::now();
        let (parents, landed) = match edge_source {
            EdgeSource::ParkRecord => {
                let Some(issue) = source.issue(&slug, child.issue) else {
                    continue;
                };
                let body = issue["body"].as_str().unwrap_or_default();
                let mut parents = park_blockers(&slug, body);
                if has_label(&issue, "loom:blocked") {
                    parents.extend(phrase_blockers(&slug, child.issue, body));
                    parents.sort();
                    parents.dedup();
                }
                let marker = crate::epic_supervisor::forge::parse_epic_phase_marker(body);
                if book.set_phase(&child, marker) {
                    book.set(&child, EdgeSource::EpicPhase, &[], read_at);
                }
                book.set_has_subs(
                    &child,
                    issue["sub_issues_summary"]["total"].as_u64().unwrap_or(0) > 0,
                );
                (parents, Vec::new())
            }
            EdgeSource::NativeDependency => {
                let Some((p, l)) = source
                    .blocked_by(&slug, child.issue)
                    .and_then(|page| native_edges(&page))
                else {
                    continue;
                };
                (p, l)
            }
            EdgeSource::SubIssue => {
                let Some((subs, l)) = source
                    .sub_issues(&slug, child.issue)
                    .and_then(|page| native_edges(&page))
                else {
                    continue;
                };
                // Membership is not a wait on the parent: a sub-issue that
                // already waits on this parent would close a cycle.
                let subs = subs
                    .into_iter()
                    .filter(|s| *s != child && !book.waits_on(s, &child))
                    .collect();
                (subs, l)
            }
            EdgeSource::Sequence => {
                let Some(pr) = prs.get(&child) else { continue };
                let Some(bodies) = source.sequence_bodies(&slug, *pr) else {
                    continue;
                };
                let parents = crate::merge_pr::sequence::parse_live(&bodies)
                    .and_then(|m| pr_issues.get(&slug)?.get(&m.after))
                    .map(|issue| vec![NodeKey::new(&slug, *issue)])
                    .unwrap_or_default();
                (parents, Vec::new())
            }
            EdgeSource::EpicPhase | EdgeSource::StackedPr => continue,
        };
        book.set(&child, edge_source, &parents, read_at);
        for (parent, closed) in landed {
            book.set_landed(&parent, closed);
        }
        if edge_source == EdgeSource::ParkRecord {
            let unknown: Vec<&NodeKey> = parents
                .iter()
                .filter(|p| !open.contains(*p) && !book.landed.contains_key(*p))
                .collect();
            for parent in unknown {
                stats.closure += 1;
                if let Some(closed) = source
                    .issue(&parent.repo, parent.issue)
                    .as_ref()
                    .and_then(closed_at)
                {
                    book.set_landed(parent, closed);
                }
            }
        }
    }
    for (child, parents) in book.phase_parents() {
        book.set(&child, EdgeSource::EpicPhase, &parents, at);
    }
    for (child, parents) in stacked_parents(candidates) {
        if let Some(parents) = parents {
            book.set(&child, EdgeSource::StackedPr, &parents, at);
        }
    }
    book.retain(&open);
    (book, stats)
}

/// What a pass reads of the tracker, under its lock, before refreshing:
/// the book so far and the items whose edges may be read.
pub(super) fn seed(tracker: &Tracker) -> (DependencyBook, Vec<DependencyCandidate>) {
    (tracker.dependencies.clone(), tracker.dependency_candidates())
}

/// Refresh the due part of the seeded book; `repos` are the pass's
/// `(checkout, slug, ..)` rows, keyed here by lowercased slug. Blocking
/// reads run off the async workers.
pub(super) async fn refresh<A, B>(
    (book, candidates): (DependencyBook, Vec<DependencyCandidate>),
    repos: &[(PathBuf, String, A, B)],
) -> DependencyBook {
    let live = Live {
        roots: repos
            .iter()
            .map(|(root, slug, ..)| (slug.to_ascii_lowercase(), root.clone()))
            .collect(),
    };
    let at = Utc::now();
    tokio::task::spawn_blocking(move || run(&live, book, &candidates, at).0)
        .await
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{native_blockers, park_blockers, repo_of_url};
    use crate::eta::dependency::NodeKey;
    use serde_json::json;

    #[test]
    fn park_records_name_same_repo_parents_and_ignore_reason_mentions() {
        let body = "text\n<!-- loom:park Blocked by: #12 by=curator reason=\"after #99\" -->\n\
                    <!-- loom:park Blocked by: #7 -->";
        assert_eq!(
            park_blockers("Owner/Repo", body),
            vec![
                NodeKey::new("owner/repo", 7),
                NodeKey::new("owner/repo", 12)
            ]
        );
        assert!(park_blockers("o/r", "no record").is_empty());
        // A qualified blocker (#10443) counts only when it names this repo.
        let qualified = "<!-- loom:park Blocked by: Other/Repo#5 -->\n\
                         <!-- loom:park Blocked by: owner/repo#6 -->";
        assert_eq!(park_blockers("Owner/Repo", qualified), vec![NodeKey::new("owner/repo", 6)]);
    }

    #[test]
    fn native_blockers_carry_repo_and_closing_time() {
        let page = json!([
            {"number": 5, "repository_url": "https://api.github.com/repos/a/b", "state": "open"},
            {"number": 6, "repository_url": "https://api.github.com/repos/c/d",
             "state": "closed", "closed_at": "2026-10-01T00:00:00Z"},
            {"number": 7}
        ]);
        let got = native_blockers(&page).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (NodeKey::new("a/b", 5), None));
        assert_eq!(got[1].0, NodeKey::new("c/d", 6));
        assert!(got[1].1.is_some());
        assert!(native_blockers(&json!({"message": "Not Found"})).is_none());
        assert_eq!(repo_of_url("https://x/repos/o/r").as_deref(), Some("o/r"));
    }

    // ------------------------------------------------ #10526 ingestion

    use super::{phrase_blockers, run, stacked_parents, Source, Stats, READ_BUDGET, REFRESH_SECS};
    use crate::eta::dependency::{DependencyBook, DependencyGraph, EdgeSource};
    use crate::eta::tracker::{DependencyCandidate, ItemKey};
    use chrono::{DateTime, Duration, Utc};
    use serde_json::Value;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    const R: &str = "o/r";

    fn k(n: u32) -> NodeKey {
        NodeKey::new(R, n)
    }

    #[derive(Default)]
    struct Mock {
        issues: RefCell<BTreeMap<(String, u32), Value>>,
        blocked_by: RefCell<BTreeMap<u32, Value>>,
        subs: RefCell<BTreeMap<u32, Value>>,
        bodies: RefCell<BTreeMap<u32, Vec<String>>>,
        issue_calls: Cell<usize>,
        other_calls: Cell<usize>,
    }

    impl Mock {
        fn issue(&self, repo: &str, n: u32, v: Value) {
            self.issues.borrow_mut().insert((repo.into(), n), v);
        }
    }

    impl Source for Mock {
        fn issue(&self, repo: &str, n: u32) -> Option<Value> {
            self.issue_calls.set(self.issue_calls.get() + 1);
            self.issues.borrow().get(&(repo.to_string(), n)).cloned()
        }
        fn blocked_by(&self, _: &str, n: u32) -> Option<Value> {
            self.other_calls.set(self.other_calls.get() + 1);
            self.blocked_by.borrow().get(&n).cloned()
        }
        fn sub_issues(&self, _: &str, n: u32) -> Option<Value> {
            self.other_calls.set(self.other_calls.get() + 1);
            self.subs.borrow().get(&n).cloned()
        }
        fn sequence_bodies(&self, _: &str, pr: u32) -> Option<Vec<String>> {
            self.other_calls.set(self.other_calls.get() + 1);
            self.bodies.borrow().get(&pr).cloned()
        }
    }

    fn cand(issue: u32, parked: bool) -> DependencyCandidate {
        DependencyCandidate {
            key: ItemKey::new(R, issue),
            pr_number: None,
            parked,
            sequenced: false,
            head_ref: None,
            base_ref: None,
        }
    }

    fn pr(issue: u32, number: u32, head: Option<&str>, base: Option<&str>) -> DependencyCandidate {
        DependencyCandidate {
            pr_number: Some(number),
            head_ref: head.map(str::to_string),
            base_ref: base.map(str::to_string),
            ..cand(issue, false)
        }
    }

    fn blocked_issue(body: &str) -> Value {
        json!({"body": body, "labels": [{"name": "loom:blocked"}], "state": "open"})
    }

    fn t0() -> DateTime<Utc> {
        Utc::now() - Duration::hours(2)
    }

    #[test]
    fn phrases_name_the_waited_on_issue_and_nothing_else() {
        // Edge direction: the issue holding the body is the child.
        assert_eq!(
            phrase_blockers(R, 1, "Blocked by #5, #6 and other/x#7 while we wait for #9."),
            vec![k(5), k(6), NodeKey::new("other/x", 7)]
        );
        assert_eq!(phrase_blockers(R, 1, "- Depends on: #3\n- also #4"), vec![k(3)]);
        // Park-record spans are not prose; self-references and malformed
        // references add nothing.
        assert!(phrase_blockers(R, 1, "<!-- loom:park Blocked by: #12 -->").is_empty());
        assert!(phrase_blockers(R, 1, "Blocked by #1").is_empty());
        assert!(phrase_blockers(R, 1, "Blocked by nothing; see #4").is_empty());
        assert!(phrase_blockers(R, 1, "<!-- Blocked by #2").is_empty());
    }

    #[test]
    fn a_stacked_pr_waits_to_merge_after_its_base_pr_and_only_when_unambiguous() {
        let c = [
            pr(10, 100, Some("feature/a"), Some("main")),
            pr(11, 101, Some("feature/b"), Some("feature/a")),
        ];
        let got = stacked_parents(&c);
        assert_eq!(got, vec![(k(10), Some(vec![])), (k(11), Some(vec![k(10)]))]);

        // Ambiguous head (two PRs): record nothing, keep what was seen.
        let amb = [
            pr(10, 100, Some("feature/a"), Some("main")),
            pr(12, 102, Some("feature/a"), Some("main")),
            pr(11, 101, Some("feature/b"), Some("feature/a")),
        ];
        assert!(stacked_parents(&amb).contains(&(k(11), None)));
        // Two issues behind one PR number: also ambiguous.
        let shared = [
            pr(10, 100, Some("feature/a"), Some("main")),
            pr(13, 100, Some("feature/a"), Some("main")),
            pr(11, 101, Some("feature/b"), Some("feature/a")),
        ];
        assert!(stacked_parents(&shared).contains(&(k(11), None)));
        // An unread sibling PR may own the base: no claim of "no parent".
        let unread = [pr(11, 101, Some("b"), Some("x")), pr(14, 104, None, None)];
        assert!(stacked_parents(&unread).contains(&(k(11), None)));
        // Another repo's identically named head is not a parent.
        let mut other = pr(15, 105, Some("feature/a"), Some("main"));
        other.key = ItemKey::new("z/z", 15);
        let across = [other, pr(11, 101, Some("b"), Some("feature/a"))];
        assert!(stacked_parents(&across).contains(&(k(11), Some(vec![]))));
    }

    #[test]
    fn a_blocked_issue_gets_its_blocker_and_the_edge_is_first_known_when_read() {
        let m = Mock::default();
        m.issue(R, 2, blocked_issue("Blocked by #1"));
        m.issue(R, 1, json!({"state": "open"}));
        m.blocked_by.borrow_mut().insert(2, json!([]));
        let before = Utc::now();
        // The body was edited long before; the edge is still new to us.
        let (book, stats) =
            run(&m, DependencyBook::default(), &[cand(2, true), cand(1, false)], t0());
        let g = DependencyGraph {
            edges: book.edges.clone(),
            nodes: BTreeMap::new(),
        };
        assert_eq!(g.parents(&k(2), before), vec![], "invisible before it was read");
        let after = g.parents(&k(2), Utc::now() + Duration::seconds(1));
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].key, k(1));
        assert_eq!(after[0].sources, vec![EdgeSource::ParkRecord]);
        assert_eq!(stats, Stats { due: 2, closure: 0 });
    }

    #[test]
    fn epic_phases_wait_on_the_nearest_lower_phase_and_a_gap_is_skipped() {
        let m = Mock::default();
        for (n, phase) in [(1, 1), (2, 2), (3, 2), (4, 4)] {
            m.issue(R, n, blocked_issue(&format!("<!-- loom:epic:99:phase:{phase} -->")));
            m.blocked_by.borrow_mut().insert(n, json!([]));
        }
        let cands: Vec<_> = (1..=4).map(|n| cand(n, true)).collect();
        let (book, _) = run(&m, DependencyBook::default(), &cands, t0());
        let mut got: Vec<(u32, u32)> = book
            .edges
            .iter()
            .filter(|e| e.source == EdgeSource::EpicPhase)
            .map(|e| (e.child.issue, e.parent.issue))
            .collect();
        got.sort_unstable();
        // 2 and 3 (phase 2) wait on 1 (phase 1); 4 (phase 4) on both phase-2
        // members; phase 1 waits on nothing. Phases never wait on siblings.
        assert_eq!(got, vec![(2, 1), (3, 1), (4, 2), (4, 3)]);
    }

    #[test]
    fn a_parent_waits_on_its_sub_issues_never_the_reverse_and_never_siblings() {
        let m = Mock::default();
        m.issue(R, 5, json!({"sub_issues_summary": {"total": 2}, "labels": []}));
        m.blocked_by.borrow_mut().insert(5, json!([]));
        let subs = json!([
            {"number": 6, "repository_url": "https://api.github.com/repos/o/r", "state": "open"},
            {"number": 7, "repository_url": "https://api.github.com/repos/x/y",
             "state": "closed", "closed_at": "2026-10-01T00:00:00Z"},
            {"number": 8, "repository_url": "https://api.github.com/repos/o/r", "state": "open"}
        ]);
        m.subs.borrow_mut().insert(5, subs);
        let mut book = DependencyBook::default();
        // Sub-issue 8 already waits on the epic: recording 5 -> 8 would cycle.
        book.set(&k(8), EdgeSource::ParkRecord, &[k(5)], t0());
        let cands = [cand(5, true), cand(8, false), cand(6, false)];
        // First pass learns of the sub-issues; the second lists them.
        let (book, _) = run(&m, book, &cands, t0());
        assert!(book.has_subs(&k(5)));
        let (book, _) = run(&m, book, &cands, Utc::now() + Duration::seconds(REFRESH_SECS + 1));
        let subs: Vec<_> = book
            .edges
            .iter()
            .filter(|e| e.source == EdgeSource::SubIssue)
            .map(|e| (e.child.clone(), e.parent.clone()))
            .collect();
        assert_eq!(subs, vec![(k(5), k(6)), (k(5), NodeKey::new("x/y", 7))]);
        assert!(book.landed.contains_key(&NodeKey::new("x/y", 7)));
        assert!(!book.waits_on(&k(6), &k(8)) && !book.waits_on(&k(8), &k(6)));
    }

    #[test]
    fn failed_reads_retain_edges_and_a_successful_read_replaces_only_its_source() {
        let m = Mock::default();
        m.issue(R, 2, blocked_issue("Blocked by #1"));
        m.blocked_by.borrow_mut().insert(
            2,
            json!([
                {"number": 3, "repository_url": "https://api.github.com/repos/o/r", "state": "open"}
            ]),
        );
        let cands = [cand(2, true), cand(1, false), cand(3, false)];
        let (book, _) = run(&m, DependencyBook::default(), &cands, t0());
        assert_eq!(book.edges.len(), 2);
        let first: Vec<_> = book.edges.iter().map(|e| e.known_at).collect();

        // Every read now fails: both edges stand.
        let later = Utc::now() + Duration::seconds(REFRESH_SECS + 1);
        m.issues.borrow_mut().clear();
        m.blocked_by.borrow_mut().clear();
        let (book, _) = run(&m, book, &cands, later);
        assert_eq!(book.edges.len(), 2);

        // The native list now answers empty: only that source drops.
        m.blocked_by.borrow_mut().insert(2, json!([]));
        let (book, _) = run(&m, book, &cands, later + Duration::seconds(1));
        assert_eq!(book.edges.len(), 1);
        assert_eq!(book.edges[0].source, EdgeSource::ParkRecord);
        assert!(first.contains(&book.edges[0].known_at), "history is not re-dated");
    }

    #[test]
    fn a_pass_stays_inside_the_budget_the_interval_and_reuses_what_it_knows() {
        let m = Mock::default();
        let n = 30_u32;
        for i in 1..=n {
            // Each parked item waits on a closed, untracked parent 1000+i.
            m.issue(R, i, blocked_issue(&format!("<!-- loom:park Blocked by: #{} -->", 1000 + i)));
            m.issue(R, 1000 + i, json!({"state": "closed", "closed_at": "2026-10-01T00:00:00Z"}));
            m.blocked_by.borrow_mut().insert(i, json!([]));
        }
        let cands: Vec<_> = (1..=n).map(|i| cand(i, true)).collect();
        let (book, stats) = run(&m, DependencyBook::default(), &cands, t0());
        assert_eq!(stats.due, READ_BUDGET);
        // Closure reads are counted apart; the call total is both.
        assert!(stats.closure <= READ_BUDGET);
        assert_eq!(m.issue_calls.get() + m.other_calls.get(), stats.due + stats.closure);

        // Inside the interval the unread get their turn; the read wait.
        let (mut book, again) = run(&m, book, &cands, t0() + Duration::seconds(1));
        assert_eq!(again.due, READ_BUDGET);
        let mut closures = stats.closure + again.closure;
        for i in 2..10_i64 {
            let (b, st) = run(&m, book, &cands, t0() + Duration::seconds(i));
            assert!(st.due <= READ_BUDGET);
            closures += st.closure;
            book = b;
        }
        // All 2n pairs read once; each closed parent was read exactly once.
        assert_eq!(book.edges.len(), n as usize);
        assert_eq!(closures, n as usize);
        let calls = m.issue_calls.get() + m.other_calls.get();
        let (book, idle) = run(&m, book, &cands, t0() + Duration::seconds(30));
        assert_eq!(idle, Stats::default());
        assert_eq!(m.issue_calls.get() + m.other_calls.get(), calls);
        assert_eq!(book.edges.len(), n as usize);
    }

    #[test]
    fn an_empty_candidate_set_reads_nothing_and_leaves_the_book_empty() {
        let m = Mock::default();
        let (book, stats) = run(&m, DependencyBook::default(), &[], t0());
        assert!(book.edges.is_empty() && book.landed.is_empty());
        assert_eq!(stats, Stats::default());
        assert_eq!(m.issue_calls.get() + m.other_calls.get(), 0);
    }
}
