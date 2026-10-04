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
//!    another repo's daemon does too. The label is added with one provenance
//!    comment in the loom-ui intent marker shape
//!    ([`provenance_marker`]: `inherited_from=owner/repo#N level=2
//!    requested_at=<source's starred-at>`), so the starred-at timeline read
//!    orders the blocker at its source's time. It is removed once no source
//!    of that level reaches the issue — but only after a **complete** walk:
//!    a capped or partly unreadable pass adds and never removes. The
//!    operator's own label is never copied, so the cap count stays exact.
//! 4. **Cap** ([`over_cap`]). A level over its cap
//!    (`autonomous.operatorPriority.levelCaps`, default from the table) is
//!    reported in the digest. Nothing is refused: loom-ui enforces the cap at
//!    click time, and a human labeling on GitHub directly is the operator's
//!    call. Inherited labels never count.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use super::edges::{self, Node};
use super::forge::StarForge;
use super::intents::{INTENT_MARKER_PREFIX, LABEL_FIELD};
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

/// The provenance intent id: safe inside a marker, the same on every host.
#[must_use]
pub fn provenance_id(level: u8, child: &Key, source: &Key) -> String {
    let raw = format!("hp{level}-{}-from-{}.{}", child.1, source.0, source.1);
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

/// The provenance marker for `reach`, in the loom-ui intent marker shape so
/// the starred-at timeline read takes the source's `requested_at`.
#[must_use]
pub fn provenance_marker(reach: &Reach, inherited_label: &str) -> String {
    let at = reach
        .requested_at
        .as_deref()
        .map(|t| format!(" requested_at={t}"))
        .unwrap_or_default();
    format!(
        "{INTENT_MARKER_PREFIX}{} action=star{at} {LABEL_FIELD}{inherited_label} inherited_from={} level={} -->",
        provenance_id(reach.level, &reach.key, &reach.source),
        display(&reach.source),
        reach.level
    )
}

/// The provenance comment for `reach`.
#[must_use]
pub fn provenance_comment(reach: &Reach, row: &PriorityLevel, inherited_label: &str) -> String {
    let via = if reach.via == reach.source {
        String::new()
    } else {
        format!(" (via {})", display(&reach.via))
    };
    format!(
        "{}\n{} Inherits {} from {}, which it blocks{via}. The daemon removes \
         `{inherited_label}` once no level-{} issue it blocks remains (#10307).",
        provenance_marker(reach, inherited_label),
        row.glyph,
        row.name,
        display(&reach.source),
        reach.level
    )
}

/// One label write the pass makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// Add `label` to `key` with a provenance comment for `reach`.
    Add {
        reach: Box<Reach>,
        label: &'static str,
    },
    /// Remove `label` from `key`: no source of its level reaches it.
    Remove { key: Key, label: &'static str },
}

/// One open item currently carrying an inherited label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub key: Key,
    pub label: &'static str,
    pub item: RestIssue,
}

/// The writes that bring the inherited labels to `closure`. `holders` are
/// the open items listed carrying an inherited label (only from listings
/// that succeeded). With `complete` false nothing is removed. A PR keeps a
/// label (Builder copies priority labels onto its PR) while an issue it
/// closes or advances is reached at that level or carries it on its own.
#[must_use]
pub fn plan_writes(
    table: &[PriorityLevel],
    closure: &Closure,
    sources: &[Source],
    holders: &[Holder],
    complete: bool,
) -> Vec<Write> {
    let mut writes = Vec::new();
    let has = |item: &RestIssue, label: &str| item.labels.iter().any(|l| l == label);
    for reach in closure.reached.values() {
        let Some(label) = operator_levels::row(table, reach.level).and_then(|r| r.inherited_label)
        else {
            continue;
        };
        if !has(&reach.issue, label) {
            writes.push(Write::Add {
                reach: Box::new(reach.clone()),
                label,
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
    for h in holders {
        let Some(row) = operator_levels::by_inherited_label(table, h.label) else {
            continue;
        };
        let keep = if h.item.is_pull_request {
            linked_issues(&h.item)
                .into_iter()
                .any(|n| level_of(&(h.key.0.clone(), n)) >= row.level)
        } else {
            closure
                .reached
                .get(&h.key)
                .is_some_and(|r| r.level == row.level)
        };
        if !keep {
            writes.push(Write::Remove {
                key: h.key.clone(),
                label: h.label,
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
            Some(Err(e)) => {
                log::debug!(
                    "star_liveness: level walk could not read {}'s dependencies: {e}",
                    display(key)
                );
                Vec::new()
            }
            None => Vec::new(),
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
    let planned = plan_writes(table, &closure, &sources, &holders, complete);
    let mut writes = 0;
    if write {
        for w in &planned {
            match apply_write(&mut reader, table, w) {
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

fn apply_write(
    reader: &mut ForgeReader<'_>,
    table: &[PriorityLevel],
    w: &Write,
) -> anyhow::Result<()> {
    match w {
        Write::Add { reach, label } => {
            let forge = reader
                .forges
                .get_mut(&reach.key.0)
                .ok_or_else(|| anyhow::anyhow!("no forge for {}", reach.key.0))?;
            forge.add_label(reach.key.1, label)?;
            log::info!(
                "star_liveness: {} inherits level {} from {} ({label})",
                display(&reach.key),
                reach.level,
                display(&reach.source)
            );
            let Some(row) = operator_levels::row(table, reach.level) else {
                return Ok(());
            };
            let id_marker = format!(
                "{INTENT_MARKER_PREFIX}{} ",
                provenance_id(reach.level, &reach.key, &reach.source)
            );
            let comments = forge.comments(reach.key.1)?;
            let me = forge.self_login();
            let posted = comments
                .iter()
                .any(|c| c.body.contains(&id_marker) && super::trust::trusted(c, me.as_deref()));
            if !posted {
                forge.post_comment(reach.key.1, &provenance_comment(reach, row, label))?;
            }
            Ok(())
        }
        Write::Remove { key, label } => {
            let forge = reader
                .forges
                .get_mut(&key.0)
                .ok_or_else(|| anyhow::anyhow!("no forge for {}", key.0))?;
            forge.remove_label(key.1, label)?;
            log::info!(
                "star_liveness: {} no longer blocks a level source; removed {label}",
                display(key)
            );
            Ok(())
        }
    }
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
    }
}
