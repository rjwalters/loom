//! Operator priority levels in the liveness pass (#10307 §2–§4): blocker
//! inheritance as a derived label, and the cap.
//!
//! # What it does, every pass
//!
//! 1. **Sources.** Every open issue carrying the operator label of a level
//!    that has an inherited label ([`crate::operator_levels`]: level 2,
//!    `loom:operator-high-priority`, today), in every repo this host
//!    manages.
//! 2. **Closure** ([`closure`]). Every open issue that blocks a source,
//!    directly or transitively, inherits the source's level. Edges are the
//!    explicit blocking links ([`blocking_targets`]): park records, the
//!    named blockers of a `loom:blocked` issue (phrases and the `##
//!    Dependencies` checklist), the liveness blockers the landing classifier
//!    names, and the forge's native "blocked by" dependencies. **Every**
//!    open blocker inherits, not only the first. Containment alone never
//!    does ([`super::edges::blocks_for_level`]): an epic's task list or
//!    sub-issues stay where they are. Unlike the plain star, a level
//!    **crosses repos** into any repo this host manages; a blocker in an
//!    unmanaged repo is reported ([`Outcome::unfollowed`]), not followed. A
//!    cycle guard (one visit per node per level) and
//!    [`MAX_LEVEL_INHERIT_DEPTH`] bound the walk; closed issues and PRs drop
//!    out. A node reached by several sources keeps the highest level, then
//!    the earliest-starred source.
//! 3. **Derived label** ([`plan_writes`]). Each reached issue whose own
//!    level is lower carries the level's inherited label
//!    (`loom:high-priority-inherited`), so every reader that queries labels
//!    (Curator, Builder's `gh issue list`, `pr-queue`, loom-ui) sees it, and
//!    another repo's daemon does too. **Provenance lives in the blocker's
//!    issue body** ([`provenance_marker`]: `<!-- loom:priority-inherited
//!    inherited_from=owner/repo#N level=2 requested_at=<source's starred-at>
//!    id=… -->`, the shape loom-ui reads, #10307), one marker per level,
//!    replaced in place when the source changes ([`with_marker`]). The body
//!    is written **before** the label, and the label is skipped when that
//!    write fails, so the `labeled` webhook carries the provenance and a
//!    failed write retries whole next pass. The work finder orders the
//!    blocker at the marker's `requested_at` ([`inherited_requested_at`]).
//!    The label and its marker are removed once no source of that level
//!    reaches the issue — but only after a **complete** walk (a capped or
//!    partly unreadable pass, including a failed dependency read, adds and
//!    never removes), and only by a host that manages the marker's source
//!    repo: a host with a narrower managed set cannot recompute a
//!    cross-repo label, so it neither removes it nor rewrites its marker.
//!    The operator's own label is never copied, so the cap count stays
//!    exact.
//! 4. **Cap** ([`over_cap`]). A level over its cap
//!    (`autonomous.operatorPriority.levelCaps`, default from the table) is
//!    reported in the digest. Nothing is refused: loom-ui enforces the cap at
//!    click time, and a human labeling on GitHub directly is the operator's
//!    call. Inherited labels never count.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use super::edges::{self, Node};
use super::forge::StarForge;
use super::landing::BLOCKED_LABEL;
use crate::forge_listing::RestIssue;
use crate::operator_levels::{self, PriorityLevel};
use crate::types::star_liveness::OverCapLevel;

/// How deep a level follows a chain of blockers. Deeper than the star's
/// [`super::collect::MAX_INHERIT_DEPTH`]: a level is rare and capped, and
/// what blocks the operator's top issue is often several links away.
pub const MAX_LEVEL_INHERIT_DEPTH: usize = 6;

/// Most single-issue forge reads (cache misses, issue and dependency reads
/// alike) one pass's level walk makes across every repo. A walk that hits
/// it is incomplete: it adds labels but removes none.
pub const MAX_LEVEL_READS_PER_PASS: usize = 150;

/// A node: (`owner/repo` as this host spells the managed repo, number).
pub type Key = (String, u32);

/// `owner/repo#N`.
#[must_use]
pub fn display(key: &Key) -> String {
    format!("{}#{}", key.0, key.1)
}

/// One source: an open issue carrying a level's operator label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub key: Key,
    pub level: u8,
    /// Its starred-at at that level, when known.
    pub requested_at: Option<String>,
    pub issue: RestIssue,
}

/// How one blocker inherits a level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    pub key: Key,
    pub level: u8,
    /// The source whose level it carries.
    pub source: Key,
    /// Its parent on the winning path (the issue it directly blocks).
    pub via: Key,
    /// The source's starred-at.
    pub requested_at: Option<String>,
    /// Edges from the source (1 = a direct blocker).
    pub depth: usize,
    /// The blocker as read this pass.
    pub issue: RestIssue,
}

/// Reads issues across the repos this host manages, with a per-pass cache
/// and read budget.
pub trait Reader {
    /// The managed slug as this host spells it, when `slug` is managed.
    fn managed(&self, slug: &str) -> Option<String>;
    /// One issue or PR (`None` when missing, unreadable, or over budget).
    fn issue(&mut self, key: &Key) -> Option<RestIssue>;
    /// Its open native "blocked by" dependencies.
    fn blocked_by(&mut self, key: &Key) -> Vec<(String, u32)>;
    /// Whether any read was refused by the budget or failed this pass.
    fn incomplete(&self) -> bool;
}

/// The issues `issue` (in `slug`) names as blocking it, in any repo,
/// deduplicated: the #10012 resolver's same-repo edges of a kind that
/// carries a level, `landing` (the liveness blockers the classifier named),
/// and every park-record or named-blocker reference into another repo.
/// Never the issue itself.
#[must_use]
pub fn blocking_targets(slug: &str, issue: &RestIssue, landing: &[u32]) -> Vec<(String, u32)> {
    let body = issue.body.as_deref().unwrap_or_default();
    let node = Node {
        number: issue.number,
        title: issue.title.as_deref().unwrap_or_default(),
        body,
        labels: &issue.labels,
        is_pull_request: issue.is_pull_request,
    };
    let mut out: BTreeSet<(String, u32)> = edges::child_edges(slug, &node)
        .into_iter()
        .filter(|e| edges::blocks_for_level(e.source))
        .map(|e| (slug.to_string(), e.child))
        .collect();
    out.extend(landing.iter().map(|n| (slug.to_string(), *n)));
    let mut refs = edges::park_refs(body);
    if issue.labels.iter().any(|l| l == BLOCKED_LABEL) {
        refs.extend(crate::dep_classify::refs::parse_named_blocker_refs(body, slug));
    }
    out.extend(refs.iter().filter_map(|r| edges::ref_target(r, slug)));
    out.retain(|(repo, n)| !(repo.eq_ignore_ascii_case(slug) && *n == issue.number));
    out.into_iter().collect()
}

/// The walk's result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Closure {
    /// Every open blocker that inherits a level, at its highest level.
    pub reached: BTreeMap<Key, Reach>,
    /// Blockers in repos this host does not manage (`owner/repo#N`).
    pub unfollowed: BTreeSet<String>,
}

fn at_key(at: Option<&str>) -> (bool, String) {
    (at.is_none(), at.unwrap_or_default().to_string())
}

fn open_issue(i: &RestIssue) -> bool {
    !i.is_pull_request && i.state.eq_ignore_ascii_case("open")
}

/// Walk from every source, highest level first. `landing` is the liveness
/// blockers the classifier named per node this pass. Pure over `reader`.
pub fn closure(
    reader: &mut dyn Reader,
    table: &[PriorityLevel],
    sources: &[Source],
    landing: &HashMap<Key, Vec<u32>>,
    max_depth: usize,
) -> Closure {
    let mut out = Closure::default();
    let mut levels: Vec<u8> = sources.iter().map(|s| s.level).collect();
    levels.sort_unstable_by(|a, b| b.cmp(a));
    levels.dedup();
    for level in levels {
        let mut group: Vec<&Source> = sources.iter().filter(|s| s.level == level).collect();
        group.sort_by(|a, b| {
            (at_key(a.requested_at.as_deref()), &a.key)
                .cmp(&(at_key(b.requested_at.as_deref()), &b.key))
        });
        // One visit per node per level: the cycle guard. Breadth-first from
        // every source at once, so a node is reached at its shallowest depth
        // from the earliest-starred source.
        let mut seen: BTreeSet<Key> = group.iter().map(|s| s.key.clone()).collect();
        let mut queue: VecDeque<(RestIssue, Key, usize, &Source)> = group
            .iter()
            .map(|s| (s.issue.clone(), s.key.clone(), 0, *s))
            .collect();
        while let Some((issue, key, depth, src)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let inherits = landing.get(&key).cloned().unwrap_or_default();
            let mut targets = blocking_targets(&key.0, &issue, &inherits);
            targets.extend(reader.blocked_by(&key));
            for (repo, n) in targets {
                let Some(managed) = reader.managed(&repo) else {
                    out.unfollowed.insert(format!("{repo}#{n}"));
                    continue;
                };
                let child: Key = (managed, n);
                if !seen.insert(child.clone()) {
                    continue;
                }
                let Some(read) = reader.issue(&child) else {
                    continue;
                };
                if !open_issue(&read) {
                    continue;
                }
                // A node at this level or above on its own keeps its own
                // label; the walk still goes through it.
                let own = operator_levels::own_level_in(table, &read.labels);
                let better =
                    own < level && out.reached.get(&child).is_none_or(|cur| cur.level < level);
                if better {
                    out.reached.insert(
                        child.clone(),
                        Reach {
                            key: child.clone(),
                            level,
                            source: src.key.clone(),
                            via: key.clone(),
                            requested_at: src.requested_at.clone(),
                            depth: depth + 1,
                            issue: read.clone(),
                        },
                    );
                }
                queue.push_back((read, child, depth + 1, src));
            }
        }
    }
    out
}

/// The provenance id: safe inside a marker, and the same on every host
/// whatever case it spells the source repo in.
#[must_use]
pub fn provenance_id(level: u8, child: &Key, source: &Key) -> String {
    let raw = format!("hp{level}-{}-from-{}.{}", child.1, source.0.to_ascii_lowercase(), source.1);
    let mut id: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
                c
            } else {
                '.'
            }
        })
        .collect();
    while id.contains("--") {
        id = id.replace("--", "-");
    }
    id.chars().take(128).collect()
}

/// The token every level provenance marker carries (loom-ui reads it).
pub const PROVENANCE_TOKEN: &str = "loom:priority-inherited";

/// The body marker for `reach` (#10307): `inherited_from`, `level` and
/// `requested_at` are the fields loom-ui reads; `id` is stable per (level,
/// blocker, source).
#[must_use]
pub fn provenance_marker(reach: &Reach) -> String {
    let at = reach
        .requested_at
        .as_deref()
        .map(|t| format!(" requested_at={t}"))
        .unwrap_or_default();
    format!(
        "<!-- {PROVENANCE_TOKEN} inherited_from={} level={}{at} id={} -->",
        display(&reach.source),
        reach.level,
        provenance_id(reach.level, &reach.key, &reach.source)
    )
}

/// One provenance marker found in a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyMarker {
    /// Byte range of the whole `<!-- … -->` comment.
    pub range: std::ops::Range<usize>,
    pub text: String,
    pub level: Option<u8>,
    /// `owner/repo#N`, or a bare `#N` meaning the item's own repo.
    pub inherited_from: Option<String>,
    pub requested_at: Option<String>,
}

impl BodyMarker {
    /// The source's repo, `None` for a bare `#N` (the item's own repo).
    #[must_use]
    pub fn source_repo(&self) -> Option<&str> {
        let from = self.inherited_from.as_deref()?;
        let (repo, _) = from.split_once('#')?;
        (!repo.is_empty()).then_some(repo)
    }
}

/// Every provenance marker in `body`, in order. Any HTML comment that
/// carries [`PROVENANCE_TOKEN`] counts; its fields may come in any order.
#[must_use]
pub fn body_markers(body: &str) -> Vec<BodyMarker> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = body[from..].find("<!--") {
        let start = from + i;
        let Some(j) = body[start..].find("-->") else {
            break;
        };
        let end = start + j + 3;
        let text = &body[start..end];
        if text.contains(PROVENANCE_TOKEN) {
            let field = |key: &str| {
                text.split_whitespace()
                    .find_map(|w| w.strip_prefix(key))
                    .map(str::to_string)
            };
            out.push(BodyMarker {
                range: start..end,
                text: text.to_string(),
                level: field("level=").and_then(|l| l.parse().ok()),
                inherited_from: field("inherited_from="),
                requested_at: field("requested_at="),
            });
        }
        from = end;
    }
    out
}

/// The provenance marker for `level` in `body`, when there is one.
#[must_use]
pub fn marker_for(body: &str, level: u8) -> Option<BodyMarker> {
    body_markers(body)
        .into_iter()
        .find(|m| m.level == Some(level))
}

/// `body` with `level`'s provenance set to `marker`, or removed when `None`.
/// The first marker of that level is replaced in place and any duplicates
/// dropped; with none, the marker is appended after a blank line. Every
/// other byte is kept, and removing an appended marker takes its blank line
/// with it.
#[must_use]
pub fn with_marker(body: &str, level: u8, marker: Option<&str>) -> String {
    let mine: Vec<std::ops::Range<usize>> = body_markers(body)
        .into_iter()
        .filter(|m| m.level == Some(level))
        .map(|m| m.range)
        .collect();
    let Some(first) = mine.first().cloned() else {
        return match marker {
            None => body.to_string(),
            Some(m) if body.is_empty() => m.to_string(),
            Some(m) => format!("{body}\n\n{m}"),
        };
    };
    let mut out = body.to_string();
    for r in mine.iter().rev() {
        if *r == first {
            if let Some(m) = marker {
                out.replace_range(r.clone(), m);
                continue;
            }
        }
        let at_end = r.end == out.len();
        let mut start = r.start;
        if at_end && out[..start].ends_with("\n\n") {
            start -= 2;
        }
        out.replace_range(start..r.end, "");
    }
    out
}

/// The starred-at a blocker inherits through its body marker (#10307): the
/// `requested_at` of the marker for the level of an inherited label `item`
/// carries, when that level is above its own. The work finder orders it
/// there, as it would its source.
#[must_use]
pub fn inherited_requested_at(
    table: &[PriorityLevel],
    labels: &[String],
    body: Option<&str>,
) -> Option<String> {
    let level = operator_levels::inherited_level_in(table, labels);
    if level == 0 || operator_levels::own_level_in(table, labels) >= level {
        return None;
    }
    marker_for(body?, level)?.requested_at
}

/// One write the pass makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// Add `label` to `reach.key`. With `provenance`, the body marker is
    /// written first, and the label is skipped when that write fails.
    Add {
        reach: Box<Reach>,
        label: &'static str,
        provenance: bool,
    },
    /// The label is there but its body marker is missing or names a source
    /// this host manages that no longer wins: rewrite the marker in place.
    Provenance { reach: Box<Reach> },
    /// Remove `label` from `key`: no source of its level reaches it. With
    /// `provenance` (an issue), its body marker for `level` goes first.
    Remove {
        key: Key,
        label: &'static str,
        level: u8,
        provenance: bool,
    },
}

/// Whether this host may write or remove provenance over `existing`: there
/// is none, or it names a source in a repo this host manages (a bare `#N`
/// is the item's own repo, which it does).
fn may_override(existing: Option<&BodyMarker>, managed: &dyn Fn(&str) -> bool) -> bool {
    existing.is_none_or(|m| m.source_repo().is_none_or(managed))
}

/// One open item currently carrying an inherited label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub key: Key,
    pub label: &'static str,
    pub item: RestIssue,
}

/// The writes that bring the inherited labels and their body markers to
/// `closure`. `holders` are the open items listed carrying an inherited
/// label (only from listings that succeeded). With `complete` false nothing
/// is removed. `managed` answers whether this host manages a repo: an issue
/// whose marker names a source in a repo it does not manage keeps its label
/// and marker (another host can recompute them; this one cannot). A PR
/// keeps a label (Builder copies priority labels onto its PR) while an
/// issue it closes or advances is reached at that level, carries it on its
/// own, or keeps it for that reason.
#[must_use]
pub fn plan_writes(
    table: &[PriorityLevel],
    closure: &Closure,
    sources: &[Source],
    holders: &[Holder],
    complete: bool,
    managed: &dyn Fn(&str) -> bool,
) -> Vec<Write> {
    let mut writes = Vec::new();
    let has = |item: &RestIssue, label: &str| item.labels.iter().any(|l| l == label);
    for reach in closure.reached.values() {
        let Some(label) = operator_levels::row(table, reach.level).and_then(|r| r.inherited_label)
        else {
            continue;
        };
        let existing = marker_for(reach.issue.body.as_deref().unwrap_or_default(), reach.level);
        let stale = existing
            .as_ref()
            .is_none_or(|m| m.text != provenance_marker(reach));
        let provenance = stale && may_override(existing.as_ref(), managed);
        if !has(&reach.issue, label) {
            writes.push(Write::Add {
                reach: Box::new(reach.clone()),
                label,
                provenance,
            });
        } else if provenance {
            writes.push(Write::Provenance {
                reach: Box::new(reach.clone()),
            });
        }
    }
    if !complete {
        return writes;
    }
    let source_level: HashMap<&Key, u8> = sources.iter().map(|s| (&s.key, s.level)).collect();
    let level_of = |key: &Key| -> u8 {
        closure
            .reached
            .get(key)
            .map_or(0, |r| r.level)
            .max(source_level.get(key).copied().unwrap_or(0))
    };
    // Issues first, so a PR can see which of its issues keep a label.
    let mut kept: BTreeSet<(Key, &'static str)> = BTreeSet::new();
    let (issues, prs): (Vec<&Holder>, Vec<&Holder>) =
        holders.iter().partition(|h| !h.item.is_pull_request);
    for h in issues.into_iter().chain(prs) {
        let Some(row) = operator_levels::by_inherited_label(table, h.label) else {
            continue;
        };
        let keep = if h.item.is_pull_request {
            linked_issues(&h.item).into_iter().any(|n| {
                let issue = (h.key.0.clone(), n);
                level_of(&issue) >= row.level || kept.contains(&(issue, h.label))
            })
        } else {
            let marker = marker_for(h.item.body.as_deref().unwrap_or_default(), row.level);
            closure
                .reached
                .get(&h.key)
                .is_some_and(|r| r.level == row.level)
                || !may_override(marker.as_ref(), managed)
        };
        if keep {
            kept.insert((h.key.clone(), h.label));
        } else {
            writes.push(Write::Remove {
                key: h.key.clone(),
                label: h.label,
                level: row.level,
                provenance: !h.item.is_pull_request,
            });
        }
    }
    writes
}

/// Issues a PR declares it closes or contributes to.
fn linked_issues(pr: &RestIssue) -> Vec<u32> {
    let body = pr.body.as_deref().unwrap_or_default();
    let mut refs: BTreeSet<u64> = crate::merge_pr::refs::closing_refs(body)
        .into_iter()
        .collect();
    refs.extend(crate::merge_pr::refs::partial_increment_refs(body));
    refs.into_iter()
        .filter_map(|n| u32::try_from(n).ok())
        .collect()
}

/// Every level over its cap, given each level's open sources (issues only).
#[must_use]
pub fn over_cap(
    table: &[PriorityLevel],
    sources: &[Source],
    cap: impl Fn(u8) -> Option<usize>,
) -> Vec<OverCapLevel> {
    table
        .iter()
        .filter_map(|row| {
            let cap = cap(row.level)?;
            let mut mine: Vec<&Source> = sources.iter().filter(|s| s.level == row.level).collect();
            if mine.len() <= cap {
                return None;
            }
            mine.sort_by(|a, b| {
                (at_key(a.requested_at.as_deref()), &a.issue.created_at, &a.key).cmp(&(
                    at_key(b.requested_at.as_deref()),
                    &b.issue.created_at,
                    &b.key,
                ))
            });
            Some(OverCapLevel {
                level: row.level,
                label: row.operator_label.to_string(),
                cap,
                count: mine.len(),
                issues: mine.iter().map(|s| display(&s.key)).collect(),
            })
        })
        .collect()
}

/// The production [`Reader`]: one [`StarForge`] per managed repo, a per-pass
/// issue cache seeded from the listings, and a shared read budget.
pub struct ForgeReader<'f> {
    /// Lower-cased slug → the slug as this host spells it.
    pub managed: HashMap<String, String>,
    pub forges: HashMap<String, Box<dyn StarForge + 'f>>,
    pub cache: HashMap<Key, Option<RestIssue>>,
    pub reads: usize,
    pub budget: usize,
    pub failed: bool,
}

impl<'f> ForgeReader<'f> {
    #[must_use]
    pub fn new(managed: HashMap<String, String>, budget: usize) -> Self {
        Self {
            managed,
            forges: HashMap::new(),
            cache: HashMap::new(),
            reads: 0,
            budget,
            failed: false,
        }
    }

    fn spend(&mut self) -> bool {
        if self.reads >= self.budget {
            self.failed = true;
            return false;
        }
        self.reads += 1;
        true
    }
}

impl Reader for ForgeReader<'_> {
    fn managed(&self, slug: &str) -> Option<String> {
        self.managed.get(&slug.to_ascii_lowercase()).cloned()
    }

    fn issue(&mut self, key: &Key) -> Option<RestIssue> {
        if let Some(hit) = self.cache.get(key) {
            return hit.clone();
        }
        if !self.spend() {
            return None;
        }
        let read = match self.forges.get_mut(&key.0).map(|f| f.issue(key.1)) {
            Some(Ok(read)) => read,
            Some(Err(e)) => {
                log::debug!("star_liveness: level walk could not read {}: {e}", display(key));
                self.failed = true;
                return None;
            }
            None => {
                self.failed = true;
                return None;
            }
        };
        self.cache.insert(key.clone(), read.clone());
        read
    }

    fn blocked_by(&mut self, key: &Key) -> Vec<(String, u32)> {
        if !self.spend() {
            return Vec::new();
        }
        match self.forges.get_mut(&key.0).map(|f| f.blocked_by(key.1)) {
            Some(Ok(v)) => v,
            // Not fatal to removals: a forge that answers the dependency
            // endpoint with an error (rather than a 404) would otherwise
            // freeze every stale label forever. Native dependencies are the
            // rarest edge; the body links still count.
            // A failed read (5xx, secondary limit, the breaker) makes the
            // walk incomplete, so this pass removes nothing. A forge without
            // the endpoint answers 404, which `cached_get` already maps to
            // "no dependencies".
            Some(Err(e)) => {
                log::debug!(
                    "star_liveness: level walk could not read {}'s dependencies: {e}",
                    display(key)
                );
                self.failed = true;
                Vec::new()
            }
            None => {
                self.failed = true;
                Vec::new()
            }
        }
    }

    fn incomplete(&self) -> bool {
        self.failed
    }
}

/// What one pass's level step produced.
#[derive(Debug, Default)]
pub struct Outcome {
    pub closure: Closure,
    pub sources: Vec<Source>,
    pub over_cap: Vec<OverCapLevel>,
    /// Whether the walk and every listing were complete (removals ran).
    pub complete: bool,
    /// Label writes that succeeded.
    pub writes: usize,
    /// One liveness row per reached blocker (the caller keeps the ones it
    /// does not already have, and annotates those it does).
    pub rows: Vec<(Key, super::collect::Evaluated)>,
    /// Per reached blocker, the work item the in-memory registry publishes:
    /// its labels plus the inherited label, at the source's starred-at.
    pub items: Vec<(Key, crate::work_finder::WorkItem)>,
}

/// One managed repo: (forge slug, workspace root).
pub type RepoRef = (String, std::path::PathBuf);

/// [`run_with`] over the production level table.
pub fn run(
    repos: &[RepoRef],
    open_forge: &mut dyn FnMut(&std::path::Path, &str) -> Box<dyn StarForge>,
    landing: &HashMap<Key, Vec<u32>>,
    starred_at: &HashMap<Key, Option<String>>,
    caps: &dyn Fn(u8) -> Option<usize>,
    write: bool,
    host: &str,
) -> Outcome {
    run_with(
        operator_levels::table(),
        repos,
        open_forge,
        landing,
        starred_at,
        caps,
        write,
        host,
    )
}

/// The level step of one pass against `table`: list sources and holders in
/// every managed repo, walk, write the derived labels (when `write`), and
/// report the cap.
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    table: &[PriorityLevel],
    repos: &[RepoRef],
    open_forge: &mut dyn FnMut(&std::path::Path, &str) -> Box<dyn StarForge>,
    landing: &HashMap<Key, Vec<u32>>,
    starred_at: &HashMap<Key, Option<String>>,
    caps: &dyn Fn(u8) -> Option<usize>,
    write: bool,
    host: &str,
) -> Outcome {
    let levelled: Vec<&PriorityLevel> = table
        .iter()
        .filter(|r| r.inherited_label.is_some())
        .collect();
    if levelled.is_empty() || repos.is_empty() {
        return Outcome {
            complete: true,
            ..Outcome::default()
        };
    }
    let managed: HashMap<String, String> = repos
        .iter()
        .map(|(s, _)| (s.to_ascii_lowercase(), s.clone()))
        .collect();
    let mut reader = ForgeReader::new(managed, MAX_LEVEL_READS_PER_PASS);
    let mut listings_ok = true;
    let mut sources: Vec<Source> = Vec::new();
    let mut holders: Vec<Holder> = Vec::new();
    for (slug, root) in repos {
        let mut forge = open_forge(root, slug);
        for row in &levelled {
            match forge.list_open(row.operator_label) {
                Ok(rows) => {
                    for r in rows.into_iter().filter(open_issue) {
                        let key = (slug.clone(), r.number);
                        reader.cache.insert(key.clone(), Some(r.clone()));
                        sources.push(Source {
                            requested_at: starred_at.get(&key).cloned().flatten(),
                            key,
                            level: row.level,
                            issue: r,
                        });
                    }
                }
                Err(e) => {
                    log::debug!(
                        "star_liveness: listing {} in {slug} failed: {e}",
                        row.operator_label
                    );
                    listings_ok = false;
                }
            }
            let Some(label) = row.inherited_label else {
                continue;
            };
            match forge.list_open(label) {
                Ok(rows) => holders.extend(
                    rows.into_iter()
                        .filter(|r| r.state.eq_ignore_ascii_case("open"))
                        .map(|item| Holder {
                            key: (slug.clone(), item.number),
                            label,
                            item,
                        }),
                ),
                Err(e) => {
                    log::debug!("star_liveness: listing {label} in {slug} failed: {e}");
                    listings_ok = false;
                }
            }
        }
        reader.forges.insert(slug.clone(), forge);
    }

    let closure = closure(&mut reader, table, &sources, landing, MAX_LEVEL_INHERIT_DEPTH);
    let complete = listings_ok && !reader.incomplete();
    let is_managed = |slug: &str| reader.managed(slug).is_some();
    let planned = plan_writes(table, &closure, &sources, &holders, complete, &is_managed);
    let mut writes = 0;
    if write {
        for w in &planned {
            match apply_write(&mut reader, w) {
                Ok(()) => writes += 1,
                Err(e) => {
                    log::warn!("star_liveness: level label write failed ({e}); retrying next pass")
                }
            }
        }
    }
    let over = over_cap(table, &sources, caps);
    let mut rows = Vec::new();
    let mut items = Vec::new();
    for reach in closure.reached.values() {
        let Some(label) = operator_levels::row(table, reach.level).and_then(|r| r.inherited_label)
        else {
            continue;
        };
        rows.push((reach.key.clone(), row_for(table, reach, &reader, host)));
        let mut item = super::collect::work_item(&reach.issue);
        if !item.labels.iter().any(|l| l == label) {
            item.labels.push(label.to_string());
        }
        item.operator_priority_at.clone_from(&reach.requested_at);
        items.push((reach.key.clone(), item));
    }
    Outcome {
        closure,
        sources,
        over_cap: over,
        complete,
        writes,
        rows,
        items,
    }
}

fn apply_write(reader: &mut ForgeReader<'_>, w: &Write) -> anyhow::Result<()> {
    let slug = match w {
        Write::Add { reach, .. } | Write::Provenance { reach } => &reach.key.0,
        Write::Remove { key, .. } => &key.0,
    };
    let forge = reader
        .forges
        .get_mut(slug)
        .ok_or_else(|| anyhow::anyhow!("no forge for {slug}"))?;
    match w {
        Write::Add {
            reach,
            label,
            provenance,
        } => {
            // Provenance first: a failed body write skips the label, so the
            // whole add retries next pass.
            if *provenance {
                set_marker(
                    forge.as_mut(),
                    &reach.key,
                    reach.level,
                    Some(&provenance_marker(reach)),
                )?;
            }
            forge.add_label(reach.key.1, label)?;
            log::info!(
                "star_liveness: {} inherits level {} from {} ({label})",
                display(&reach.key),
                reach.level,
                display(&reach.source)
            );
            Ok(())
        }
        Write::Provenance { reach } => {
            set_marker(forge.as_mut(), &reach.key, reach.level, Some(&provenance_marker(reach)))
        }
        Write::Remove {
            key,
            label,
            level,
            provenance,
        } => {
            if *provenance {
                set_marker(forge.as_mut(), key, *level, None)?;
            }
            forge.remove_label(key.1, label)?;
            log::info!(
                "star_liveness: {} no longer blocks a level source; removed {label}",
                display(key)
            );
            Ok(())
        }
    }
}

/// Read-modify-write `key`'s body so its level-`level` marker is `marker`.
/// The body is re-read just before the write, and nothing is written when
/// the marker is already right.
fn set_marker(
    forge: &mut dyn StarForge,
    key: &Key,
    level: u8,
    marker: Option<&str>,
) -> anyhow::Result<()> {
    let current = forge
        .issue(key.1)?
        .ok_or_else(|| anyhow::anyhow!("{} is gone", display(key)))?
        .body
        .unwrap_or_default();
    let next = with_marker(&current, level, marker);
    if next != current {
        forge.set_body(key.1, &next)?;
    }
    Ok(())
}

/// The liveness row for a blocker reached only by a level: classified from
/// its own facts (its named blockers' open state from this pass's reads).
fn row_for(
    table: &[PriorityLevel],
    reach: &Reach,
    reader: &ForgeReader<'_>,
    host: &str,
) -> super::collect::Evaluated {
    use super::landing::{classify, BlockerRef, StarFacts};
    let slug = &reach.key.0;
    let issue = &reach.issue;
    let blockers: Vec<BlockerRef> = if issue.labels.iter().any(|l| l == BLOCKED_LABEL) {
        let body = issue.body.as_deref().unwrap_or_default();
        crate::dep_classify::refs::parse_named_blocker_refs(body, slug)
            .into_iter()
            .filter_map(|r| edges::ref_target(&r, slug))
            .map(|(repo, n)| {
                let same = repo.eq_ignore_ascii_case(slug);
                let managed = reader.managed(&repo);
                let open = managed
                    .as_ref()
                    .and_then(|m| reader.cache.get(&(m.clone(), n)))
                    .and_then(|hit| hit.as_ref().map(open_issue));
                BlockerRef {
                    display: if same {
                        format!("#{n}")
                    } else {
                        format!("{repo}#{n}")
                    },
                    number: same.then_some(n),
                    open,
                    cross_repo_managed: (!same).then_some(managed.is_some()),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut item = super::collect::work_item(issue);
    if let Some(label) = operator_levels::row(table, reach.level).and_then(|r| r.inherited_label) {
        if !item.labels.iter().any(|l| l == label) {
            item.labels.push(label.to_string());
        }
    }
    let facts = StarFacts {
        repo: slug.clone(),
        managed: true,
        issue: super::landing::ItemFacts {
            labels: item.labels.clone(),
            ..super::collect::item_facts(issue)
        },
        blockers,
        host: host.to_string(),
        ..StarFacts::default()
    };
    let landing = classify(&facts);
    super::collect::Evaluated {
        fingerprint: super::progress::fingerprint(&item.labels, None),
        facts,
        landing,
        starred_at: reach.requested_at.clone(),
        inherited_from: None,
        item,
        level_inherited_from: Some(display(&reach.source)),
        inherited_level: reach.level,
        inherited_label: operator_levels::row(table, reach.level).and_then(|r| r.inherited_label),
    }
}
