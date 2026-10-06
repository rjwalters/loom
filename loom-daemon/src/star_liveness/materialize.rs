//! The inherited star as the label (#10012 §2–§3).
//!
//! The walk ([`super::collect`]) finds every open descendant of a starred
//! issue. This module turns that into label writes, so every reader of the
//! label (Curator, Builder's starred-first query, the dashboard, `forge
//! starred`) sees what the work finder already knew:
//!
//! - **Add.** A reached child without the star gets it, with the inherited
//!   audit comment ([`super::inherited_star::marker`]) whose `requested_at` is
//!   the root's starred-at, so it orders at the parent's star time
//!   everywhere. A child whose star the operator took off by hand stays
//!   unstarred ([`super::inherited_star::operator_removed`]). Only a link
//!   the issue text records is written: a landing-only edge (a blocker named
//!   in a comment, a merge refusal's incident, the red-main fix) stays an
//!   in-memory ordering.
//! - **Remove.** A starred item whose **latest** star event is a daemon
//!   inherited marker ([`super::inherited_star::owner`]) loses the star when
//!   the root that marker names has lost its own, and no starred ancestor
//!   reaches it any more. A root that closed while starred keeps its label,
//!   so its children keep theirs. An operator's own star is never removed.
//!
//! # Which starred items the walk starts from
//!
//! Once a child carries the label it is in the starred listing too. Treating
//! it as a root would keep it starred forever, so [`classify`] splits the
//! listing by owner: operator stars are roots, inherited stars are walked as
//! children of their roots. An inherited star whose root is still starred
//! but no longer reaches it stays (the removal rule keys on the root's star,
//! not on the edge).
//!
//! # Budget
//!
//! - Ownership needs one timeline read per item whose only operator label is
//!   the star, cached until the item's `updated_at` moves, at most
//!   [`MAX_OWNER_READS_PER_PASS`] per repo per pass (least recently checked
//!   first). An item not checked yet is a root and is never removed.
//! - At most [`MAX_STAR_WRITES_PER_PASS`] adds and removes per repo per pass,
//!   so a starred epic with many children converges over several passes.
//! - Nothing is removed after an incomplete walk (a deferred child, a failed
//!   listing): an unread ancestor may still reach the item.
//! - Every forge call goes through [`StarForge`], whose production
//!   implementation honors the rate-limit breaker per call; the pass skips
//!   entirely while the breaker is tripped, and only repos that pass
//!   `write_scope::gate_root` are visited.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::forge::StarForge;
use super::inherited_star::{self, Owner, RootState};
use crate::forge_listing::RestIssue;
use crate::work_finder::OPERATOR_PRIORITY_LABEL;

/// Most ownership (timeline) reads per repo per pass.
pub const MAX_OWNER_READS_PER_PASS: usize = 20;
/// Most star writes (an add or a remove) per repo per pass.
pub const MAX_STAR_WRITES_PER_PASS: usize = 10;

/// What is known about who owns one item's star.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    /// Read this pass, or cached under the item's current `updated_at`.
    Fresh(Owner),
    /// Read before the item last changed, and not re-read yet.
    Stale(Owner),
    /// Never read (or the read failed).
    Unknown,
}

#[derive(Debug, Clone)]
struct Cached {
    updated_at: Option<String>,
    owner: Owner,
    /// The [`OwnerCache::clock`] value of the read.
    checked: u64,
}

/// Star ownership across passes, keyed by (repo, number).
#[derive(Debug, Default)]
pub struct OwnerCache {
    entries: HashMap<(String, u32), Cached>,
    clock: u64,
}

/// Whether propagation could own `labels`' star: the star is its only
/// operator label (a higher level is always the operator's).
#[must_use]
pub fn may_be_inherited(labels: &[String]) -> bool {
    labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL)
        && crate::operator_levels::own_level_in(crate::operator_levels::table(), labels) <= 1
}

/// Who owns each starred issue's star, reading at most `max_reads`
/// timelines. Items that cannot be inherited are [`Owner::Operator`].
pub fn owners(
    forge: &mut dyn StarForge,
    cache: &mut OwnerCache,
    slug: &str,
    starred: &[RestIssue],
    max_reads: usize,
) -> BTreeMap<u32, Known> {
    cache.clock += 1;
    let live: BTreeSet<u32> = starred.iter().map(|i| i.number).collect();
    cache
        .entries
        .retain(|(s, n), _| s != slug || live.contains(n));
    let mut out = BTreeMap::new();
    let mut due: Vec<(u64, u32)> = Vec::new();
    for i in starred {
        if !may_be_inherited(&i.labels) {
            out.insert(i.number, Known::Fresh(Owner::Operator));
            continue;
        }
        match cache.entries.get(&(slug.to_string(), i.number)) {
            Some(c) if i.updated_at.is_some() && c.updated_at == i.updated_at => {
                out.insert(i.number, Known::Fresh(c.owner));
            }
            Some(c) => {
                out.insert(i.number, Known::Stale(c.owner));
                due.push((c.checked, i.number));
            }
            None => {
                out.insert(i.number, Known::Unknown);
                due.push((0, i.number));
            }
        }
    }
    due.sort_unstable();
    for (_, n) in due.into_iter().take(max_reads) {
        match forge.star_events(n) {
            Ok(events) => {
                let owner = inherited_star::owner(&events);
                let updated_at = starred
                    .iter()
                    .find(|i| i.number == n)
                    .and_then(|i| i.updated_at.clone());
                cache.entries.insert(
                    (slug.to_string(), n),
                    Cached {
                        updated_at,
                        owner,
                        checked: cache.clock,
                    },
                );
                out.insert(n, Known::Fresh(owner));
            }
            Err(e) => {
                log::debug!("star_liveness: reading the star events of {slug}#{n} failed: {e}")
            }
        }
    }
    out
}

/// The starred listing split by owner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classified {
    /// Where the walk starts: operator stars, and any star whose owner is
    /// not known fresh as inherited.
    pub roots: BTreeSet<u32>,
    /// Inherited stars whose root still carries a star (or could not be
    /// read): walked as children, kept when nothing reaches them.
    pub held: BTreeMap<u32, u32>,
    /// Inherited stars whose root lost its star: walked as children, removed
    /// when nothing reaches them and the walk was complete.
    pub orphaned: BTreeMap<u32, u32>,
}

/// Split `known` (every open starred issue) by owner. `root_state` answers
/// for the root an inherited marker names.
pub fn classify(
    known: &BTreeMap<u32, Known>,
    mut root_state: impl FnMut(u32) -> RootState,
) -> Classified {
    let mut out = Classified::default();
    for (&n, k) in known {
        match *k {
            Known::Fresh(Owner::Inherited { root }) if root != n => match root_state(root) {
                RootState::Unstarred => {
                    out.orphaned.insert(n, root);
                }
                RootState::Starred | RootState::Unknown => {
                    out.held.insert(n, root);
                }
            },
            // A stale inherited owner may have been re-starred by hand since:
            // never removed on it, but not a root either.
            Known::Stale(Owner::Inherited { root }) if root != n => {
                out.held.insert(n, root);
            }
            _ => {
                out.roots.insert(n);
            }
        }
    }
    out
}

/// One star to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Add {
    pub child: u32,
    /// The starred root the child inherits from.
    pub root: u32,
    /// The root's starred-at.
    pub starred_at: Option<String>,
}

/// One inherited star to take back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remove {
    pub child: u32,
    /// The root its marker names (which lost its star).
    pub root: u32,
}

/// One repo's writes for this pass, in the order they are made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub adds: Vec<Add>,
    pub removes: Vec<Remove>,
}

impl Plan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adds.is_empty() && self.removes.is_empty()
    }
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applied {
    pub added: usize,
    pub removed: usize,
    /// Reached children left unstarred because the operator unstarred them.
    pub respected: usize,
    pub failed: usize,
    /// Writes left for a later pass by the cap.
    pub deferred: usize,
}

/// Make `plan`'s writes, at most `max_writes` of them.
///
/// An add first reads the child's comments: when its latest trusted
/// inherited marker is a star, someone took that star off by hand (or a peer
/// host starred it a moment ago and this pass read a stale listing), and the
/// child is left alone. A remove posts the `action=unstar` marker before it
/// takes the label off, so a later pass can tell the daemon removed it.
pub fn apply(forge: &mut dyn StarForge, slug: &str, plan: &Plan, max_writes: usize) -> Applied {
    let mut done = Applied::default();
    let mut writes = 0usize;
    let me = forge.self_login();
    for add in &plan.adds {
        if writes >= max_writes {
            done.deferred += 1;
            continue;
        }
        match forge.comments(add.child) {
            Ok(comments) if inherited_star::operator_removed(&comments, me.as_deref()) => {
                done.respected += 1;
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("star_liveness: reading {slug}#{} comments failed ({e}); not starring it this pass", add.child);
                done.failed += 1;
                continue;
            }
        }
        writes += 1;
        match super::parent_link::write_inherited_star(
            forge,
            add.root,
            add.child,
            add.starred_at.as_deref(),
        ) {
            Ok(()) => {
                done.added += 1;
                log::info!(
                    "star_liveness: starred {slug}#{} (inherited from #{})",
                    add.child,
                    add.root
                );
            }
            Err(e) => {
                done.failed += 1;
                log::warn!("star_liveness: starring {slug}#{} failed: {e:#}", add.child);
            }
        }
    }
    for rm in &plan.removes {
        if writes >= max_writes {
            done.deferred += 1;
            continue;
        }
        writes += 1;
        let result = forge
            .post_comment(rm.child, &inherited_star::unstar_marker(rm.root, rm.child))
            .and_then(|()| forge.remove_label(rm.child, OPERATOR_PRIORITY_LABEL));
        match result {
            Ok(()) => {
                done.removed += 1;
                log::info!(
                    "star_liveness: unstarred {slug}#{} (#{} lost its star)",
                    rm.child,
                    rm.root
                );
            }
            Err(e) => {
                done.failed += 1;
                log::warn!("star_liveness: unstarring {slug}#{} failed: {e:#}", rm.child);
            }
        }
    }
    done
}
