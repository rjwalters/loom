//! Operator priority **levels** (#10307): the one table that maps a level to
//! its labels and cap.
//!
//! The star (`loom:operator-priority`, #9244) filled up: once a quarter of
//! the backlog carries it, the order inside it is just oldest-first, and the
//! operator has no way to say "this one, now". Levels add capped signals
//! above the star without new machinery per level:
//!
//! | Level | UI | Operator label | Inherited label (daemon-written) | Cap |
//! |---|---|---|---|---|
//! | 1 | ⭐ | `loom:operator-priority` | none | none |
//! | 2 | ⭐⭐ | `loom:operator-high-priority` | `loom:high-priority-inherited` | 5 fleet-wide |
//! | 3 (later) | ⭐⭐⭐ | `loom:operator-top-priority` | `loom:top-priority-inherited` | smaller |
//!
//! **Adding level 3.** Every Rust consumer takes the table as data, so the
//! daemon half is a new row in [`LEVELS`] plus its two `defaults/labels.json`
//! entries (then `loom-daemon labels generate --write`). The role prompts
//! do **not** read the table yet: each hard-codes the level labels and must
//! be edited by hand for a new level until #10311 makes them table-driven.
//! Each prompt site below carries a "level list: keep in sync with
//! operator_levels.rs LEVELS until #10311" note:
//!
//! - (No longer a site: a PR's label copy is `create-pr.sh` asking
//!   `forge priority-labels`, which reads this table — #10518.)
//! - `defaults/.claude/commands/loom/builder.md`, Priority Order and the
//!   starred-first `for L in …` query.
//! - `defaults/.claude/commands/loom/curator.md`, Priority 0 `for L in …`.
//! - `defaults/.claude/commands/loom/champion-pr-merge.md`, the held-PR
//!   digest's `LVL` jq mapping (level 0/1/2) and its ⭐ / ⭐⭐ glyph `case`.
//! - `defaults/.claude/commands/loom/champion.md`, Priority 4 epic queue
//!   `sort_by` (it buckets every `high-priority` label as one level).
//! - Prose (no sync note) that names the level-2 labels:
//!   `defaults/docs/label-state-machine.md` (levels bullet) and
//!   `defaults/docs/pr-planning.md` (Operator star row).
//!
//! Three more surfaces name only the star (no level-2 label either):
//! the `defaults/roles/*.json` interval prompts, the
//! `defaults/.claude/agents/loom-*.md` one-line descriptions, and Guide's
//! WORK_PLAN "Operator Priority" query (`guide.md`). They defer to the role
//! prompts above and are in #10311's scope. Regenerate the
//! `defaults/.agents/skills/` copies with `loom-daemon generate-agent-skills`;
//! never edit them by hand.
//!
//! # Rules every consumer follows
//!
//! - **Effective level** = the highest level among an item's operator labels
//!   and inherited labels ([`level_in`]). Ordering drains by effective level,
//!   highest first, then by the keys that ordered starred work before.
//! - **Levels nest.** Level ≥ 1 counts as starred everywhere
//!   ([`is_starred`]), so a path that only knew the star keeps every
//!   higher-level item. A level-2 intent never adds or removes the star.
//! - **Operator labels are human-only**, like the star: no role applies or
//!   removes one on its own judgment; the loom-ui intent relay applies them.
//!   **Inherited labels are daemon-only** (the star-liveness pass writes and
//!   removes them) and are never accepted from an intent.
//! - **No level label is a hold.** They share the `loom:operator-` prefix
//!   with the operator holds; every prefix match must exclude them by name
//!   ([`is_level_label`]).

/// One operator priority level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityLevel {
    /// 1 = the star; higher sorts first.
    pub level: u8,
    /// How loom-ui and the digest show it.
    pub glyph: &'static str,
    /// What the audit comment calls it (`operator high priority`).
    pub name: &'static str,
    /// The label a human applies (directly, or through a loom-ui intent).
    pub operator_label: &'static str,
    /// The label the star-liveness pass writes on every open blocker of an
    /// item at this level, or `None` when the level does not propagate
    /// through blockers this way (the star keeps its own in-memory path).
    pub inherited_label: Option<&'static str>,
    /// Most open issues fleet-wide that may carry [`Self::operator_label`]
    /// by default, or `None` for no cap. The cap is enforced by loom-ui at
    /// click time; the daemon only reports an over-cap level.
    pub default_cap: Option<usize>,
}

/// The level table. Ascending by level; one row per level.
pub const LEVELS: &[PriorityLevel] = &[
    PriorityLevel {
        level: 1,
        glyph: "⭐",
        name: "operator priority",
        operator_label: "loom:operator-priority",
        inherited_label: None,
        default_cap: None,
    },
    PriorityLevel {
        level: 2,
        glyph: "⭐⭐",
        name: "operator high priority",
        operator_label: "loom:operator-high-priority",
        inherited_label: Some("loom:high-priority-inherited"),
        default_cap: Some(5),
    },
];

/// The level-2 operator label.
pub const OPERATOR_HIGH_PRIORITY_LABEL: &str = "loom:operator-high-priority";
/// The level-2 inherited label.
pub const HIGH_PRIORITY_INHERITED_LABEL: &str = "loom:high-priority-inherited";

/// The production table.
#[must_use]
pub fn table() -> &'static [PriorityLevel] {
    LEVELS
}

/// The row for `level` in `table`.
#[must_use]
pub fn row(table: &[PriorityLevel], level: u8) -> Option<&PriorityLevel> {
    table.iter().find(|r| r.level == level)
}

/// The row whose **operator** label is `label`. An inherited label is not
/// an operator label and answers `None`.
#[must_use]
pub fn by_operator_label<'t>(table: &'t [PriorityLevel], label: &str) -> Option<&'t PriorityLevel> {
    table.iter().find(|r| r.operator_label == label)
}

/// The row whose **inherited** label is `label`.
#[must_use]
pub fn by_inherited_label<'t>(
    table: &'t [PriorityLevel],
    label: &str,
) -> Option<&'t PriorityLevel> {
    table.iter().find(|r| r.inherited_label == Some(label))
}

fn max_level<S: AsRef<str>>(labels: &[S], pick: impl Fn(&str) -> Option<u8>) -> u8 {
    labels
        .iter()
        .filter_map(|l| pick(l.as_ref()))
        .max()
        .unwrap_or(0)
}

/// The highest level among `labels`' **operator** labels (0 = none).
#[must_use]
pub fn own_level_in<S: AsRef<str>>(table: &[PriorityLevel], labels: &[S]) -> u8 {
    max_level(labels, |l| by_operator_label(table, l).map(|r| r.level))
}

/// The highest level among `labels`' **inherited** labels (0 = none).
#[must_use]
pub fn inherited_level_in<S: AsRef<str>>(table: &[PriorityLevel], labels: &[S]) -> u8 {
    max_level(labels, |l| by_inherited_label(table, l).map(|r| r.level))
}

/// The effective level `labels` carry: own or inherited, whichever is
/// higher (0 = not starred at all).
#[must_use]
pub fn level_in<S: AsRef<str>>(table: &[PriorityLevel], labels: &[S]) -> u8 {
    own_level_in(table, labels).max(inherited_level_in(table, labels))
}

/// [`level_in`] over the production table.
#[must_use]
pub fn level<S: AsRef<str>>(labels: &[S]) -> u8 {
    level_in(LEVELS, labels)
}

/// Whether `labels` count as starred: any level, own or inherited.
#[must_use]
pub fn is_starred<S: AsRef<str>>(labels: &[S]) -> bool {
    level(labels) >= 1
}

/// Whether `label` is any level's operator or inherited label (never a
/// hold, whatever its prefix).
#[must_use]
pub fn is_level_label(label: &str) -> bool {
    by_operator_label(LEVELS, label).is_some() || by_inherited_label(LEVELS, label).is_some()
}

/// Every operator label, ascending by level.
#[must_use]
pub fn operator_labels(table: &[PriorityLevel]) -> Vec<&'static str> {
    table.iter().map(|r| r.operator_label).collect()
}

/// Every label that makes an item starred (operator and inherited labels),
/// ascending by level, operator label first within a level. The listings a
/// starred-first pass reads.
#[must_use]
pub fn starred_labels(table: &[PriorityLevel]) -> Vec<&'static str> {
    table
        .iter()
        .flat_map(|r| std::iter::once(r.operator_label).chain(r.inherited_label))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A level-3 row, exactly as the issue sketches it.
    pub(crate) const LEVEL_3: PriorityLevel = PriorityLevel {
        level: 3,
        glyph: "⭐⭐⭐",
        name: "operator top priority",
        operator_label: "loom:operator-top-priority",
        inherited_label: Some("loom:top-priority-inherited"),
        default_cap: Some(2),
    };

    fn with_level_3() -> Vec<PriorityLevel> {
        let mut t = LEVELS.to_vec();
        t.push(LEVEL_3);
        t
    }

    #[test]
    fn the_table_is_ascending_with_one_row_per_level_starting_at_the_star() {
        assert_eq!(LEVELS[0].operator_label, "loom:operator-priority");
        assert_eq!(LEVELS[0].level, 1);
        for w in LEVELS.windows(2) {
            assert_eq!(w[1].level, w[0].level + 1, "levels are contiguous");
        }
        assert_eq!(row(LEVELS, 2).unwrap().operator_label, OPERATOR_HIGH_PRIORITY_LABEL);
        assert_eq!(row(LEVELS, 2).unwrap().inherited_label, Some(HIGH_PRIORITY_INHERITED_LABEL));
        assert_eq!(row(LEVELS, 2).unwrap().default_cap, Some(5));
    }

    #[test]
    fn every_level_label_is_a_priority_label_in_the_registry() {
        let reg = crate::label_registry::Registry::embedded();
        for name in starred_labels(LEVELS) {
            let label = reg
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing from labels.json"));
            assert_eq!(label.kind, "priority", "{name}");
            assert!(!label.hold && !label.park && !label.skip, "{name} is not a hold");
            assert!(!name.contains("urgent"), "{name}: avoid the retired loom:urgent's word");
        }
    }

    #[test]
    fn the_effective_level_is_the_highest_own_or_inherited_label() {
        assert_eq!(level::<&str>(&[]), 0);
        assert_eq!(level(&["loom:issue"]), 0);
        assert_eq!(level(&["loom:operator-priority"]), 1);
        assert_eq!(level(&["loom:operator-high-priority"]), 2);
        assert_eq!(level(&["loom:high-priority-inherited"]), 2);
        assert_eq!(level(&["loom:operator-priority", "loom:high-priority-inherited"]), 2);
        assert_eq!(own_level_in(LEVELS, &["loom:high-priority-inherited"]), 0);
        assert!(is_starred(&["loom:operator-high-priority"]), "levels nest: 2 counts as starred");
        assert!(is_starred(&["loom:high-priority-inherited"]));
    }

    #[test]
    fn level_labels_are_never_holds_and_an_inherited_label_is_not_an_operator_label() {
        for l in starred_labels(LEVELS) {
            assert!(is_level_label(l));
        }
        assert!(!is_level_label("loom:operator-only"));
        assert!(by_operator_label(LEVELS, HIGH_PRIORITY_INHERITED_LABEL).is_none());
        assert_eq!(
            operator_labels(LEVELS),
            vec!["loom:operator-priority", "loom:operator-high-priority"]
        );
    }

    /// AC: adding a level-3 row is enough for the level to be read (the
    /// ordering and inheritance tests that take the table build on this).
    #[test]
    fn a_level_3_row_is_read_with_no_other_change() {
        let t = with_level_3();
        assert_eq!(level_in(&t, &["loom:operator-top-priority"]), 3);
        assert_eq!(
            level_in(&t, &["loom:top-priority-inherited", "loom:operator-high-priority"]),
            3
        );
        assert_eq!(level_in(LEVELS, &["loom:operator-top-priority"]), 0, "unknown without the row");
        assert_eq!(starred_labels(&t).len(), 5);
    }
}
