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
//! `BLOCKING` and queue_blocked `HOLD_LABELS` are derived too; `tests.rs`
//! pins each to its literal.
//!
//! The `propagate` rule table (#10012) is gone (#11510): its only consumer,
//! the star-liveness propagation pass, was removed by #11104.
//!
//! #11795 folded the hold family into three labels (`loom:blocked`,
//! `loom:operator`, `loom:frozen`). Hold-shaped sets use
//! [`embedded_set_with_retired`] so the retired names keep their
//! replacement's meaning until open items are migrated.
//!
//! Retired labels (#11513) are not in `labels`: they are listed in the
//! top-level `retired` array ([`RetiredLabel`]) with their old -> new
//! mapping, which the forge-side migration (#11511) consumes. That array is
//! never rendered into `labels.yml`, so the additive `sync-labels.sh` cannot
//! re-create a retired label.
//!
//! Fields documented as inert (`stale_after_minutes`, `lifecycle`) have no
//! consumer yet.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

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

/// [`embedded_set`] plus every retired label whose `replaced_by` carries
/// `property` (#11795), in registry order then retired order.
///
/// A retired label is read as its replacement while open items still carry
/// it: the daemon rolls before `labels migrate` relabels them, so a set that
/// knew only the active names would drop a held item out of its hold for the
/// length of that window. Use it for the hold-shaped sets (park, skip, hold,
/// the operator gates); never for `contradicts_approval`, whose order is a
/// report contract.
///
/// # Panics
/// As [`embedded_set`].
#[must_use]
pub fn embedded_set_with_retired(property: &str) -> Vec<&'static str> {
    let reg = Registry::embedded();
    let mut set = embedded_set(property);
    set.extend(
        reg.retired
            .iter()
            .filter(|r| {
                r.replaced_by
                    .as_deref()
                    .and_then(|to| reg.get(to))
                    .and_then(|l| l.property(property))
                    == Some(true)
            })
            .map(|r| r.name.as_str()),
    );
    set
}

/// `active` followed by every retired label whose `replaced_by` is `active`
/// (#11795): the names an unmigrated item may still carry for it.
///
/// # Panics
/// If `active` is not a declared label (a programming error).
#[must_use]
pub fn embedded_aliases(active: &'static str) -> Vec<&'static str> {
    let reg = Registry::embedded();
    assert!(reg.get(active).is_some(), "{active} is not a registry label");
    let mut v = vec![active];
    v.extend(
        reg.retired
            .iter()
            .filter(|r| r.replaced_by.as_deref() == Some(active))
            .map(|r| r.name.as_str()),
    );
    v
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
    "size",
    "external",
];

/// Allowed values of [`Label::lifecycle`]. A retired label leaves `labels`
/// for [`Registry::retired`] instead (#11513).
pub const LIFECYCLES: &[&str] = &["active", "paused"];

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
    /// Inert: `active` or `paused`.
    pub lifecycle: String,
    /// Trailing `# note` on the generated `color:` line.
    pub color_note: Option<String>,
    /// Comment/blank lines emitted verbatim above the generated entry.
    pub yaml_preamble: Vec<String>,
}

/// A label Loom no longer declares, and what replaces it on the forge
/// (#11513). Read by the forge-side migration (#11511).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredLabel {
    pub name: String,
    /// The active label an item carrying `name` gets instead; `None` means
    /// remove with no replacement.
    pub replaced_by: Option<String>,
    /// `true`: Loom declared this label, so the migration may strip it from
    /// open items and delete it. `false`: Loom never declared it (a
    /// consumer's label), so the migration only adds `replaced_by` and never
    /// removes or deletes it.
    pub owned: bool,
    /// The issue that retired it, e.g. `#11103`.
    pub retired_in: String,
}

/// The parsed registry.
#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    #[serde(rename = "$comment", default)]
    pub comment: Vec<String>,
    pub yaml_header: Vec<String>,
    pub labels: Vec<Label>,
    /// Retired labels and their mapping; never rendered into `labels.yml`.
    #[serde(default)]
    pub retired: Vec<RetiredLabel>,
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
        self.validate_retired()
    }

    /// `retired` invariants (#11513): a retired name is unique and not also
    /// declared, and each `replaced_by` names an active declared label.
    fn validate_retired(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for r in &self.retired {
            if !seen.insert(r.name.as_str()) {
                bail!("duplicate retired label {}", r.name);
            }
            if self.get(&r.name).is_some() {
                bail!("{}: retired but still declared in labels", r.name);
            }
            if r.retired_in.is_empty() {
                bail!("{}: retired_in must name the retiring issue", r.name);
            }
            if let Some(to) = &r.replaced_by {
                match self.get(to) {
                    Some(l) if l.lifecycle == "active" => {}
                    Some(_) => bail!("{}: replaced_by {to} is not an active label", r.name),
                    None => bail!("{}: replaced_by unknown label {to}", r.name),
                }
            }
        }
        Ok(())
    }

    /// Look a retired label up by name.
    #[must_use]
    pub fn get_retired(&self, name: &str) -> Option<&RetiredLabel> {
        self.retired.iter().find(|r| r.name == name)
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
