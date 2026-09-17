//! Aggregate ratchet on the shell epic #7810 is actually retiring (#8084).
//!
//! # Why
//!
//! Four scripts have been ported and roughly 2,200 lines of production shell
//! deleted. Over the same window the pool those ports are draining went **up**:
//! 38,374 lines immediately before the epic's first port commit, 38,439 now.
//! A net **+65**. More portable shell arrived than left, and nothing measured
//! it, so four merged ports read as progress.
//!
//! # Why `portable` and not the total
//!
//! The first cut of this gate ratcheted total production shell — 51,814 lines.
//! That number cannot reach zero and was never meant to: `bootstrap` (runs
//! before a `loom-daemon` binary exists, so requiring the binary to install
//! itself is circular) and `vendored` (owned upstream in rjwalters/repo) are
//! 13,230 lines of correctly permanent shell. Ratcheting "what we are retiring"
//! plus "what we are keeping" yields a figure with no target, so no run of it
//! ever says how far along the epic is.
//!
//! `portable` — `contract` + `hook-entry`, whose logic moves into the daemon
//! behind a stub — has a target: the ~145 lines of `stub` glue a ported file
//! leaves behind. `total` stays ratcheted so floor growth is visible rather
//! than free, but the floor is not debt.
//!
//! # Why the existing gates miss all of this
//!
//! `check-file-size-budget.sh` ratchets INDIVIDUAL files already over 1,000
//! lines; `check-shell-allowlist.sh` wants a category and reason per NEW
//! script. Neither constrains aggregate volume, and the aggregate is where the
//! growth is: 29,795 of the lines added since the 60-day mark arrived as 148
//! brand-new scripts, nearly all under the per-file threshold and each with a
//! defensible reason. `check-file-size-budget.sh`'s own header names the
//! failure mode — *"every individual addition is defensible while the aggregate
//! is the problem."*
//!
//! # What this is not
//!
//! Not a ban. Growth stays possible; it just has to be spelled out as a changed
//! number in `scripts/shell-budget-baseline.txt`, visible in the diff, instead
//! of arriving invisibly across a hundred files.
//!
//! # Where this actually gates
//!
//! CI runs `loom-daemon shell-budget --check` in its own unconditional job, not
//! this test. `Rust Unit Tests` is gated on the `backend` paths filter, which
//! covers `loom-daemon/**` but NOT `scripts/**` or `defaults/**` — so a PR that
//! adds a shell script would have skipped the job entirely and the ratchet would
//! have first fired on the push to `main`. A gate that is skipped on exactly the
//! changes it targets is not a gate. Every sibling structural ratchet (File
//! Size, Shell Allowlist, Markdown Token, Role Prompt) runs unconditionally for
//! the same reason.
//!
//! This test keeps the same gate honest locally and under `cargo test`; both
//! call [`shell_budget::check`], so they cannot disagree about what passes.
//!
//! # There is nothing to update any more
//!
//! This used to compare against a committed number, and that number had to be
//! regenerated every time `main` moved. That chore was the defect, not the
//! upkeep cost of it: a standing instruction to regenerate teaches people to
//! regenerate without looking, which is the reflex a ratchet exists to prevent.
//! It went stale twice in one day, and the same design red-lined `main` for 8
//! commits via #8073.
//!
//! The growth decision now lives in `loom-daemon shell-budget --check`, which
//! compares against the MERGE-BASE — "does THIS change add portable shell?" —
//! and consults no committed number at all.
//!
//! What remains here are the invariants that hold of any single tree and need
//! no `before` to judge: every production script is accounted for in the
//! allowlist, and the scope filter still sees the tree. Those cannot be checked
//! by a delta, and they are how a gate comes to pass for the wrong reason.
//!
//! # Historical
//!
//! ```text
//! UPDATE_SHELL_BUDGET=1 cargo test -p loom-daemon --test shell_budget_ratchet
//! ```
//!
//! Legitimate for recording shrinkage, or for a reviewed decision to admit
//! growth. A reviewer should treat an update that RAISES `portable` as the
//! thing to ask about. `origin_portable` is never regenerated — an origin that
//! moves measures nothing.

use std::path::PathBuf;

use loom_daemon::shell_budget;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon must have a parent directory")
        .to_path_buf()
}

#[test]
fn every_production_script_is_accounted_for() {
    let root = repo_root();
    let budget = shell_budget::measure(&root).expect("measure");
    let origin = shell_budget::read_origin_portable(&root)
        .expect("scripts/shell-budget-baseline.txt must carry origin_portable");

    // Always print it. A gate that only speaks up on failure teaches nobody
    // which way the number is moving, which is how the portable pool grew +317
    // across four merged ports unnoticed.
    println!("{}", shell_budget::render_report(&budget, origin));

    if let Err(why) = shell_budget::check_invariants(&budget) {
        panic!("{why}\n\n{}", shell_budget::render_report(&budget, origin));
    }
}

/// The denominator must never be regenerated. A moving origin measures nothing,
/// so this pins the one value in that file against a silent edit.
#[test]
fn the_progress_denominator_is_the_pre_epic_figure() {
    let root = repo_root();
    assert_eq!(
        shell_budget::read_origin_portable(&root),
        Some(38_374),
        "origin_portable is the portable-shell figure immediately before epic #7810's first \
         port commit (143fe332^). Changing it re-bases every progress number ever reported \
         and is almost certainly a mistake — if the epic's start really is being redefined, \
         say so in the commit."
    );
}
