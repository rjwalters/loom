//! Which labels travel from a parent to its children (#10012 §6).
//!
//! One small table ([`RULES`]): label, add mode, removal mode, and whether a
//! linked PR gets it too. Every other label **never propagates**; the
//! registry audit test keeps that list honest, so a label added to
//! `defaults/labels.json` later must be classified here (as a rule, or by its
//! kind in [`NEVER_KINDS`] / by name in [`NEVER_LABELS`]) before it builds.
//! The label registry (#10013) is meant to absorb this table into its
//! reserved `propagate` field; until then this is the one place it lives.
//!
//! [`plan`] is the pure decision for one child; it writes nothing. The pass
//! that applies it to the forge is the §2 materialization slice (built on
//! #9975's `forge star` path). The star's own provenance and multi-ancestor
//! bookkeeping stay in [`super::inherited_star`]; this table only says that
//! the star is one of the labels that travels, and how.
//!
//! The `<!-- loom:main-red-fix -->` body marker is copied at creation time by
//! `create-issue.sh --parent` ([`super::parent_link::child_body`]). It is not
//! a label, so nothing here (or in any later pass) edits bodies.

use crate::work_finder::OPERATOR_PRIORITY_LABEL;

/// The outside-submission gate label.
pub const EXTERNAL_LABEL: &str = "external";
/// The tier family prefix.
pub const TIER_PREFIX: &str = "tier:";

/// Which labels a rule covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// Exactly this label.
    Exact(&'static str),
    /// Every label starting with this prefix (a mutually exclusive family).
    Family(&'static str),
}

impl Match {
    #[must_use]
    pub fn covers(self, label: &str) -> bool {
        match self {
            Match::Exact(l) => label == l,
            Match::Family(p) => label.starts_with(p),
        }
    }
}

/// When a child gets the label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMode {
    /// Whenever an ancestor carries it and the child does not.
    Always,
    /// Only as a default: the child carries no member of the family yet.
    /// Never overwrites a child's own choice.
    DefaultIfFamilyAbsent,
}

/// When propagation takes the label back off a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// Once no ancestor carries it any more, and only a copy whose provenance
    /// is propagation (never one a human applied to the child).
    WithParent,
    /// Never: once copied, the child owns it.
    Never,
}

/// One propagation rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub label: Match,
    pub add: AddMode,
    pub removal: Removal,
    /// A PR linked to the child (or the parent) gets it too (§5).
    pub to_prs: bool,
}

/// The whole table (§6). Direction is always parent → child.
pub const RULES: &[Rule] = &[
    // The star: down to issues and PRs, removed with the parent's star.
    Rule {
        label: Match::Exact(OPERATOR_PRIORITY_LABEL),
        add: AddMode::Always,
        removal: Removal::WithParent,
        to_prs: true,
    },
    // A child of an unapproved outside submission must not bypass the
    // maintainer gate; approving the parent (removing the label) frees it.
    Rule {
        label: Match::Exact(EXTERNAL_LABEL),
        add: AddMode::Always,
        removal: Removal::WithParent,
        to_prs: false,
    },
    // A default only. Guide still owns re-tiering.
    Rule {
        label: Match::Family(TIER_PREFIX),
        add: AddMode::DefaultIfFamilyAbsent,
        removal: Removal::Never,
        to_prs: false,
    },
];

/// Registry kinds that never propagate, whatever the label: per-item state
/// (holds, claims, lifecycle, PR lanes), proposal kinds, structure, size and
/// resource weight. Decomposition exists to separate the automatable part
/// from the rest, so a hold or claim on the parent says nothing about a child;
/// `loom:heavy` would false-positive on the light children.
pub const NEVER_KINDS: &[&str] = &[
    "workflow",
    "claim",
    "pr-lane",
    "proposal",
    "hold",
    "structural",
    "size",
    "resource",
];

/// Labels of an otherwise-propagating kind that never propagate by this
/// table.
pub const NEVER_LABELS: &[&str] = &[
    // Retired; no role applies it.
    "loom:urgent",
    // Priority levels (#10307) travel to *blockers* by their own pass, with
    // body provenance; containment never carries a level.
    "loom:operator-high-priority",
    "loom:high-priority-inherited",
];

/// The rule covering `label`, or `None` when it never propagates.
#[must_use]
pub fn rule_for(label: &str) -> Option<&'static Rule> {
    if NEVER_LABELS.contains(&label) {
        return None;
    }
    RULES.iter().find(|r| r.label.covers(label))
}

/// The child as the pass sees it.
#[derive(Debug, Clone, Copy)]
pub struct Child<'a> {
    /// Its labels now.
    pub labels: &'a [String],
    /// It is a PR (linked to the descendant set), not an issue.
    pub is_pr: bool,
    /// Labels it carries whose latest provenance is propagation (a trusted
    /// daemon marker), as opposed to a human's or role's own write. Only
    /// these may ever be removed.
    pub inherited: &'a [String],
}

/// One write the pass would make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Add(String),
    Remove(String),
}

fn has(labels: &[String], l: &str) -> bool {
    labels.iter().any(|x| x == l)
}

/// The writes that bring `child` in line with its ancestors.
///
/// - `ancestors`: the labels of every ancestor that reaches the child by an
///   edge ([`super::edges`]), **nearest first**, closed ones included (a
///   parent that closed still carries its labels; AC 5).
/// - `complete`: the walk read every ancestor. When it did not, nothing is
///   removed (an unreadable parent may still carry the label).
///
/// Adds come before removals, each in [`RULES`] order, so the output is the
/// same on every host.
#[must_use]
pub fn plan(ancestors: &[Vec<String>], child: Child<'_>, complete: bool) -> Vec<Action> {
    let mut adds = Vec::new();
    let mut removes = Vec::new();
    for rule in RULES {
        if child.is_pr && !rule.to_prs {
            continue;
        }
        let carried = |labels: &[String]| -> Vec<String> {
            labels
                .iter()
                .filter(|l| rule.label.covers(l) && rule_for(l) == Some(rule))
                .cloned()
                .collect()
        };
        match rule.add {
            AddMode::Always => {
                let mut wanted: Vec<String> = ancestors.iter().flat_map(|a| carried(a)).collect();
                wanted.sort();
                wanted.dedup();
                adds.extend(wanted.iter().filter(|l| !has(child.labels, l)).cloned());
                if rule.removal == Removal::WithParent && complete {
                    removes.extend(
                        carried(child.labels)
                            .into_iter()
                            .filter(|l| has(child.inherited, l) && !wanted.contains(l)),
                    );
                }
            }
            AddMode::DefaultIfFamilyAbsent => {
                if !carried(child.labels).is_empty() {
                    continue;
                }
                // The nearest ancestor that carries the family decides; one
                // carrying several members is ambiguous and decides nothing.
                if let Some(mut found) =
                    ancestors.iter().map(|a| carried(a)).find(|f| !f.is_empty())
                {
                    if found.len() == 1 {
                        adds.push(found.remove(0));
                    }
                }
            }
        }
    }
    adds.into_iter()
        .map(Action::Add)
        .chain(removes.into_iter().map(Action::Remove))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::label_registry::Registry;

    const STAR: &str = OPERATOR_PRIORITY_LABEL;

    fn v(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| (*s).to_string()).collect()
    }

    fn issue<'a>(labels: &'a [String], inherited: &'a [String]) -> Child<'a> {
        Child {
            labels,
            is_pr: false,
            inherited,
        }
    }

    fn add(l: &str) -> Action {
        Action::Add(l.into())
    }

    fn remove(l: &str) -> Action {
        Action::Remove(l.into())
    }

    // ---- the audit: every registry label is classified -------------------

    #[test]
    fn every_registry_label_is_a_rule_or_never_propagates() {
        let reg = Registry::embedded();
        let mut unclassified = Vec::new();
        for l in &reg.labels {
            let ruled = rule_for(&l.name).is_some();
            let never =
                NEVER_KINDS.contains(&l.kind.as_str()) || NEVER_LABELS.contains(&l.name.as_str());
            if ruled == never {
                unclassified
                    .push(format!("{} (kind {}, rule {ruled}, never {never})", l.name, l.kind));
            }
        }
        assert!(
            unclassified.is_empty(),
            "classify each label in star_liveness::propagation_rules (exactly one of a rule or the never list): {unclassified:?}"
        );
    }

    #[test]
    fn the_never_lists_name_real_registry_entries() {
        let reg = Registry::embedded();
        for k in NEVER_KINDS {
            assert!(!reg.with_kind(k).is_empty(), "NEVER_KINDS has stale kind {k}");
        }
        for l in NEVER_LABELS {
            assert!(reg.get(l).is_some(), "NEVER_LABELS has stale label {l}");
        }
        for r in RULES {
            let hit = reg.labels.iter().any(|l| r.label.covers(&l.name));
            assert!(hit, "rule {:?} covers no registry label", r.label);
        }
    }

    #[test]
    fn holds_claims_lifecycle_heavy_and_points_never_propagate() {
        for l in [
            "loom:blocked",
            "loom:operator",
            "loom:operator-only",
            "loom:operator-decision",
            "loom:needs-capability",
            "loom:building",
            "loom:issue",
            "loom:curated",
            "loom:pr",
            "loom:review-requested",
            "loom:architect",
            "loom:epic",
            "loom:epic-phase",
            "loom:heavy",
            "points:3",
            "loom:urgent",
            "loom:operator-high-priority",
            "loom:high-priority-inherited",
            // Not a Loom label at all.
            "bug",
        ] {
            assert!(rule_for(l).is_none(), "{l} must never propagate");
        }
        let parent = v(&[
            "loom:blocked",
            "loom:operator",
            "loom:heavy",
            "points:8",
            "loom:building",
        ]);
        assert!(plan(&[parent], issue(&[], &[]), true).is_empty());
    }

    // ---- external --------------------------------------------------------

    #[test]
    fn external_goes_down_to_issues_but_not_prs() {
        let parent = v(&[EXTERNAL_LABEL]);
        assert_eq!(
            plan(std::slice::from_ref(&parent), issue(&[], &[]), true),
            vec![add(EXTERNAL_LABEL)]
        );
        let pr = Child {
            labels: &[],
            is_pr: true,
            inherited: &[],
        };
        assert!(plan(&[parent], pr, true).is_empty());
    }

    #[test]
    fn external_is_removed_with_the_parent_label_only_when_it_was_inherited() {
        let mine = v(&[EXTERNAL_LABEL]);
        // The parent was approved: the inherited copy comes off.
        assert_eq!(plan(&[v(&[])], issue(&mine, &mine), true), vec![remove(EXTERNAL_LABEL)]);
        // A maintainer put it on the child directly: it stays.
        assert!(plan(&[v(&[])], issue(&mine, &[]), true).is_empty());
        // Still on the parent: it stays.
        assert!(plan(&[v(&[EXTERNAL_LABEL])], issue(&mine, &mine), true).is_empty());
    }

    #[test]
    fn external_stays_while_any_ancestor_carries_it() {
        let mine = v(&[EXTERNAL_LABEL]);
        let ancestors = [v(&[]), v(&[EXTERNAL_LABEL])];
        assert!(plan(&ancestors, issue(&mine, &mine), true).is_empty());
    }

    #[test]
    fn an_incomplete_walk_adds_but_never_removes() {
        let mine = v(&[EXTERNAL_LABEL]);
        assert!(plan(&[v(&[])], issue(&mine, &mine), false).is_empty());
        assert_eq!(
            plan(&[v(&[EXTERNAL_LABEL])], issue(&[], &[]), false),
            vec![add(EXTERNAL_LABEL)]
        );
    }

    // ---- tier:* ----------------------------------------------------------

    #[test]
    fn tier_is_a_default_for_an_untiered_child() {
        let parent = v(&["tier:goal-advancing"]);
        assert_eq!(plan(&[parent], issue(&[], &[]), true), vec![add("tier:goal-advancing")]);
    }

    #[test]
    fn tier_never_overwrites_the_childs_own() {
        let parent = v(&["tier:goal-advancing"]);
        let child = v(&["tier:maintenance"]);
        assert!(plan(&[parent], issue(&child, &[]), true).is_empty());
    }

    #[test]
    fn tier_is_never_removed_by_propagation() {
        // The parent dropped its tier; the child's (even an inherited one) stays.
        let child = v(&["tier:goal-supporting"]);
        assert!(plan(&[v(&[])], issue(&child, &child), true).is_empty());
    }

    #[test]
    fn tier_comes_from_the_nearest_tiered_ancestor_and_ambiguity_decides_nothing() {
        let near = v(&["tier:goal-supporting"]);
        let far = v(&["tier:goal-advancing"]);
        assert_eq!(
            plan(&[v(&[]), near, far.clone()], issue(&[], &[]), true),
            vec![add("tier:goal-supporting")]
        );
        let both = v(&["tier:goal-supporting", "tier:maintenance"]);
        assert!(plan(&[both, far], issue(&[], &[]), true).is_empty());
    }

    #[test]
    fn tier_does_not_reach_prs() {
        let pr = Child {
            labels: &[],
            is_pr: true,
            inherited: &[],
        };
        assert!(plan(&[v(&["tier:maintenance"])], pr, true).is_empty());
    }

    // ---- the star, and ordering ------------------------------------------

    #[test]
    fn the_star_reaches_issues_and_prs_and_leaves_an_operator_star_alone() {
        let parent = v(&[STAR]);
        assert_eq!(plan(std::slice::from_ref(&parent), issue(&[], &[]), true), vec![add(STAR)]);
        let pr = Child {
            labels: &[],
            is_pr: true,
            inherited: &[],
        };
        assert_eq!(plan(&[parent], pr, true), vec![add(STAR)]);
        let own = v(&[STAR]);
        assert!(plan(&[v(&[])], issue(&own, &[]), true).is_empty());
        assert_eq!(plan(&[v(&[])], issue(&own, &own), true), vec![remove(STAR)]);
    }

    #[test]
    fn one_parent_carrying_every_rule_label_yields_adds_in_table_order() {
        let parent = v(&["tier:maintenance", EXTERNAL_LABEL, STAR, "loom:blocked"]);
        assert_eq!(
            plan(&[parent], issue(&[], &[]), true),
            vec![add(STAR), add(EXTERNAL_LABEL), add("tier:maintenance")]
        );
    }
}
