//! The label registry (#10013): `defaults/labels.json`, the single source of
//! truth for Loom label semantics, embedded at build time so label meaning
//! ships with the binary and cannot drift per host.
//!
//! Slice 1 scope: the registry itself, its query API, and the generator for
//! the Loom marker block of the two full `labels.yml` copies.
//!
//! Slice 2a: the work-finder and hard-exclusion label sets (`PARK_LABELS`,
//! `SKIP_LABELS`, `HARD_EXCLUSION_LABELS`, `CHAMPION_PATH_LABELS`) are
//! [`LabelSet`]s derived from registry properties via [`embedded_set`].
//!
//! Slice 2b: `OPERATOR_GATE_LABELS`, pr_latency `hold_labels`, merge_pr
//! `BLOCKING` and queue_blocked `HOLD_LABELS` are derived too; `tests.rs` pins each to its previous literal. dep_classify's
//! operator-only labels are single named consts, kept in lockstep by a test.
//!
//! Final slice (AC 6): `propagate` holds #10012's propagation rule table;
//! `star_liveness::propagation_rules::RULES` is derived from it.
//!
//! Fields documented as inert (`stale_after_minutes`, `lifecycle`) have no
//! consumer yet.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub mod generate;

#[cfg(test)]
mod tests;

/// The embedded registry source.
pub const REGISTRY_JSON: &str = include_str!("../../../defaults/labels.json");

/// A label set derived from the embedded registry, computed on first use and
/// shared for the life of the process. Derefs to `[&'static str]`, so
/// `.contains()` / `.iter()` read like the `&[&str]` constants it replaces.
pub type LabelSet = std::sync::LazyLock<Vec<&'static str>>;

/// Names with boolean `property` true in the embedded registry, in registry
/// order. The initializer for a [`LabelSet`].
///
/// # Panics
/// If `property` is not in [`BOOL_PROPERTIES`] (a programming error caught by
/// the registry tests, which force every derived set).
#[must_use]
pub fn embedded_set(property: &str) -> Vec<&'static str> {
    Registry::embedded()
        .with_property(property)
        .unwrap_or_else(|| panic!("{property} is not a registry boolean property"))
}

/// Boolean properties queryable through [`Registry::with_property`].
pub const BOOL_PROPERTIES: &[&str] = &[
    "park",
    "skip",
    "hold",
    "operator_gate",
    "blocked_colabel",
    "hard_exclusion",
    "champion_path",
    "human_gated",
    "merge_hold",
    "operator_hold",
    "contradicts_approval",
];

/// Allowed values of [`Label::kind`].
pub const KINDS: &[&str] = &[
    "workflow",
    "claim",
    "pr-lane",
    "proposal",
    "hold",
    "priority",
    "structural",
    "tier",
    "size",
    "external",
];

/// Allowed values of [`Label::lifecycle`].
pub const LIFECYCLES: &[&str] = &["active", "paused", "retired"];

/// Which way a label travels along a containment edge. Only downward today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    ParentToChild,
}

/// When a child gets a propagating label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AddMode {
    /// Whenever an ancestor carries it and the child does not.
    Always,
    /// Only as a default: the child carries no member of the family yet.
    /// Never overwrites a child's own choice.
    DefaultIfFamilyAbsent,
}

/// When propagation takes a label back off a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Removal {
    /// Once no ancestor carries it any more, and only a copy whose provenance
    /// is propagation (never one a human applied to the child).
    WithParent,
    /// Never: once copied, the child owns it.
    Never,
}

/// A label's propagation rule (#10012 §6); `None` on [`Label::propagate`]
/// means it never propagates.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Propagate {
    pub direction: Direction,
    /// 1-based rule order (adds, then removals, are planned in this order).
    /// Members of one `family` share one rule and so one rank.
    pub rank: u32,
    /// A mutually exclusive family prefix (e.g. `tier:`): the rule covers
    /// every label with that prefix, and all members carry the same object.
    pub family: Option<String>,
    pub add: AddMode,
    pub removal: Removal,
    /// A PR linked to the child (or the parent) gets it too (§5).
    pub to_prs: bool,
}

/// One label's full definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub name: String,
    pub description: String,
    pub color: String,
    pub kind: String,
    pub applied_by: Option<String>,
    pub removed_by: Option<String>,
    pub park: bool,
    pub skip: bool,
    pub hold: bool,
    pub operator_gate: bool,
    pub blocked_colabel: bool,
    pub hard_exclusion: bool,
    pub champion_path: bool,
    /// A human must act (#11087): the notifier contract. An external
    /// notifier reads `labels list --property human_gated` instead of
    /// hard-coding names; see `label-state-machine.md`.
    pub human_gated: bool,
    /// Operator holds that make an approved PR's wait a merge hold (#10218).
    pub merge_hold: bool,
    /// Operator holds: a human is needed before the item moves (#10210).
    pub operator_hold: bool,
    /// 1-based position in the order labels that contradict `loom:pr` are
    /// reported; `None` = does not contradict.
    pub contradicts_approval: Option<u32>,
    /// Inert (no consumer yet).
    pub stale_after_minutes: Option<u32>,
    pub requires_base: Option<String>,
    pub remove_with: Vec<String>,
    /// Inert: `active`, `paused` or `retired`.
    pub lifecycle: String,
    /// #10012's propagation rule; `None` = never propagates.
    pub propagate: Option<Propagate>,
    /// Trailing `# note` on the generated `color:` line.
    pub color_note: Option<String>,
    /// Comment/blank lines emitted verbatim above the generated entry.
    pub yaml_preamble: Vec<String>,
}

/// The parsed registry.
#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    #[serde(rename = "$comment", default)]
    pub comment: Vec<String>,
    pub yaml_header: Vec<String>,
    pub labels: Vec<Label>,
}

impl Label {
    /// Value of a boolean property; `None` when `property` is not one.
    #[must_use]
    pub fn property(&self, property: &str) -> Option<bool> {
        Some(match property {
            "park" => self.park,
            "skip" => self.skip,
            "hold" => self.hold,
            "operator_gate" => self.operator_gate,
            "blocked_colabel" => self.blocked_colabel,
            "hard_exclusion" => self.hard_exclusion,
            "champion_path" => self.champion_path,
            "human_gated" => self.human_gated,
            "merge_hold" => self.merge_hold,
            "operator_hold" => self.operator_hold,
            "contradicts_approval" => self.contradicts_approval.is_some(),
            _ => return None,
        })
    }
}

impl Registry {
    /// Parse registry JSON and validate its invariants.
    pub fn parse(json: &str) -> Result<Self> {
        let reg: Registry = serde_json::from_str(json).context("parsing labels.json")?;
        reg.validate()?;
        Ok(reg)
    }

    /// The registry embedded in this binary.
    ///
    /// # Panics
    /// If the embedded file is invalid (a build-time bug caught by tests).
    #[must_use]
    pub fn embedded() -> &'static Registry {
        static REG: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
        REG.get_or_init(|| {
            Registry::parse(REGISTRY_JSON).expect("embedded defaults/labels.json is invalid")
        })
    }

    /// Schema invariants: unique names, valid colors/kinds/lifecycles,
    /// description within GitHub's 100-char limit, consistent cross-references.
    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for l in &self.labels {
            if !seen.insert(l.name.as_str()) {
                bail!("duplicate label {}", l.name);
            }
            if !(l.name.starts_with("loom:") || l.name == "external" || l.name.contains(':')) {
                bail!("{}: unexpected name shape", l.name);
            }
            if l.color.len() != 6 || !l.color.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("{}: color must be 6 hex digits", l.name);
            }
            if l.description.chars().count() > 100 {
                bail!("{}: description exceeds GitHub's 100-char limit", l.name);
            }
            if l.description.contains('"') || l.description.contains('\\') {
                bail!("{}: description may not contain quote or backslash", l.name);
            }
            if !KINDS.contains(&l.kind.as_str()) {
                bail!("{}: unknown kind {}", l.name, l.kind);
            }
            if !LIFECYCLES.contains(&l.lifecycle.as_str()) {
                bail!("{}: unknown lifecycle {}", l.name, l.lifecycle);
            }
            if l.park && !l.skip {
                bail!("{}: a park label must also be a skip label (#4444)", l.name);
            }
        }
        for l in &self.labels {
            for r in l.remove_with.iter().chain(l.requires_base.iter()) {
                if !seen.contains(r.as_str()) {
                    bail!("{}: references unknown label {r}", l.name);
                }
            }
            for r in &l.remove_with {
                let sub = self.get(r).context("checked above")?;
                if sub.requires_base.as_deref() != Some(l.name.as_str()) {
                    bail!("{}: remove_with {r} but {r}.requires_base disagrees", l.name);
                }
            }
        }
        let mut ranks: Vec<u32> = self
            .labels
            .iter()
            .filter_map(|l| l.contradicts_approval)
            .collect();
        ranks.sort_unstable();
        if ranks.iter().copied().ne(1..=ranks.len() as u32) {
            bail!("contradicts_approval ranks must be 1..=N without gaps");
        }
        self.validate_propagate()
    }

    /// `propagate` invariants: a family rule is shared verbatim by its
    /// members and is the only way to say "default if family absent"; each
    /// rule (a family counts once) has a distinct rank, 1..=N without gaps.
    fn validate_propagate(&self) -> Result<()> {
        let mut rules: Vec<&Propagate> = Vec::new();
        for l in &self.labels {
            let Some(p) = &l.propagate else { continue };
            match &p.family {
                Some(f) if !l.name.starts_with(f.as_str()) => {
                    bail!("{}: propagate.family {f} is not a prefix of the name", l.name)
                }
                None if p.add == AddMode::DefaultIfFamilyAbsent => {
                    bail!("{}: default-if-family-absent needs a propagate.family", l.name)
                }
                _ => {}
            }
            match rules
                .iter()
                .find(|r| p.family.is_some() && r.family == p.family)
            {
                Some(r) if *r != p => {
                    bail!("{}: family members must share one propagate object", l.name)
                }
                Some(_) => {}
                None => rules.push(p),
            }
        }
        let mut ranks: Vec<u32> = rules.iter().map(|r| r.rank).collect();
        ranks.sort_unstable();
        if ranks.iter().copied().ne(1..=ranks.len() as u32) {
            bail!("propagate ranks must be 1..=N without gaps (a family counts once)");
        }
        Ok(())
    }

    /// Look a label up by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Label> {
        self.labels.iter().find(|l| l.name == name)
    }

    /// Names with `property` true, in registry order; `contradicts_approval`
    /// is returned in its report order. `None` for an unknown property.
    #[must_use]
    pub fn with_property(&self, property: &str) -> Option<Vec<&str>> {
        if !BOOL_PROPERTIES.contains(&property) {
            return None;
        }
        let mut v: Vec<&Label> = self
            .labels
            .iter()
            .filter(|l| l.property(property) == Some(true))
            .collect();
        if property == "contradicts_approval" {
            v.sort_by_key(|l| l.contradicts_approval);
        }
        Some(v.into_iter().map(|l| l.name.as_str()).collect())
    }

    /// Names of the given `kind`, in registry order.
    #[must_use]
    pub fn with_kind(&self, kind: &str) -> Vec<&str> {
        self.labels
            .iter()
            .filter(|l| l.kind == kind)
            .map(|l| l.name.as_str())
            .collect()
    }
}
