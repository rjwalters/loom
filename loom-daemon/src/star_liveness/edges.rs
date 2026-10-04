//! The parent/child edge resolver for star propagation (#10012 §1).
//!
//! A star on a parent P reaches its children because the operator starred P
//! (#9974's star authority): the daemon never stars on its own judgment. This
//! module answers the one question every propagation path needs — *which
//! issues are P's children* — from every machine-readable link Loom and its
//! humans write today, and nothing else. Pure: the caller supplies the
//! issue text it has already read.
//!
//! # Edge sources (same repo only)
//!
//! | Source | Read from | [`EdgeSource`] |
//! |---|---|---|
//! | `<!-- loom:park Blocked by: #C … -->` | P's body | [`EdgeSource::ParkRecord`] |
//! | `Blocked by` / `Depends on` / `Requires #C` on a `loom:blocked` P | P's body | [`EdgeSource::BlockedBy`] |
//! | `- [ ] #C` / `- [x] #C` task-list entry (not a ticked `## Dependencies` item, #10024) | P's body | [`EdgeSource::TaskList`] |
//! | `<!-- loom:epic:P:phase:n -->` | C's body | [`EdgeSource::EpicPhase`] |
//! | `<!-- loom:parent #P -->` | C's body | [`EdgeSource::ParentMarker`] |
//! | a line starting `Part of #P` | C's body (issues only) | [`EdgeSource::PartOf`] |
//! | `[Parent #P]` title prefix | C's title | [`EdgeSource::TitlePrefix`] |
//! | native GitHub sub-issue | the forge | [`EdgeSource::SubIssue`] |
//!
//! A prose mention (`see #12`, `like #12 did`) never creates an edge. A
//! reference to another repo (`owner/other#12`, or a URL into it) never
//! creates an edge either: stars do not cross repos.
//!
//! # Closure
//!
//! [`descendants`] walks the edges from every starred root, transitively up
//! to [`super::collect::MAX_INHERIT_DEPTH`], with a cycle guard. A child
//! reached from several starred ancestors inherits from the one with the
//! **earliest** starred-at (#10012 AC 6), so it keeps the star while any of
//! them is starred. Every child is returned, not only the first: the old
//! blocker path let only the first open blocker inherit (AC 3).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::OnceLock;

use regex::Regex;

use super::landing::BLOCKED_LABEL;

/// Where an edge came from. Ordered by precedence: when two sources name the
/// same `(parent, child)` pair, [`resolve`] keeps the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeSource {
    /// `<!-- loom:park Blocked by: #C -->` in P's body (Builder decomposition).
    ParkRecord,
    /// A dependency phrase in a `loom:blocked` P's body.
    BlockedBy,
    /// `<!-- loom:epic:P:phase:n -->` in C's body (Champion epic phases).
    EpicPhase,
    /// A native forge sub-issue link.
    SubIssue,
    /// A `- [ ] #C` task-list entry in P's body.
    TaskList,
    /// `<!-- loom:parent #P -->` in C's body.
    ParentMarker,
    /// A line starting `Part of #P` in an issue's body.
    PartOf,
    /// A `[Parent #P]` title prefix (the legacy Builder recipe).
    TitlePrefix,
    /// A liveness blocker the landing classifier names for P (a `loom:blocked`
    /// blocker, the open incident behind a merge refusal, or the red-main
    /// fix); supplied by [`super::collect`], never parsed here.
    LandingBlocker,
}

/// One parent → child link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub parent: u32,
    pub child: u32,
    pub source: EdgeSource,
}

/// The text of one issue (or PR) as the caller read it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Node<'a> {
    pub number: u32,
    pub title: &'a str,
    pub body: &'a str,
    pub labels: &'a [String],
    pub is_pull_request: bool,
}

/// One reference: optional `owner/repo` prefix + `#N`, or an issues URL.
const REF: &str = r"(?:[A-Za-z0-9._-]+/[A-Za-z0-9._-]+)?#[0-9]+|https?://[^\s)\]>,]+/issues/[0-9]+";

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static edge pattern"))
}

fn ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, REF)
}

fn park_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"(?s)<!--\s*loom:park\s+(.*?)-->")
}

fn park_reason_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r#"reason="[^"]*""#)
}

fn task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"(?m)^\s*[-*+]\s+\[[ xX]\]\s+({REF})\b"))
            .expect("static task-list pattern")
    })
}

fn checked_task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"(?m)^\s*[-*+]\s+\[[xX]\]\s+({REF})\b"))
            .expect("static checked-task pattern")
    })
}

/// Same-repo task-list children of `body`, minus each **checked** item of its
/// `## Dependencies` section. That section is a dependency list, not
/// containment: a ticked item there is a satisfied prerequisite and never a
/// blocker (#10024), so it inherits nothing. A ticked item anywhere else, or
/// the same number also listed outside the section, is still a child.
fn task_children(body: &str, slug: &str) -> BTreeSet<u32> {
    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    for c in task_re().captures_iter(body) {
        if let Some(n) = c.get(1).and_then(|m| same_repo_number(m.as_str(), slug)) {
            *counts.entry(n).or_default() += 1;
        }
    }
    let section = crate::dep_recheck::named::dependencies_section(body);
    for c in checked_task_re().captures_iter(&section) {
        if let Some(n) = c.get(1).and_then(|m| same_repo_number(m.as_str(), slug)) {
            if let Some(k) = counts.get_mut(&n) {
                *k = k.saturating_sub(1);
            }
        }
    }
    counts
        .into_iter()
        .filter(|(_, k)| *k > 0)
        .map(|(n, _)| n)
        .collect()
}

fn parent_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"<!--\s*loom:parent\s+({REF})\s*-->"))
            .expect("static parent-marker pattern")
    })
}

fn part_of_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"(?m)^\s*[*_]{{0,2}}Part of[*_:]*\s+({REF})\b"))
            .expect("static part-of pattern")
    })
}

fn title_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"^\s*\[Parent\s+({REF})\]")).expect("static title-prefix pattern")
    })
}

/// The issue number `reference` names in `slug`, or `None` for another repo
/// (or something that is not a reference). Case-insensitive on the slug.
#[must_use]
pub fn same_repo_number(reference: &str, slug: &str) -> Option<u32> {
    let r = reference.trim();
    let (repo, num) = if let Some(rest) = r
        .strip_prefix("https://")
        .or_else(|| r.strip_prefix("http://"))
    {
        // host/owner/repo/issues/N
        let mut parts = rest.split('/');
        let _host = parts.next()?;
        let owner = parts.next()?;
        let name = parts.next()?;
        if parts.next()? != "issues" {
            return None;
        }
        (Some(format!("{owner}/{name}")), parts.next()?)
    } else {
        let (repo, num) = r.split_once('#')?;
        ((!repo.is_empty()).then(|| repo.to_string()), num)
    };
    if repo.is_some_and(|repo| !repo.eq_ignore_ascii_case(slug)) {
        return None;
    }
    num.parse().ok().filter(|n| *n > 0)
}

fn numbers<'t>(refs: impl Iterator<Item = &'t str>, slug: &str) -> BTreeSet<u32> {
    refs.filter_map(|r| same_repo_number(r, slug)).collect()
}

/// Children `node` names in its own body, as P (park records, dependency
/// phrases when it is `loom:blocked`, task-list entries), ascending by number.
#[must_use]
pub fn child_edges(slug: &str, node: &Node<'_>) -> Vec<Edge> {
    let p = node.number;
    let mut out = Vec::new();
    let mut push = |children: BTreeSet<u32>, source| {
        out.extend(children.into_iter().filter(|c| *c != p).map(|child| Edge {
            parent: p,
            child,
            source,
        }));
    };
    let park: BTreeSet<u32> = park_re()
        .captures_iter(node.body)
        .filter_map(|c| c.get(1))
        .flat_map(|m| {
            let inner = park_reason_re().replace_all(m.as_str(), "").into_owned();
            ref_re()
                .find_iter(&inner)
                .map(|r| r.as_str().to_string())
                .collect::<Vec<_>>()
        })
        .filter_map(|r| same_repo_number(&r, slug))
        .collect();
    push(park, EdgeSource::ParkRecord);
    if node.labels.iter().any(|l| l == BLOCKED_LABEL) {
        let deps = crate::dep_classify::refs::parse_dependency_refs(node.body, slug);
        push(numbers(deps.iter().map(String::as_str), slug), EdgeSource::BlockedBy);
    }
    push(task_children(node.body, slug), EdgeSource::TaskList);
    out
}

/// Parents `node` names in its own body or title, as C.
#[must_use]
pub fn parent_edges(slug: &str, node: &Node<'_>) -> Vec<Edge> {
    let c = node.number;
    let mut out = Vec::new();
    let mut push = |parents: BTreeSet<u32>, source| {
        out.extend(parents.into_iter().filter(|p| *p != c).map(|parent| Edge {
            parent,
            child: c,
            source,
        }));
    };
    if let Some((parent, _phase)) =
        crate::epic_supervisor::forge::parse_epic_phase_marker(node.body)
    {
        push(BTreeSet::from([parent]), EdgeSource::EpicPhase);
    }
    let markers = parent_marker_re()
        .captures_iter(node.body)
        .filter_map(|m| m.get(1).map(|m| m.as_str()));
    push(numbers(markers, slug), EdgeSource::ParentMarker);
    if !node.is_pull_request {
        let part_of = part_of_re()
            .captures_iter(node.body)
            .filter_map(|m| m.get(1).map(|m| m.as_str()));
        push(numbers(part_of, slug), EdgeSource::PartOf);
    }
    let title = title_prefix_re()
        .captures_iter(node.title)
        .filter_map(|m| m.get(1).map(|m| m.as_str()));
    push(numbers(title, slug), EdgeSource::TitlePrefix);
    out
}

/// Every edge among `nodes` plus the forge's native sub-issue links
/// (`(parent, child)` pairs, already known to be same-repo), deduplicated by
/// `(parent, child)` with the highest-precedence source kept, and sorted.
#[must_use]
pub fn resolve(slug: &str, nodes: &[Node<'_>], sub_issues: &[(u32, u32)]) -> Vec<Edge> {
    let mut best: BTreeMap<(u32, u32), EdgeSource> = BTreeMap::new();
    let all = nodes
        .iter()
        .flat_map(|n| {
            child_edges(slug, n)
                .into_iter()
                .chain(parent_edges(slug, n))
        })
        .chain(sub_issues.iter().map(|&(parent, child)| Edge {
            parent,
            child,
            source: EdgeSource::SubIssue,
        }));
    for e in all.filter(|e| e.parent != e.child) {
        best.entry((e.parent, e.child))
            .and_modify(|s| *s = (*s).min(e.source))
            .or_insert(e.source);
    }
    best.into_iter()
        .map(|((parent, child), source)| Edge {
            parent,
            child,
            source,
        })
        .collect()
}

/// One starred root the closure starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub number: u32,
    /// Its starred-at (RFC 3339), when known.
    pub starred_at: Option<String>,
}

/// How one descendant inherits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inheritance {
    pub child: u32,
    /// Its parent on the winning path (what the row nests under).
    pub via: u32,
    /// The starred root whose star it carries (what an inherited marker names).
    pub root: u32,
    /// The root's starred-at.
    pub starred_at: Option<String>,
    /// Edges from the root (1 = a direct child).
    pub depth: usize,
}

fn at_key(at: Option<&str>) -> (bool, Option<chrono::DateTime<chrono::Utc>>, String) {
    let parsed = at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    // Unknown starred-at sorts last; an unparseable one by its text.
    (at.is_none(), parsed, at.unwrap_or_default().to_string())
}

/// Whether `a` should win over `b` for the same child: earliest starred-at,
/// then the shallower path, then the lower root, then the lower parent.
fn better(a: &Inheritance, b: &Inheritance) -> bool {
    let k = |i: &Inheritance| (at_key(i.starred_at.as_deref()), i.depth, i.root, i.via);
    k(a) < k(b)
}

/// The transitive closure of `edges` from `roots`, at most `max_depth` edges
/// deep. A root is never its own (or another root's) descendant: a starred
/// issue keeps its own star. A cycle terminates (each root's walk visits a
/// node once).
#[must_use]
pub fn descendants(roots: &[Root], edges: &[Edge], max_depth: usize) -> BTreeMap<u32, Inheritance> {
    let mut children: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for e in edges {
        children.entry(e.parent).or_default().insert(e.child);
    }
    let starred: BTreeSet<u32> = roots.iter().map(|r| r.number).collect();
    let mut out: BTreeMap<u32, Inheritance> = BTreeMap::new();
    for root in roots {
        let mut seen = BTreeSet::from([root.number]);
        let mut queue = VecDeque::from([(root.number, 0usize)]);
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for &child in children.get(&node).into_iter().flatten() {
                if !seen.insert(child) || starred.contains(&child) {
                    continue;
                }
                let cand = Inheritance {
                    child,
                    via: node,
                    root: root.number,
                    starred_at: root.starred_at.clone(),
                    depth: depth + 1,
                };
                match out.get(&child) {
                    Some(cur) if !better(&cand, cur) => {}
                    _ => {
                        out.insert(child, cand);
                    }
                }
                queue.push_back((child, depth + 1));
            }
        }
    }
    out
}
