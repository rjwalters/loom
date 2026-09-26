//! `merge-pr.sh`'s logic, ported incrementally (#8191, epic #7810).
//!
//! `merge-pr.sh` is the most defect-dense script in the portable pool — 48
//! fix-commits in six months over 1,458 code lines, against a next-worst of
//! 1.4 per 100 — and it performs 21 irreversible operations (API merges,
//! pushes, branch deletion, worktree removal) unattended, under Champion's
//! auto-merge. It is also frozen by the file-size ratchet at 1,460 lines, so
//! the fixes it keeps needing must now be net-zero or smaller. That trap is
//! what makes a port the remedy rather than another patch.
//!
//! At ~3,100 total lines it is three times the watchdog (#8086) and is being
//! taken in slices, each independently reviewable and each keeping the 22
//! retained suites (8,509 assertions' worth of lines) passing unchanged.
//!
//! Slice 1 is [`refs`]: the closing-keyword / partial-increment analysis. It
//! goes first because it is the highest fragility per line — five stacked
//! `awk`/`sed`/`grep -oiE` pipelines whose entire bug history is about what
//! the regex accidentally matched — and because it is pure, which makes a
//! byte-for-byte differential against the shell possible.
//!
//! Slice 2 is [`labels`]: the verdict-label mutual-exclusion guard (#8112) —
//! refusing to merge a PR that carries `loom:pr` alongside a contradicting
//! label.
//!
//! Slice 4 is [`head_sync`]: the self-sync head-SHA attribution guard
//! (#8164) — deciding when a 409 "Head branch was modified" was caused by
//! this run's OWN base-sync push, and may therefore be retried once against a
//! freshly-read head, versus a foreign push that must stay a hard stop.
//!
//! Slice 3 is [`stale_checks`]: the required-check freshness guard (#8248) —
//! refusing a merge whose green required-check results predate the base
//! branch's current tip, which is how a ratchet baseline tightened under an
//! in-flight PR red-lined main on 2026-09-18.
//!
//! [`redate`] is not a port — it is new functionality (#8508) closing the gap
//! #8248 left open: once that guard blocks a merge, nothing automatically
//! produces the fresh evidence it is waiting for when the merge token lacks
//! `actions:write` to re-run the stale check directly. It pushes a
//! tree-identical no-op commit instead, which re-triggers CI without
//! weakening the guard itself — bounded to one attempt per head, after which
//! the PR is escalated to a durable `loom:operator` hold rather than pushed
//! at forever.
//!
//! [`rerun`] (#8914) runs before [`redate`]: when the merge identity has
//! Actions: write, it re-runs the workflow runs holding the stale required
//! checks IN PLACE, so the head SHA — and the Judge verdict — survive. It
//! falls back to [`redate`]'s push only when the forge refuses the re-run.
//!
//! [`loom_pr_guard`] is the pre-merge `loom:pr` review-signal guard (#7419)
//! — the OTHER half of the verdict-label story [`labels`] tells: this one
//! fires on `loom:pr`'s ABSENCE ("nobody reviewed this head") rather than a
//! contradiction beside a present approval, and carries the only override
//! flag in the family (`--allow-unapproved`) because "nobody reviewed it" and
//! "a reviewer said no" are different acts.
//!
//! [`hold_state`] completes that trio: the advisory warning [`loom_pr_guard`]
//! deliberately left in the shell, fired only when `loom:pr` IS present and
//! Champion's recorded merge-risk-hold head is not the head about to merge. It
//! is the only member of the family that never refuses anything — and the port
//! fixes two ways the retired `grep | tail -1 | sed` pipeline lost the warning
//! silently, which for a check nothing else duplicates is the whole risk.

pub mod head_sync;
pub mod hold_state;
pub mod labels;
pub mod loom_pr_guard;
pub mod redate;
pub mod refs;
pub mod rerun;
pub mod stale_checks;
