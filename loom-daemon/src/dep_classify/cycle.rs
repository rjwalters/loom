//! Dependency-cycle detection — the Rust port of `detect-dependency-cycle.sh`'s
//! graph walk (epic #7810, PR 3).
//!
//! An issue blocked by a dependency can be waited on. An issue in a **cycle**
//! cannot — waiting is the one thing that will never resolve it — so Champion
//! must tell the two apart before deferring anything.
//!
//! # The invariant most at risk in a rewrite
//!
//! The walk memoises explored nodes, but **only when their subtree completed**:
//!
//! ```bash
//! local incomplete_before="$INCOMPLETE_COUNT"
//! if ! _walk "$ref" "$((depth + 1))" "$path $ref"; then return 1; fi
//! if [[ "$INCOMPLETE_COUNT" -eq "$incomplete_before" ]]; then
//!     _in_set "$ref" "$EXPLORED" || EXPLORED="$EXPLORED $ref"
//! fi
//! ```
//!
//! If a depth, node or step budget was hit anywhere inside that subtree, the
//! node is deliberately **not** marked explored — a different path may reach it
//! with budget left, and skipping it would miss a real cycle.
//!
//! Memoising on return is how one naturally writes a DFS, and it converts a
//! budget truncation into a **missed cycle** whose only symptom is an issue
//! deferred forever with no error anywhere. [`Walk::walk`] preserves the
//! shell's rule, and `a_truncated_subtree_is_not_memoised` pins it.
//!
//! # Budgets
//!
//! Defaults match the shell (`--max-depth 4`, `--max-nodes 25`,
//! `--max-steps 500`). They exist because this walk reaches the forge once per
//! node: an unbounded walk over a large dependency graph is a rate-limit
//! incident, not a slow command.

use std::collections::{HashMap, HashSet};

/// One node's forge state, as the shell cached it (state on line 1, body after).
#[derive(Debug, Clone)]
pub struct Node {
    pub state: String,
    pub body: String,
}

/// Walk limits. Each is `--flag`- and env-overridable in the CLI.
#[derive(Debug, Clone, Copy)]
pub struct Budgets {
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_steps: usize,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            max_depth: 4,
            max_nodes: 25,
            max_steps: 500,
        }
    }
}

/// Why the walk could not see the whole graph.
///
/// Recorded rather than returned as an error: a truncated walk still reports
/// any cycle it *did* find, and the caller needs to know the "no cycle" answer
/// was incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Truncation {
    Depth,
    Nodes,
    Steps,
}

impl Truncation {
    /// The token the shell wrote into `$TRUNCATED`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Truncation::Depth => "depth",
            Truncation::Nodes => "nodes",
            Truncation::Steps => "steps",
        }
    }
}

/// What a walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No cycle reachable within budget. Check [`Walk::is_complete`] before
    /// treating this as "no cycle exists".
    NoCycle,
    /// The closed loop, from its first recurrence, with the repeated node at
    /// both ends.
    Cycle(Vec<String>),
}

/// A dependency-graph walk.
///
/// `fetch` is injected so the traversal is testable without a forge — the shell
/// could only be exercised through a `gh` stub on `PATH`, which is a large part
/// of why its own suite was hard to trust.
pub struct Walk<F> {
    fetch: F,
    budgets: Budgets,
    cache: HashMap<String, Option<Node>>,
    explored: HashSet<String>,
    truncated: Vec<Truncation>,
    incomplete: usize,
    fetch_count: usize,
    step_count: usize,
    unreadable: Vec<String>,
}

impl<F> Walk<F>
where
    F: FnMut(&str) -> Option<Node>,
{
    pub fn new(fetch: F, budgets: Budgets) -> Self {
        Self {
            fetch,
            budgets,
            cache: HashMap::new(),
            explored: HashSet::new(),
            truncated: Vec::new(),
            incomplete: 0,
            fetch_count: 0,
            step_count: 0,
            unreadable: Vec::new(),
        }
    }

    /// Whether the walk saw the whole reachable graph.
    ///
    /// `false` means a `NoCycle` result is "no cycle **found**", not "no cycle
    /// exists" — the distinction a caller must not collapse.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.incomplete == 0
    }

    /// Truncation reasons encountered, deduplicated, in first-seen order.
    #[must_use]
    pub fn truncations(&self) -> &[Truncation] {
        &self.truncated
    }

    /// Nodes the forge could not be read for.
    #[must_use]
    pub fn unreadable(&self) -> &[String] {
        &self.unreadable
    }

    fn note_truncated(&mut self, why: Truncation) {
        if !self.truncated.contains(&why) {
            self.truncated.push(why);
        }
        self.incomplete += 1;
    }

    /// Fetch a node, memoising both hits and misses.
    ///
    /// A miss is cached too (the shell's `.miss` marker), so an unreadable node
    /// costs one forge call however many paths reach it.
    fn fetch_node(&mut self, node: &str) -> Option<Node> {
        if let Some(cached) = self.cache.get(node) {
            return cached.clone();
        }
        if self.fetch_count >= self.budgets.max_nodes {
            self.note_truncated(Truncation::Nodes);
            return None;
        }
        self.fetch_count += 1;

        let got = (self.fetch)(node);
        if got.is_none() {
            self.cache.insert(node.to_string(), None);
            if !self.unreadable.iter().any(|n| n == node) {
                self.unreadable.push(node.to_string());
            }
            self.incomplete += 1;
            return None;
        }
        self.cache.insert(node.to_string(), got.clone());
        got
    }

    /// Walk from `root`, returning the first cycle reachable within budget.
    pub fn run(&mut self, root: &str) -> Outcome {
        // The root must be in the cache before its body can be read.
        if self.fetch_node(root).is_none() {
            return Outcome::NoCycle;
        }
        match self.walk(root, 0, &[root.to_string()]) {
            Some(cycle) => Outcome::Cycle(cycle),
            None => Outcome::NoCycle,
        }
    }

    /// One DFS level. `Some(cycle)` propagates a find straight to the caller,
    /// matching the shell's `return 1`.
    fn walk(&mut self, node: &str, depth: usize, path: &[String]) -> Option<Vec<String>> {
        let body = self
            .cache
            .get(node)
            .and_then(|n| n.as_ref().map(|n| n.body.clone()))
            .unwrap_or_default();
        let default_repo = node.rsplit_once('#').map_or(node, |(r, _)| r).to_string();

        for r in super::refs::parse_dependency_refs(&body, &default_repo) {
            // Budget is checked BEFORE the step is spent, and a step is spent
            // per reference considered — not per node visited.
            if self.step_count >= self.budgets.max_steps {
                self.note_truncated(Truncation::Steps);
                return None;
            }
            self.step_count += 1;

            if r == node {
                continue;
            }

            if path.contains(&r) {
                return Some(cycle_segment(&r, path));
            }

            if self.explored.contains(&r) {
                continue;
            }

            if depth + 1 >= self.budgets.max_depth {
                // `continue`, not `return`: the rest of THIS node's references
                // are still worth examining; only the deeper walk is skipped.
                self.note_truncated(Truncation::Depth);
                continue;
            }

            if self.fetch_node(&r).is_none() {
                continue;
            }

            // Only an OPEN dependency can block. A closed one cannot hold a
            // cycle shut, so the walk does not descend into it.
            let is_open = self
                .cache
                .get(&r)
                .and_then(|n| n.as_ref().map(|n| n.state == "OPEN"))
                .unwrap_or(false);
            if !is_open {
                continue;
            }

            let incomplete_before = self.incomplete;

            let mut next = path.to_vec();
            next.push(r.clone());
            if let Some(cycle) = self.walk(&r, depth + 1, &next) {
                return Some(cycle);
            }

            // THE invariant: memoise only a subtree that completed. See the
            // module docs — memoising a truncated subtree turns a budget limit
            // into a missed cycle.
            if self.incomplete == incomplete_before {
                self.explored.insert(r);
            }
        }
        None
    }
}

/// The closed loop: from `reference`'s first occurrence in `path` to the end,
/// with `reference` repeated to close it.
///
/// `A B C B` reads as "B depends on C, which depends back on B".
#[must_use]
fn cycle_segment(reference: &str, path: &[String]) -> Vec<String> {
    let start = path.iter().position(|p| p == reference).unwrap_or(0);
    let mut out: Vec<String> = path[start..].to_vec();
    out.push(reference.to_string());
    out
}

#[cfg(test)]
mod tests;
