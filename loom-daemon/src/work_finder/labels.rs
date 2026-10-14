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
/// parks, the skip-only labels in registry order (the generic hold
/// [`OPERATOR_HOLD_LABEL`], then [`EPIC_LABEL`]), then the sub-kinds that
/// require a base label ([`OPERATOR_DECISION_LABEL`]). The operator hold sits
/// in this list but NOT in [`PARK_LABELS`] — see [`OPERATOR_HOLD_LABEL`] for why.
///
/// **Not every member is a human hold.** [`BUILDING_LABEL`] is the daemon's
/// own claim and [`EPIC_LABEL`] marks a tracking container; a reader that
/// needs "a human must act" keys on the registry's `hold` / `operator_hold`
/// properties (or [`PARK_LABELS`]), never on membership here.
pub static SKIP_LABELS: LabelSet = LabelSet::new(|| {
    let reg = Registry::embedded();
    let mut skip = embedded_set("skip");
    skip.sort_by_key(|name| {
        let l = reg.get(name).expect("with_property names a registry label");
        (l.kind != "claim", !l.park, l.requires_base.is_some())
    });
    skip
});

/// The epic tracking container (#11399). An epic is decomposed into
/// `loom:epic-phase` children by Champion (or the epic supervisor); it is never
/// build work itself, so a session dispatched onto one can only skip it.
///
/// A `loom:issue` row can legitimately carry it (nothing forbids approving an
/// epic), and before #11399 the work finder dispatched such a row: the session
/// skipped it, the reaper handed `loom:issue` back, and the finder offered it
/// again until the PR-less retry bound (#7972) parked the epic under a
/// `loom:blocked` that described nothing real. Skipping the candidate costs no
/// session at all.
///
/// A skip, deliberately **not** a park ([`PARK_LABELS`]): the dispatch-time
/// guard keys on parks only, so an operator's explicit `loom-daemon dispatch
/// <N>` on an epic still runs, and the epic supervisor's own listing and its
/// `loom:epic-phase` child dispatches never consult this set. Matching is
/// exact, so `loom:epic-phase` stays dispatchable.
///
/// Membership in [`SKIP_LABELS`] comes from the registry's `skip` property
/// (#10013), so outside tests this name is documentation only.
#[cfg_attr(not(test), allow(dead_code))]
pub const EPIC_LABEL: &str = "loom:epic";

/// The `loom:operator-only` decision sub-kind (#5671): an owner has to rule.
///
/// It always accompanies `loom:operator-only`, so on a `loom:issue` row it was
/// already skipped through that base label. #9244 made it a skip label in its
/// own right: an operator-starred issue then reached the candidate list from
/// outside `loom:issue` (no longer, #11508), and an item waiting on an
/// owner's decision must never get a sweep dispatched onto it, even if the
/// base label was dropped. Not a park ([`PARK_LABELS`]): the base
/// label already is one wherever it matters.
///
/// Membership in [`SKIP_LABELS`] now comes from the registry's `skip`
/// property (#10013), so outside tests this name is documentation only.
#[cfg_attr(not(test), allow(dead_code))]
pub const OPERATOR_DECISION_LABEL: &str = "loom:operator-decision";

/// "Not now, don't touch" (#11743): a human applies it and only a human
/// removes it. It is a park, a skip and a hard exclusion in the registry, so
/// every set-driven route refuses it; [`is_frozen`] is the one check for the
/// daemon passes that must make **no** write to such an item at all (not even
/// removing their own label).
pub const FROZEN_LABEL: &str = "loom:frozen";

/// Whether `labels` carries [`FROZEN_LABEL`] (exact match, #11366).
#[must_use]
pub fn is_frozen(labels: &[String]) -> bool {
    labels.iter().any(|l| l == FROZEN_LABEL)
}

/// The one shared veto on handing an item back to the build queue (#11480):
/// the first label in `labels` that forbids re-adding `loom:issue`, or `None`
/// when the re-add may proceed.
///
/// Every release path that restores `loom:issue` — the reaper's claim release,
/// the quarantine TTL/clear release, the startup quarantine reconciliation,
/// the no-op hold lift and claim reconciliation's reclaim — consults this, so
/// an item a human (or a gate) took out of the queue is not put back by a
/// daemon path that only knew about its own label. Before #11480 each site
/// carried its own partial list, and loom-ui#1950 (`loom:blocked`) was
/// re-dispatched 31 times in 72 h.
///
/// A veto is any [`SKIP_LABELS`] entry other than [`BUILDING_LABEL`] (the
/// daemon's own claim, which every release removes) or any
/// [`crate::capability::LANE_VETO_LABELS`] entry. `removing` names the
/// label(s) the caller is itself taking off in the same edit (the quarantine
/// release removes its own `loom:blocked`), which never veto their own
/// removal. Matching is exact (#11366). `loom:decision-malformed` is a skip
/// label in the registry, so a bounced Decision issue is covered here.
///
/// Pure: labels in, verdict out. Callers decide how to read the labels and
/// fail open (no veto) when the read fails, as their existing probes do.
pub fn restore_veto<'a>(labels: &'a [String], removing: &[&str]) -> Option<&'a str> {
    if is_frozen(labels) {
        return Some(FROZEN_LABEL);
    }
    labels.iter().map(String::as_str).find(|l| {
        !removing.contains(l)
            && *l != BUILDING_LABEL
            && (SKIP_LABELS.contains(l) || crate::capability::LANE_VETO_LABELS.contains(l))
    })
}

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
