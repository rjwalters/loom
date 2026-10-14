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

use crate::label_registry::{embedded_set_with_retired, LabelSet, Registry};

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
/// **One narrow exemption exists (#6893).** The [`OPERATOR_HOLD_LABEL`] park is
/// capability-aware: an item whose body declares
/// `<!-- loom:capability=<name> -->` markers (#6892) that this worker's own
/// `LOOM_WORKER_CAPABILITIES` declaration fully covers, and carries no
/// decision block, may be dispatched into a propose-mode lane instead of
/// parked. See [`WorkItem::is_skipped_with_capabilities`] and
/// [`crate::capability`]. The exemption is applied by the callers that opt
/// into it, never by removing a label from this set, and it is inert unless a
/// host opts in.
///
/// **This is the issue-dispatch park.** The PR lane reads
/// [`PR_LANE_PARK_LABELS`] instead, which leaves [`OPERATOR_HOLD_LABEL`] out.
///
/// Derived: the registry's `park` labels in registry order (the order the park
/// guards report a label in when an issue carries several), then the retired
/// names that fold into one of them (#11795), so an unmigrated
/// `loom:operator-only` item stays parked.
pub static PARK_LABELS: LabelSet = LabelSet::new(|| embedded_set_with_retired("park"));

/// The park set of the PR-lane consumers: the reaper's linked-PR park probe
/// (`pr_park`), the role runner's changes-axis demand, and Doctor's queue in
/// `pr_planning` (#11795).
///
/// [`PARK_LABELS`] minus [`OPERATOR_HOLD_LABEL`], derived explicitly. On a pull
/// request `loom:operator` is not a park: Champion's merge-risk and
/// critical-file holds clear themselves, and Champion routes stale held PRs
/// to Doctor with the label still on, which Doctor drains (#7660). The retired
/// `loom:operator-only` stays in this set, so an unmigrated PR parks exactly
/// as it did before the fold.
pub static PR_LANE_PARK_LABELS: LabelSet = LabelSet::new(|| {
    PARK_LABELS
        .iter()
        .copied()
        .filter(|l| *l != OPERATOR_HOLD_LABEL)
        .collect()
});

/// The daemon's own claim label. Disqualifies a *fresh* work-finder candidate
/// (a `loom:building` row is already being worked), but is NOT a park — see
/// [`PARK_LABELS`].
pub const BUILDING_LABEL: &str = "loom:building";

/// The operator hold (#11795): a human must act or decide now, and only a
/// human removes it. On an issue it is a park ([`PARK_LABELS`]), so no
/// dispatch route reaches it: before the fold `loom:operator-only` carried the
/// park and this label was skip-only, and a hold applied after dispatch was
/// overridden by watchdog re-dispatch, checkpoint resume or the epic
/// supervisor (#4366). On a pull request it is not a park
/// ([`PR_LANE_PARK_LABELS`]).
///
/// What the human must do is read from the body, not from a sub-label: a
/// ```` ```decision ```` block (#10057) makes it a decision, a capability
/// marker with no decision block makes it mechanical (#6893). Readers that
/// must also see the retired names use [`is_operator_hold`].
pub const OPERATOR_HOLD_LABEL: &str = "loom:operator";

/// The blocked hold (#11795): waiting on something named — another change, a
/// capability — and released automatically once it resolves. Folds the
/// retired `loom:needs-capability` (with its capability marker in the body).
pub const BLOCKED_HOLD_LABEL: &str = "loom:blocked";

/// [`OPERATOR_HOLD_LABEL`] and the retired names that fold into it
/// (`loom:operator-only`, `-decision`, `-mechanical`, `-blocked`), from the
/// registry. `loom:operator-blocked` is a human hold (operator, 2026-10-14):
/// a named blocker it waited on stays in the body text.
pub static OPERATOR_HOLD_NAMES: LabelSet =
    LabelSet::new(|| crate::label_registry::embedded_aliases(OPERATOR_HOLD_LABEL));

/// [`BLOCKED_HOLD_LABEL`] and the retired names that fold into it
/// (`loom:needs-capability`), from the registry.
pub static BLOCKED_HOLD_NAMES: LabelSet =
    LabelSet::new(|| crate::label_registry::embedded_aliases(BLOCKED_HOLD_LABEL));

/// Every hold under any of its names (#11795): `loom:blocked`,
/// `loom:operator`, `loom:frozen`, then the retired names that fold into one
/// of them. Derived from the registry's `hold` property. The set a PR-side
/// pass reads for "a human or a hold is involved: leave it alone".
pub static HOLD_NAMES: LabelSet = LabelSet::new(|| embedded_set_with_retired("hold"));

/// Whether `labels` carry the operator hold, under its name or a retired name
/// that folds into it (#11795). Exact match.
#[must_use]
pub fn is_operator_hold<S: AsRef<str>>(labels: &[S]) -> bool {
    labels
        .iter()
        .any(|l| OPERATOR_HOLD_NAMES.contains(&l.as_ref()))
}

/// Whether `labels` carry the blocked hold, under its name or a retired name
/// that folds into it (#11795). Exact match.
#[must_use]
pub fn is_blocked_hold<S: AsRef<str>>(labels: &[S]) -> bool {
    labels
        .iter()
        .any(|l| BLOCKED_HOLD_NAMES.contains(&l.as_ref()))
}

/// Labels that disqualify an issue from dispatch even if it still appears in
/// the `loom:issue`-filtered listing.
///
/// A `loom:issue` row should never itself carry these (they are mutually
/// exclusive states in the `.github/labels.yml` state machine), but `gh`'s
/// label cache can be briefly stale, so the finder checks defensively.
///
/// Derived from the registry's `skip` property (plus the retired names that
/// fold into a skip label, #11795), so it can never drift from
/// [`PARK_LABELS`] (#4444: every park label is also a skip label, which
/// `Registry::validate` enforces). Ordered as the claim ([`BUILDING_LABEL`]),
/// the parks, then the skip-only labels ([`EPIC_LABEL`],
/// `loom:decision-malformed`); a retired name sorts as its replacement, after
/// the active names of its group.
///
/// **Not every member is a human hold.** [`BUILDING_LABEL`] is the daemon's
/// own claim and [`EPIC_LABEL`] marks a tracking container; a reader that
/// needs "a human must act" uses [`is_operator_hold`] or the registry's
/// `hold` / `operator_hold` properties, never membership here.
pub static SKIP_LABELS: LabelSet = LabelSet::new(|| {
    let reg = Registry::embedded();
    let mut skip = embedded_set_with_retired("skip");
    skip.sort_by_key(|name| {
        let retired = reg.get_retired(name);
        let active = retired
            .and_then(|r| r.replaced_by.as_deref())
            .unwrap_or(name);
        let l = reg
            .get(active)
            .expect("with_property names a registry label");
        (l.kind != "claim", !l.park, retired.is_some())
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
/// daemon's own claim, which every release removes). The retired hold names
/// are skip entries (#11795), so an unmigrated item stays vetoed. `removing` names the
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
    labels
        .iter()
        .map(String::as_str)
        .find(|l| !removing.contains(l) && *l != BUILDING_LABEL && SKIP_LABELS.contains(l))
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
