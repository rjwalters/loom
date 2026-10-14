//! The role a step runs, and the claim it holds (Issue #11898; design #11894,
//! `docs/design/work-finder-step-dispatch.md` §4.4 and §5).
//!
//! A **step** is one role session that advances one item by one stage. The
//! sweep registry tracks it as [`crate::types::SweepKind::Step`], whose `role`
//! is a [`StepRole`]. This module is the one place that maps a step role to
//! its claim label and to the stage a dead step's claim is restored to, so the
//! reaper, roll pause/resume and the claim records never re-derive the table.
//!
//! # Why Champion is two roles here
//!
//! Champion runs two different steps with two different claims: a promotion
//! (`loom:evaluating` on an issue, restored to `loom:curated`) and a merge (a
//! PR lease with `step=champion_merge` and no claim label). The registry kind
//! must say which one a live session holds, or a dead merge step would be
//! "restored" to `loom:curated`. [`StepRole::role_name`] folds both back to
//! `champion` for everything that wants the role (the slash command, the
//! role runner's resolvers, telemetry's `role` attribute).
//!
//! # Relation to the item classifier
//!
//! The classifier (#11895) has its own, finer `Step` verdict (Curator
//! keep-or-close, Champion re-evaluate, Champion hold re-evaluation, …). Each
//! of those runs as one of the six roles here: keep-or-close as
//! [`StepRole::Curator`], re-evaluate as [`StepRole::ChampionPromote`], hold
//! re-evaluation as [`StepRole::ChampionMerge`]. `From<Step> for StepRole`
//! is that fold (#11900); the two enums stay separate.
//!
//! # Wire form
//!
//! `snake_case`: `curator`, `champion_promote`, `builder`, `judge`, `doctor`,
//! `champion_merge`. [`StepRole::as_str`] is the same string, and is the
//! `step` value of a lease record and a peer claim.

use serde::{Deserialize, Serialize};

/// The role a step runs, split where the claim differs (Champion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepRole {
    /// Curator on an issue: claim `loom:curating`.
    Curator,
    /// Champion promoting (or re-evaluating) an issue: claim `loom:evaluating`.
    ChampionPromote,
    /// Builder on an issue: claim `loom:building` plus the issue lease.
    Builder,
    /// Judge on a PR: claim `loom:reviewing` plus the PR lease.
    Judge,
    /// Doctor on a PR: claim `loom:treating` plus the PR lease.
    Doctor,
    /// Champion merging (or re-evaluating a hold on) a PR: the PR lease with
    /// `step=champion_merge`, no claim label.
    ChampionMerge,
}

impl StepRole {
    /// Every step role, in lifecycle order.
    pub const ALL: [StepRole; 6] = [
        StepRole::Curator,
        StepRole::ChampionPromote,
        StepRole::Builder,
        StepRole::Judge,
        StepRole::Doctor,
        StepRole::ChampionMerge,
    ];

    /// The wire name (the serde form), also a lease record's `step` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            StepRole::Curator => "curator",
            StepRole::ChampionPromote => "champion_promote",
            StepRole::Builder => "builder",
            StepRole::Judge => "judge",
            StepRole::Doctor => "doctor",
            StepRole::ChampionMerge => "champion_merge",
        }
    }

    /// The role that runs the step: the `/loom:<role>` command and the role
    /// runner's role name. Both Champion steps are `champion`.
    #[must_use]
    pub const fn role_name(self) -> &'static str {
        match self {
            StepRole::Curator => "curator",
            StepRole::ChampionPromote | StepRole::ChampionMerge => "champion",
            StepRole::Builder => "builder",
            StepRole::Judge => "judge",
            StepRole::Doctor => "doctor",
        }
    }

    /// The step's item number names a PR (it is invoked on the PR), not an
    /// issue.
    #[must_use]
    pub const fn acts_on_pr(self) -> bool {
        matches!(self, StepRole::Judge | StepRole::Doctor | StepRole::ChampionMerge)
    }

    /// The label the step claims its item with, `None` for the merge step
    /// (its claim is the PR lease alone).
    #[must_use]
    pub const fn claim_label(self) -> Option<&'static str> {
        match self {
            StepRole::Curator => Some("loom:curating"),
            StepRole::ChampionPromote => Some("loom:evaluating"),
            StepRole::Builder => Some("loom:building"),
            StepRole::Judge => Some("loom:reviewing"),
            StepRole::Doctor => Some("loom:treating"),
            StepRole::ChampionMerge => None,
        }
    }

    /// The stage a dead step's claim is restored to (design §5): the label
    /// that makes the item ready for this same role again.
    #[must_use]
    pub const fn restore_label(self) -> Option<&'static str> {
        match self {
            StepRole::Curator => Some("loom:triage"),
            StepRole::ChampionPromote => Some("loom:curated"),
            StepRole::Builder => Some("loom:issue"),
            StepRole::Judge => Some("loom:review-requested"),
            StepRole::Doctor => Some("loom:changes-requested"),
            StepRole::ChampionMerge => None,
        }
    }

    /// The step role whose claim label is `label`, if any.
    #[must_use]
    pub fn from_claim_label(label: &str) -> Option<StepRole> {
        Self::ALL
            .into_iter()
            .find(|r| r.claim_label() == Some(label))
    }

    /// The step and item a step's sweep id names (`step-<role>-<item>-<ts>`,
    /// optionally `-r<n>` for a roll resume; `generate_sweep_id`). The kind
    /// is recoverable from the id alone, so a lock's owner says whether its
    /// holder is a step (#11898: startup adoption). Pure.
    #[must_use]
    pub fn parse_sweep_id(id: &str) -> Option<(StepRole, u32)> {
        let mut parts = id.strip_prefix("step-")?.split('-');
        let role = parts.next()?.parse().ok()?;
        let item = parts.next()?.parse().ok()?;
        Some((role, item))
    }

    /// The `/loom:<role> <item>` prompt a fresh step session starts with.
    #[must_use]
    pub fn prompt(self, item: u32) -> String {
        format!("/loom:{} {item}", self.role_name())
    }
}

/// The classifier's finer step folded onto the role that runs it (the module
/// doc's "Relation to the item classifier"): the outcome ledger (#11900) keys
/// on `(item, StepRole)`, the same pair a step claim and its peer ad carry.
impl From<crate::work_finder::step_item::Step> for StepRole {
    fn from(step: crate::work_finder::step_item::Step) -> Self {
        use crate::work_finder::step_item::Step;
        match step {
            Step::Curator | Step::CuratorKeepOrClose => StepRole::Curator,
            Step::ChampionPromote | Step::ChampionReevaluate => StepRole::ChampionPromote,
            Step::Builder => StepRole::Builder,
            Step::Judge => StepRole::Judge,
            Step::Doctor => StepRole::Doctor,
            Step::ChampionMerge | Step::ChampionHoldReevaluate => StepRole::ChampionMerge,
        }
    }
}

impl std::fmt::Display for StepRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for StepRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| format!("unknown step role `{s}`"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn wire_name_round_trips_through_serde_and_from_str() {
        for role in StepRole::ALL {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            assert_eq!(serde_json::from_str::<StepRole>(&json).unwrap(), role);
            assert_eq!(role.as_str().parse::<StepRole>().unwrap(), role);
        }
        assert!("sweep".parse::<StepRole>().is_err());
    }

    /// Every classifier step folds onto the role whose claim it takes.
    #[test]
    fn classifier_steps_fold_onto_their_role() {
        use crate::work_finder::step_item::Step;
        let table = [
            (Step::Curator, StepRole::Curator),
            (Step::CuratorKeepOrClose, StepRole::Curator),
            (Step::ChampionPromote, StepRole::ChampionPromote),
            (Step::ChampionReevaluate, StepRole::ChampionPromote),
            (Step::Builder, StepRole::Builder),
            (Step::Judge, StepRole::Judge),
            (Step::Doctor, StepRole::Doctor),
            (Step::ChampionMerge, StepRole::ChampionMerge),
            (Step::ChampionHoldReevaluate, StepRole::ChampionMerge),
        ];
        for (step, role) in table {
            assert_eq!(StepRole::from(step), role, "{step:?}");
            assert_eq!(step.role(), role.role_name());
            assert_eq!(step.acts_on_pr(), role.acts_on_pr(), "{step:?}");
        }
    }

    /// Design §5's restore table, row by row.
    #[test]
    fn restore_table_matches_the_design() {
        let table = [
            ("loom:curating", "loom:triage"),
            ("loom:evaluating", "loom:curated"),
            ("loom:building", "loom:issue"),
            ("loom:reviewing", "loom:review-requested"),
            ("loom:treating", "loom:changes-requested"),
        ];
        for (claim, ready) in table {
            let role = StepRole::from_claim_label(claim).unwrap();
            assert_eq!(role.restore_label(), Some(ready), "{claim}");
        }
        assert_eq!(StepRole::ChampionMerge.claim_label(), None);
        assert_eq!(StepRole::ChampionMerge.restore_label(), None);
    }

    /// `SweepKind::Step` is additive on the wire: the old kinds keep their
    /// shapes, and a step has its own (#11898).
    #[test]
    fn sweep_kind_step_wire_shape_is_additive() {
        use crate::types::SweepKind;
        let step = SweepKind::Step {
            item: 42,
            role: StepRole::ChampionMerge,
        };
        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "Step", "value": {"item": 42, "role": "champion_merge"}})
        );
        assert_eq!(serde_json::from_value::<SweepKind>(json).unwrap(), step);
        let issue: SweepKind = serde_json::from_str(r#"{"type":"Issue","value":7}"#).unwrap();
        assert_eq!(issue, SweepKind::Issue(7));
        let prs: SweepKind = serde_json::from_str(r#"{"type":"PrSet","value":[1,2]}"#).unwrap();
        assert_eq!(prs, SweepKind::PrSet(vec![1, 2]));

        assert_eq!(step.item_number(), Some(42));
        assert_eq!(step.role(), Some(StepRole::ChampionMerge));
        assert_eq!(step.issue_number(), None, "a PR step names no issue");
        let builder = SweepKind::Step {
            item: 9,
            role: StepRole::Builder,
        };
        assert_eq!(builder.issue_number(), Some(9));
        assert_eq!(issue.item_number(), Some(7));
        assert_eq!(issue.role(), None);
        assert_eq!(prs.item_number(), None);
    }

    #[test]
    fn champion_steps_share_the_role_and_split_the_target() {
        assert_eq!(StepRole::ChampionPromote.role_name(), "champion");
        assert_eq!(StepRole::ChampionMerge.role_name(), "champion");
        assert!(!StepRole::ChampionPromote.acts_on_pr());
        assert!(StepRole::ChampionMerge.acts_on_pr());
        assert_eq!(StepRole::Judge.prompt(42), "/loom:judge 42");
    }
}

#[cfg(test)]
#[path = "step_kind_sites_tests.rs"]
mod step_kind_sites_tests;
