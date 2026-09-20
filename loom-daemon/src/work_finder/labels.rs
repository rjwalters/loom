//! Work-finder label constants — split out as a sibling module rather than
//! appended to `work_finder.rs`: that module is over the 1000-line ratchet
//! threshold and frozen at its current size (see
//! `.loom/docs/file-size-policy.md`).

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
pub const PARK_LABELS: &[&str] = &["loom:blocked", "loom:operator-only"];

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
/// Composed as [`BUILDING_LABEL`] + [`PARK_LABELS`] + [`OPERATOR_HOLD_LABEL`]
/// rather than re-listing the label strings, so the constants can never drift
/// apart (#4444). The operator hold sits in this list but NOT in
/// [`PARK_LABELS`] — see [`OPERATOR_HOLD_LABEL`] for why.
pub const SKIP_LABELS: &[&str] = &[
    BUILDING_LABEL,
    PARK_LABELS[0],
    PARK_LABELS[1],
    OPERATOR_HOLD_LABEL,
];
