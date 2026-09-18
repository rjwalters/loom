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

pub mod refs;
