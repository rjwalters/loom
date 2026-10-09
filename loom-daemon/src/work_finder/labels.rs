//! Work-finder label constants — split out as a sibling module rather than
//! appended to `work_finder.rs`: that module is over the 1000-line ratchet
//! threshold and frozen at its current size (see
//! `.loom/docs/file-size-policy.md`).
//!
//! The label *sets* here ([`PARK_LABELS`], [`SKIP_LABELS`]) are derived from
//! the label registry's `park` / `skip` properties (`defaults/labels.json`,
//! #10013), never hand-listed: changing which labels park or skip is an edit
//! to the registry. The single-label constants are names, not sets; the
//! registry tests check each one carries the properties its doc claims.

use crate::label_registry::{embedded_set, LabelSet, Registry};

/// Labels marking a **deliberate park** — a human (or an agent acting on a
/// human's behalf) has taken the issue out of the automation queue and it must
/// stay out until the label is cleared (Issue #4444).
///
/// This is the strict subset of [`SKIP_LABELS`] that survives *every* dispatch
/// route, so it is the constant the dispatch-time guard in
/// `SweepRegistry::dispatch()` (step 2.7) consults. It deliberately EXCLUDES
/// [`BUILDING_LABEL`]: `loom:building` is legitimately present on the daemon's
/// own in-flight claim, so a guard that refused it would break the watchdogs'
/// cancel-and-re-dispatch and the reaper's checkpoint-resume — both of which
/// re-dispatch an issue the daemon itself already flipped to `loom:building`.
///
/// **One narrow exemption exists (#6893).** `loom:operator-only`'s park is
/// capability-aware for the `loom:operator-mechanical` sub-kind *only*: an item
/// carrying both labels, declaring `<!-- loom:capability=<name> -->` markers
/// (#6892) that this worker's own `LOOM_WORKER_CAPABILITIES` declaration fully
/// covers, may be dispatched into a propose-mode lane instead of parked. See
/// [`WorkItem::is_skipped_with_capabilities`] and [`crate::capability`]. The
/// list here is unchanged and stays the authoritative *set* of park labels —
/// the exemption is applied by the callers that opt into it, never by removing
/// a label from this constant, and it is inert unless a host opts in.
///
/// **`loom:operator` deliberately does NOT belong here** — it is re-evaluable
/// by design and must never refuse the routes that re-evaluate held work. See
/// [`OPERATOR_HOLD_LABEL`] for the full vibesql#6664 rationale.
///
/// Derived: the registry's `park` labels, in registry order (the order the
/// park guards report a label in when an issue carries several).
pub static PARK_LABELS: LabelSet = LabelSet::new(|| embedded_set("park"));

/// The daemon's own claim label. Disqualifies a *fresh* work-finder candidate
/// (a `loom:building` row is already being worked), but is NOT a park — see
/// [`PARK_LABELS`].
pub const BUILDING_LABEL: &str = "loom:building";

/// The generic operator hold — "the engine has stopped on this artifact and a
/// human must act" (`defaults/docs/label-state-machine.md`). Disqualifies a
/// *fresh work-finder candidate* exactly like [`BUILDING_LABEL`] does, but is
/// **not** a park ([`PARK_LABELS`]) and must never become one: the hold is
/// re-evaluable by design, and the park-guarded routes (watchdog re-dispatch,
/// reaper checkpoint-resume, explicit `loom-daemon dispatch <N>`) must keep
/// reaching held items.
///
/// vibesql#6664: a sweep that concludes "a human is needed" releases its claim
/// (restoring `loom:issue`) and applies this label in one motion. Without this
/// constant in [`SKIP_LABELS`] the work finder immediately re-listed the issue
/// and dispatched another `--claim-owned` builder onto the fresh hold —
/// observed 3× in 13 minutes on vibesql#6172 (each new session noticed the
/// hold in the comment trail and declined, which is luck, not a contract).
/// Skipping the candidate here IS the contract; the human (or the re-evaluation
/// lanes) takes it from there. This is the single home of the vibesql#6664
/// rationale — [`PARK_LABELS`] and [`SKIP_LABELS`] point back here rather than
/// repeating it.
pub const OPERATOR_HOLD_LABEL: &str = "loom:operator";

/// Labels that disqualify an issue from dispatch even if it still appears in
/// the `loom:issue`-filtered listing.
///
/// A `loom:issue` row should never itself carry these (they are mutually
/// exclusive states in the `.github/labels.yml` state machine), but `gh`'s
/// label cache can be briefly stale, so the finder checks defensively.
///
/// Derived from the registry's `skip` property, so it can never drift from
/// [`PARK_LABELS`] (#4444: every park label is also a skip label, which
/// `Registry::validate` enforces). Ordered as the claim ([`BUILDING_LABEL`]), the
/// parks, the generic hold ([`OPERATOR_HOLD_LABEL`]), then the sub-kinds that
/// require a base label ([`OPERATOR_DECISION_LABEL`]). The operator hold sits
/// in this list but NOT in [`PARK_LABELS`] — see [`OPERATOR_HOLD_LABEL`] for why.
pub static SKIP_LABELS: LabelSet = LabelSet::new(|| {
    let reg = Registry::embedded();
    let mut skip = embedded_set("skip");
    skip.sort_by_key(|name| {
        let l = reg.get(name).expect("with_property names a registry label");
        (l.kind != "claim", !l.park, l.requires_base.is_some())
    });
    skip
});

/// The `loom:operator-only` decision sub-kind (#5671): an owner has to rule.
///
/// It always accompanies `loom:operator-only`, so on a `loom:issue` row it was
/// already skipped through that base label. #9244 made it a skip label in its
/// own right: a starred (`loom:operator-priority`) issue now reaches the
/// candidate list from outside `loom:issue`, and starring an item that is
/// waiting on an owner's decision must never dispatch a sweep onto it, even
/// if the base label was dropped. Not a park ([`PARK_LABELS`]): the base
/// label already is one wherever it matters.
///
/// Membership in [`SKIP_LABELS`] now comes from the registry's `skip`
/// property (#10013), so outside tests this name is documentation only.
#[cfg_attr(not(test), allow(dead_code))]
pub const OPERATOR_DECISION_LABEL: &str = "loom:operator-decision";

/// Log — at DEBUG, once per skipped candidate — that a candidate was dropped
/// for carrying a hard-exclusion label (#7528), naming the rule.
///
/// DEBUG rather than INFO on purpose. The candidate listing re-evaluates the
/// same rows every tick, so an INFO here would reproduce the #6440
/// 865-refusals-in-an-hour shape for an intake backlog that is doing exactly
/// what it should (sitting still until a maintainer clears the label). The
/// operator-visible signal is the per-tick `declined-skip` count on the
/// `work_finder: tick — …` line, plus the reaper's threshold WARN
/// (`SweepRegistry::record_decline`) for an issue that actually reached
/// dispatch and burned a session.
pub(super) fn log_hard_exclusion_skip(issue: u32, rule: &str) {
    log::debug!(
        "work_finder: skipping issue #{issue} — carries the hard-exclusion label `{rule}`, \
         which every Loom role declines on; a maintainer must remove it (or close the issue) \
         before it is dispatchable (#7528)"
    );
}
