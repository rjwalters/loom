//! Blocker inheritance (#9244 liveness item 4).
//!
//! When a starred issue is blocked by another issue in the same repo (named
//! by `loom:blocked`, named as the incident behind a merge refusal, or the
//! repo's red-main fix), the blocker takes the star's place in the queue:
//! the liveness pass [`publish`]es it here per workspace root, and the work
//! finder's listing calls [`apply`], which marks the blocker
//! `operator_priority_inherited_from` and gives it the star's starred-at.
//! Dispatch then orders it exactly like a starred issue (keys 1 and 2 of
//! `candidate_cmp`) and the snapshot row shows it as inherited.
//!
//! The next pass that no longer sees the blocker blocking publishes a list
//! without it, and the next listing drops the mark: nothing to clean up.
//!
//! A blocker outside the `loom:issue` listing (#9268 sat in `loom:triage`) is
//! added to the ready rows from the pass's own read, like a starred row, so
//! it can be dispatched from Curator.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::work_finder::WorkItem;

/// One inherited star.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inherited {
    /// The blocker.
    pub number: u32,
    /// The starred issue it blocks.
    pub from: u32,
    /// The starred issue's starred-at, when known.
    pub starred_at: Option<String>,
    /// The blocker as the pass read it (labels, body, dates), for adding it
    /// when the ready listing does not carry it.
    pub item: WorkItem,
}

fn registry() -> &'static Mutex<HashMap<String, Vec<Inherited>>> {
    static REG: OnceLock<Mutex<HashMap<String, Vec<Inherited>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(root: &Path) -> String {
    root.display().to_string()
}

/// Replace the inherited stars for the workspace at `root`.
pub fn publish(root: &Path, list: Vec<Inherited>) {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if list.is_empty() {
        guard.remove(&key(root));
    } else {
        guard.insert(key(root), list);
    }
}

/// The inherited stars currently published for `root`.
#[must_use]
pub fn current(root: &Path) -> Vec<Inherited> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key(root))
        .cloned()
        .unwrap_or_default()
}

/// Mark (or add) every inherited blocker in `items`. Pure given `list`.
///
/// A listed blocker keeps its own starred-at when it is starred itself;
/// otherwise it takes the star's. An unlisted blocker is added unless it is
/// already claimed or on a Champion path, the same filter the starred
/// listing applies (`work_finder::operator_priority::merge_starred`).
pub fn apply_list(items: &mut Vec<WorkItem>, list: &[Inherited]) {
    for inh in list {
        if let Some(item) = items.iter_mut().find(|i| i.number == inh.number) {
            let own_star = item
                .labels
                .iter()
                .any(|l| l == crate::work_finder::OPERATOR_PRIORITY_LABEL);
            if !own_star {
                item.operator_priority_inherited_from = Some(inh.from);
                item.operator_priority_at.clone_from(&inh.starred_at);
            }
            continue;
        }
        let mut added = crate::work_finder::operator_priority::merge_starred(
            Vec::new(),
            vec![inh.item.clone()],
        );
        if let Some(mut item) = added.pop() {
            item.operator_priority_inherited_from = Some(inh.from);
            item.operator_priority_at.clone_from(&inh.starred_at);
            items.push(item);
        }
    }
}

/// [`apply_list`] with the list published for `root`. `None` (a listing
/// with no workspace root) is a no-op.
pub fn apply(root: Option<&Path>, items: &mut Vec<WorkItem>) {
    let Some(root) = root else {
        return;
    };
    let list = current(root);
    if !list.is_empty() {
        apply_list(items, &list);
    }
}
