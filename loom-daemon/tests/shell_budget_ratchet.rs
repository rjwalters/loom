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
//! # Updating
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

fn baseline_path() -> PathBuf {
    repo_root().join("scripts/shell-budget-baseline.txt")
}

#[test]
fn portable_shell_does_not_grow() {
    let root = repo_root();
    let budget = shell_budget::measure(&root).expect("measure");

    if std::env::var_os("UPDATE_SHELL_BUDGET").is_some() {
        let origin = shell_budget::read_origin_portable(&root)
            .expect("origin_portable must already exist — it is never regenerated");
        // Refuse to bank an undercount: an unlisted script makes every figure
        // below wrong, and writing it as the new baseline would freeze the
        // error in place.
        assert!(
            budget.unlisted.is_empty(),
            "refusing to regenerate while {} production script(s) have no allowlist entry:\n{}",
            budget.unlisted.len(),
            budget
                .unlisted
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let text = std::fs::read_to_string(baseline_path()).expect("read baseline");
        let header: String = text
            .lines()
            .take_while(|l| l.trim_start().starts_with('#') || l.trim().is_empty())
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(
            baseline_path(),
            format!(
                "{header}portable {}\ntotal {}\nfiles {}\norigin_portable {origin}\n",
                budget.portable(),
                budget.total(),
                budget.file_count()
            ),
        )
        .expect("write baseline");
        eprintln!("{}", shell_budget::render_report(&budget, origin));
        return;
    }

    let base = shell_budget::read_baseline(&root).expect("read baseline");
    println!("{}", shell_budget::render_report(&budget, base.origin_portable));

    if let Err(why) = shell_budget::check(&budget, &base) {
        panic!("{why}\n\n{}", shell_budget::render_report(&budget, base.origin_portable));
    }
}
