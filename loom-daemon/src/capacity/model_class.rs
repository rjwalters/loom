//! Per-model-class token-pool capacity (#8058 Phase 3).
//!
//! Phase 1 (#8090) taught `.bad_tokens` to carry a `[model-class:<class>]`
//! marker so an Opus ceiling no longer bad-marks an account for Haiku, and
//! Phase 2 (#8241) gave `tokens_pool::health` the same per-class shape for
//! every non-Claude provider. Both phases changed what the *selector* does.
//! Neither changed what an operator **sees**: every health/status surface
//! still printed one account-wide number, which by construction counts a
//! class-scoped hold as a whole-account outage. A pool reading `2/20 healthy`
//! while eighteen accounts were perfectly able to serve Sonnet was
//! indistinguishable from a genuinely dead pool.
//!
//! This module is the single computation behind the per-class counts on
//! `loom-daemon health` (the `tokens` section) and `loom-daemon status` (the
//! `Token capacity:` block), so the two can never disagree.
//!
//! # Degradation is the contract
//!
//! [`ClassCapacity::by_class`] is **empty** unless `.bad_tokens` actually
//! names at least one model class. Every renderer is written to emit exactly
//! its pre-#8058 line in that case, so a pool with no class-scoped state (the
//! overwhelmingly common one — see the "no producer yet" note on #8277) reads
//! byte-for-byte as it did before this phase. Per-class counts appear only
//! once there is per-class state to report.
//!
//! # Narrower, never wider
//!
//! A class-scoped query is strictly narrower than the account-wide one (see
//! [`super::super::tokens_pool::bad_tokens::blocking_entry_in_dir_for_class`]),
//! so `by_class[c] >= healthy` always holds: the per-class figure can only
//! ever reveal capacity the account-wide number was hiding, never claim
//! capacity the selector would refuse to hand out.
//!
//! # Why this is not on the work-finder hot path
//!
//! Resolving one class costs a `.bad_tokens` re-read per ranking row, so the
//! whole snapshot is `O(rows x classes)` file reads. That is fine for the
//! interactive CLI surfaces this serves (a human typed `health` or `status`)
//! and deliberately not wired into [`super::read_ranking_at`], which the work
//! finder calls every tick. The account-wide count the tick loop reads is
//! unchanged by this module.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::tokens_pool::bad_tokens;
use crate::tokens_pool::select::parse_ranking_line;

use super::AccountHealth;

/// Per-model-class healthy counts for one resolved token-pool directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassCapacity {
    /// Accounts listed in `.ranking` — the same inventory
    /// [`super::read_ranking_at`] counts as `total`.
    pub total: usize,
    /// Accounts healthy to the **account-wide** question: the single number
    /// every pre-#8058 surface printed, recomputed here from the same inputs
    /// so a renderer can put the per-class breakdown next to a figure it knows
    /// was derived identically.
    pub healthy: usize,
    /// Healthy count per model class, keyed by the `.bad_tokens` class
    /// vocabulary (`haiku` / `sonnet` / `opus` / `fable`, see
    /// [`bad_tokens::model_class_of`]).
    ///
    /// **Empty means "no class-scoped state exists"**, not "no class has
    /// capacity" — callers must render today's single number in that case.
    pub by_class: BTreeMap<String, usize>,
}

impl ClassCapacity {
    /// Whether there is any per-class state worth showing. `false` ⇒ every
    /// renderer degrades to the pre-#8058 single number.
    #[must_use]
    pub fn has_class_state(&self) -> bool {
        !self.by_class.is_empty()
    }

    /// The classes whose healthy count **exceeds** the account-wide count —
    /// i.e. the classes that still have capacity the headline number hides.
    ///
    /// This is the actionable subset: a class whose count equals `healthy`
    /// adds nothing an operator can act on, because every account blocked for
    /// it is blocked account-wide anyway.
    #[must_use]
    pub fn classes_with_hidden_capacity(&self) -> Vec<(&str, usize)> {
        self.by_class
            .iter()
            .filter(|(_, n)| **n > self.healthy)
            .map(|(c, n)| (c.as_str(), *n))
            .collect()
    }

    /// A compact `per class: opus 2/20, sonnet 18/20` fragment, or an empty
    /// string when there is no class-scoped state.
    ///
    /// Every class is listed once there is any class state at all (not just
    /// [`Self::classes_with_hidden_capacity`]): an operator reading
    /// "per class: opus 2/20" needs to see that Sonnet is also 2/20 to
    /// conclude the pool is genuinely dead rather than merely Opus-starved.
    #[must_use]
    pub fn summary_fragment(&self) -> String {
        if self.by_class.is_empty() {
            return String::new();
        }
        let per_class = self
            .by_class
            .iter()
            .map(|(class, n)| format!("{class} {n}/{}", self.total))
            .collect::<Vec<_>>()
            .join(", ");
        format!("per class: {per_class}")
    }

    /// [`Self::summary_fragment`] wrapped as a ` (…)` suffix for splicing into
    /// an existing one-line summary, or an empty string when there is no
    /// class-scoped state — so a caller can concatenate unconditionally and
    /// still emit its exact pre-#8058 line.
    #[must_use]
    pub fn summary_suffix(&self) -> String {
        let fragment = self.summary_fragment();
        if fragment.is_empty() {
            String::new()
        } else {
            format!(" ({fragment})")
        }
    }

    /// The structured `--json` payload for this snapshot: a plain
    /// `class -> healthy` object, omitted entirely (`None`) when there is no
    /// class-scoped state so a JSON consumer sees the pre-#8058 shape
    /// unchanged.
    #[must_use]
    pub fn detail(&self) -> Option<serde_json::Value> {
        if self.by_class.is_empty() {
            return None;
        }
        Some(serde_json::json!(self.by_class))
    }
}

/// [`ClassCapacity::summary_suffix`] for an optional snapshot — `""` when
/// there is none, so a renderer can splice it into its existing one-line
/// summary unconditionally and still emit its exact pre-#8058 text.
///
/// A free function rather than a method so a caller holding
/// `Option<&ClassCapacity>` (every renderer does: the snapshot is absent
/// whenever there is no readable `.ranking`) needs one expression, not a
/// `map`/`unwrap_or_default` chain, at each of the surfaces this must stay
/// identical across.
#[must_use]
pub fn summary_suffix_of(cap: Option<&ClassCapacity>) -> String {
    cap.map(ClassCapacity::summary_suffix).unwrap_or_default()
}

/// The `class -> healthy` JSON object for an optional snapshot, empty when
/// there is no class-scoped state.
///
/// Deliberately an empty **object** rather than `null`/an absent key: the
/// field's type never changes shape, so a `--json` consumer can read it
/// unconditionally, and "no class-scoped state" and "a class with no
/// capacity" stay distinguishable (`{}` vs `{"opus": 0}`).
#[must_use]
pub fn detail_of(cap: Option<&ClassCapacity>) -> serde_json::Value {
    cap.and_then(ClassCapacity::detail)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Every model class named by any `.bad_tokens` line in `pool_dir`.
///
/// Deliberately liveness-**agnostic**: this only decides whether *any*
/// class-scoped state exists and which classes to name beyond the known
/// vocabulary. Whether a given account is actually held for one is answered
/// per account by [`bad_tokens::blocking_entry_in_dir_for_class`], the same
/// authoritative, cooldown-and-session-aware scan the selector uses — so a
/// stale marker can widen the question but can never fabricate a block.
///
/// Parsing reuses [`bad_tokens::reason_model_class`] (the tolerant marker
/// reader) applied to the whole line rather than a re-split `reason` field:
/// the marker is a substring search, so the two are equivalent, and going
/// through the public parser keeps exactly one definition of the marker
/// syntax in the tree. The marker is then normalized through
/// [`bad_tokens::model_class_of`] for the same reason the blocking predicate
/// does: a marker that normalizes to nothing is **class-less** (it blocks
/// everything), so it must not be reported here as a class either — the two
/// would otherwise disagree about what the file says.
fn classes_named_in_bad_tokens(pool_dir: &Path) -> BTreeSet<String> {
    let path = pool_dir.join(".bad_tokens");
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    text.lines()
        .filter_map(bad_tokens::reason_model_class)
        .filter_map(bad_tokens::model_class_of)
        .collect()
}

/// The classes to report on, given the classes `.bad_tokens` actually names.
///
/// Empty in, empty out — that is the degradation contract. Once *any* class
/// is named, the report covers the whole known vocabulary
/// ([`crate::script_helpers::model_tiers::TASK_TOOL_ALIASES`], the list
/// [`bad_tokens::model_class_of`] normalizes into and therefore a superset of
/// `named`), because the un-named classes are the entire point: the operator
/// staring at `2/20 healthy` needs to be told that Sonnet is 20/20, and a
/// class with no marks at all can never appear in the file.
fn classes_to_report(named: &BTreeSet<String>) -> BTreeSet<String> {
    if named.is_empty() {
        return BTreeSet::new();
    }
    crate::script_helpers::model_tiers::TASK_TOOL_ALIASES
        .iter()
        .map(|c| (*c).to_string())
        .collect()
}

/// [`read_class_capacity_at`] for an **optional** pool directory — `None` in,
/// `None` out.
///
/// That is the shape every caller actually holds: the directory arrives from
/// the daemon's optional [`crate::types::DaemonStatusReport::token_pool_dir`]
/// (#4292), absent on a pre-#4292 or unreachable daemon. Having the one
/// helper keeps each collection site a single expression, so the three
/// surfaces this must stay identical across cannot drift apart.
#[must_use]
pub fn read_for_pool_dir(pool_dir: Option<&Path>) -> Option<ClassCapacity> {
    read_class_capacity_at(pool_dir?)
}

/// Per-class healthy counts for the pool at `pool_dir`, or `None` when
/// `.ranking` is absent/unparseable.
///
/// `None` is the same "no probe data exists" signal
/// [`super::read_ranking_at`] returns, and means the caller should fall back
/// to whatever it already did without a ranking. A `Some` snapshot with an
/// empty [`ClassCapacity::by_class`] is the different (and far more common)
/// "ranking exists, no class-scoped state" case.
///
/// The account-wide `healthy` count is computed exactly as
/// [`super::read_ranking_at`] computes its `available`: a row whose status
/// word is `available` is re-verified against the LIVE `.bad_tokens` state
/// (#7522) and downgraded when it disagrees.
#[must_use]
pub fn read_class_capacity_at(pool_dir: &Path) -> Option<ClassCapacity> {
    let text = std::fs::read_to_string(pool_dir.join(".ranking")).ok()?;
    let classes = classes_to_report(&classes_named_in_bad_tokens(pool_dir));
    let mut cap = ClassCapacity {
        by_class: classes.iter().map(|c| (c.clone(), 0)).collect(),
        ..ClassCapacity::default()
    };
    for line in text.lines() {
        // Mirror `read_ranking_at`: a row with no `|` is genuinely malformed
        // and is skipped rather than counted with an empty status.
        if !line.contains('|') {
            continue;
        }
        let Some(row) = parse_ranking_line(line) else {
            continue;
        };
        cap.total += 1;
        // A row the probe already called unhealthy is unhealthy for every
        // class. Only an `available` row can have its fate decided by a
        // class-scoped mark, which is precisely the #8058 case.
        if AccountHealth::parse(&row.status) != AccountHealth::Available {
            continue;
        }
        if bad_tokens::blocking_entry_in_dir(pool_dir, &row.name).is_none() {
            cap.healthy += 1;
        }
        for (class, count) in &mut cap.by_class {
            if bad_tokens::blocking_entry_in_dir_for_class(pool_dir, &row.name, Some(class))
                .is_none()
            {
                *count += 1;
            }
        }
    }
    if cap.total == 0 {
        return None;
    }
    Some(cap)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{read_class_capacity_at, ClassCapacity};
    use std::collections::BTreeMap;
    use std::path::Path;

    fn pool(ranking: &str, bad_tokens: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_pool(dir.path(), ranking, bad_tokens);
        dir
    }

    fn write_pool(dir: &Path, ranking: &str, bad_tokens: &str) {
        std::fs::write(dir.join(".ranking"), ranking).unwrap();
        if !bad_tokens.is_empty() {
            std::fs::write(dir.join(".bad_tokens"), bad_tokens).unwrap();
        }
    }

    /// A `.bad_tokens` line timestamped now, so its exhaustion cooldown is
    /// unambiguously live.
    fn fresh(name: &str, reason: &str) -> String {
        format!("{} {name} {reason}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"))
    }

    fn counts(cap: &ClassCapacity) -> BTreeMap<&str, usize> {
        cap.by_class.iter().map(|(c, n)| (c.as_str(), *n)).collect()
    }

    // ---- degradation ---------------------------------------------------

    /// AC2's degradation clause: a pool with no class-scoped mark reports no
    /// class state at all, so every renderer emits its pre-#8058 line.
    #[test]
    fn no_class_marks_means_no_class_state() {
        let dir = pool("a|available\nb|available\nc|exhausted\n", "");
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.total, 3);
        assert_eq!(cap.healthy, 2);
        assert!(!cap.has_class_state());
        assert_eq!(cap.summary_suffix(), "");
        assert_eq!(cap.detail(), None);
    }

    /// A class-LESS `.bad_tokens` entry is account-wide state, not class
    /// state: it lowers `healthy` and still reports no per-class breakdown.
    #[test]
    fn a_class_less_mark_is_not_class_state() {
        let dir =
            pool("a|available\nb|available\n", &fresh("a", "exhausted: session limit reached"));
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 1);
        assert!(!cap.has_class_state());
    }

    #[test]
    fn an_absent_ranking_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_class_capacity_at(dir.path()).is_none());
    }

    #[test]
    fn an_unparseable_ranking_is_none() {
        let dir = pool("# comment only\nnot-a-row\n", "");
        assert!(read_class_capacity_at(dir.path()).is_none());
    }

    // ---- the actual observability gap -----------------------------------

    /// The #8058 headline case: one class at its ceiling on most accounts
    /// reads as a nearly-dead pool account-wide, while every other class
    /// still has capacity. That divergence is the whole point of this phase.
    #[test]
    fn a_class_scoped_mark_hides_capacity_from_the_account_wide_count() {
        let mut marks = String::new();
        for name in ["a", "b", "c"] {
            marks.push_str(&fresh(name, "exhausted: model credits [model-class:opus]"));
        }
        let dir = pool("a|available\nb|available\nc|available\nd|available\n", &marks);
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.total, 4);
        // Account-wide: a class-scoped hold still blocks the class-less
        // question, exactly as Phase 1 specified.
        assert_eq!(cap.healthy, 1);
        // Per class: opus really is down to one, but nothing else is — and
        // the classes with NO marks are reported too, because they are the
        // hidden capacity the headline `1/4` was concealing.
        assert_eq!(
            counts(&cap),
            BTreeMap::from([("fable", 4), ("haiku", 4), ("opus", 1), ("sonnet", 4)])
        );
        assert_eq!(
            cap.classes_with_hidden_capacity(),
            vec![("fable", 4), ("haiku", 4), ("sonnet", 4)]
        );
        assert_eq!(
            cap.summary_suffix(),
            " (per class: fable 4/4, haiku 4/4, opus 1/4, sonnet 4/4)"
        );
    }

    /// Two classes held on disjoint accounts: each class's count reflects
    /// only its own holds, and every class still exceeds the account-wide
    /// figure of 1.
    #[test]
    fn classes_are_counted_independently() {
        let mut marks = fresh("a", "exhausted: credits [model-class:opus]");
        marks.push_str(&fresh("b", "exhausted: credits [model-class:sonnet]"));
        let dir = pool("a|available\nb|available\nc|available\n", &marks);
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 1);
        assert_eq!(
            counts(&cap),
            BTreeMap::from([("fable", 3), ("haiku", 3), ("opus", 2), ("sonnet", 2)])
        );
        assert_eq!(
            cap.summary_suffix(),
            " (per class: fable 3/3, haiku 3/3, opus 2/3, sonnet 2/3)"
        );
    }

    /// A marker naming something outside the known vocabulary normalizes to
    /// nothing, which Phase 1 defines as **class-less** — it blocks every
    /// class. This surface must agree: it reports no class state at all,
    /// rather than inventing a class whose count would contradict the
    /// selector.
    #[test]
    fn an_unnormalizable_class_name_reads_as_account_wide() {
        let dir = pool(
            "a|available\nb|available\n",
            &fresh("a", "exhausted: credits [model-class:gpt-5-codex]"),
        );
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 1);
        assert!(!cap.has_class_state());
    }

    /// A marker written by a caller with no classifier of its own — the raw
    /// `$LOOM_MODEL` `claude-wrapper.sh` appends — normalizes to its family
    /// class, exactly as the blocking predicate reads it.
    #[test]
    fn a_raw_model_id_marker_normalizes_to_its_class() {
        let dir = pool(
            "a|available\nb|available\n",
            &fresh("a", "exhausted: credits [model-class:claude-opus-5]"),
        );
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 1);
        assert_eq!(cap.by_class.get("opus"), Some(&1));
        assert_eq!(cap.by_class.get("sonnet"), Some(&2));
    }

    /// Narrower, never wider (#8058's design constraint): no class may ever
    /// report fewer healthy accounts than the account-wide question, because
    /// every account-wide block is checked first and identically.
    #[test]
    fn no_class_count_is_ever_below_the_account_wide_count() {
        let mut marks = fresh("a", "auth: 401 Invalid bearer token");
        marks.push_str(&fresh("b", "exhausted: session limit reached"));
        marks.push_str(&fresh("c", "exhausted: credits [model-class:opus]"));
        let dir = pool("a|available\nb|available\nc|available\nd|available\n", &marks);
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 1);
        for (class, n) in &cap.by_class {
            assert!(
                *n >= cap.healthy,
                "class {class} reported {n} healthy, below the account-wide {}",
                cap.healthy
            );
        }
        // `a` is auth-dead (permanent, every class) and `b` is account-wide
        // exhausted, so no class recovers those two. Opus additionally loses
        // `c`; every other class keeps it.
        assert_eq!(
            counts(&cap),
            BTreeMap::from([("fable", 2), ("haiku", 2), ("opus", 1), ("sonnet", 2)])
        );
    }

    /// An auth entry blocks every class whatever marker its reason carries —
    /// a broken credential is broken for all of them (Phase 1's edge case).
    #[test]
    fn an_auth_entry_blocks_every_class_despite_a_marker() {
        let dir = pool(
            "a|available\nb|available\n",
            &fresh("a", "auth-dead: 401 Invalid bearer token [model-class:opus]"),
        );
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 1);
        // `a` is out for every class including the one its marker names.
        assert_eq!(
            counts(&cap),
            BTreeMap::from([("fable", 1), ("haiku", 1), ("opus", 1), ("sonnet", 1)])
        );
    }

    /// A row the probe already called unhealthy is unhealthy for every class:
    /// no class-scoped mark can resurrect an `exhausted`/`blocked` row.
    #[test]
    fn a_non_available_row_counts_for_no_class() {
        let dir = pool(
            "a|exhausted\nb|blocked\nc|rate_limited\nd|available\n",
            &fresh("d", "exhausted: credits [model-class:opus]"),
        );
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.total, 4);
        assert_eq!(cap.healthy, 0);
        // Only `d` was ever a candidate, and its opus mark takes opus to 0.
        assert_eq!(
            counts(&cap),
            BTreeMap::from([("fable", 1), ("haiku", 1), ("opus", 0), ("sonnet", 1)])
        );
    }

    /// An EXPIRED class-scoped mark still names its class (so the class is
    /// reported), but blocks nobody — the liveness decision is delegated to
    /// the same scan the selector uses, never to the marker's presence.
    #[test]
    fn an_expired_class_mark_names_its_class_but_blocks_nobody() {
        let old = chrono::Utc::now() - chrono::Duration::days(30);
        let dir = pool(
            "a|available\nb|available\n",
            &format!(
                "{} a exhausted: credits [model-class:opus]\n",
                old.format("%Y-%m-%dT%H:%M:%SZ")
            ),
        );
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 2);
        assert_eq!(
            counts(&cap),
            BTreeMap::from([("fable", 2), ("haiku", 2), ("opus", 2), ("sonnet", 2)])
        );
    }

    /// A malformed marker reads as account-wide (Phase 1's fail-safe
    /// direction), so it contributes no class and still blocks everything.
    #[test]
    fn a_malformed_marker_stays_account_wide() {
        let dir =
            pool("a|available\nb|available\n", &fresh("a", "exhausted: credits [model-class:"));
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.healthy, 1);
        assert!(!cap.has_class_state());
    }

    // ---- rendering helpers ----------------------------------------------

    #[test]
    fn summary_fragment_is_empty_without_class_state() {
        assert_eq!(ClassCapacity::default().summary_fragment(), "");
        assert_eq!(ClassCapacity::default().summary_suffix(), "");
        assert_eq!(ClassCapacity::default().detail(), None);
    }

    #[test]
    fn detail_is_a_plain_class_to_count_object() {
        let cap = ClassCapacity {
            total: 4,
            healthy: 1,
            by_class: BTreeMap::from([("opus".to_string(), 1), ("sonnet".to_string(), 3)]),
        };
        assert_eq!(cap.detail().unwrap(), serde_json::json!({"opus": 1, "sonnet": 3}));
        assert_eq!(cap.summary_fragment(), "per class: opus 1/4, sonnet 3/4");
    }

    /// The ranking format this module reads is the shared one — a legacy
    /// two-field row and a full four-field row both parse, so a per-class
    /// count never depends on the optional trailing columns (the backward
    /// compatibility AC1's negative-finding path pins).
    #[test]
    fn legacy_and_full_ranking_rows_both_parse() {
        let dir = pool(
            "a|available\nb|available|0.42\nc|available|0.10|2026-09-20T03:00:00Z\n",
            &fresh("a", "exhausted: credits [model-class:opus]"),
        );
        let cap = read_class_capacity_at(dir.path()).unwrap();
        assert_eq!(cap.total, 3);
        assert_eq!(cap.healthy, 2);
        assert_eq!(
            counts(&cap),
            BTreeMap::from([("fable", 3), ("haiku", 3), ("opus", 2), ("sonnet", 3)])
        );
    }
}
