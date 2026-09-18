//! The tree invariants behind epic #7810's shell budget (#8084).
//!
//! # What this file does and does not check
//!
//! The **growth** decision does not live here. `loom-daemon shell-budget
//! --check` owns it, in its own unconditional CI job, and it works by comparing
//! against the merge-base — "does THIS change add portable shell?" — so there
//! is no committed number to consult and nothing to regenerate.
//!
//! What lives here are the invariants that hold of any single tree and need no
//! `before` to judge, which a delta therefore cannot check:
//!
//! - **Every production script is accounted for** in `scripts/shell-allowlist.txt`.
//!   An unlisted one makes every figure an undercount, whichever revision you
//!   measure.
//! - **Something was counted at all.** The real bound on the production count
//!   lives in `shell_budget::measure`, against the allowlist's non-`test`
//!   entries — an independent signal, because a floor that used
//!   `is_production_shell` could not catch that predicate being wrong.
//! - **The progress denominator is pinned.** `origin_portable` is the portable
//!   figure immediately before the epic's first port commit. An origin that
//!   moves measures nothing, so a silent edit to it must fail here.
//!
//! # Why the growth check moved out
//!
//! It used to compare against a committed baseline number, which had to be
//! regenerated every time `main` moved. That chore was the defect, not its
//! upkeep cost: a standing instruction to regenerate teaches people to
//! regenerate without looking, which is the reflex a ratchet exists to prevent.
//! The same design red-lined `main` for 8 consecutive commits (#8073) and went
//! stale twice in one day here. `UPDATE_SHELL_BUDGET=1` no longer exists.
//!
//! # Why a category-aware number at all
//!
//! Total production shell cannot reach zero and was never meant to. `bootstrap`
//! (runs before a `loom-daemon` binary exists, so requiring the binary to
//! install itself is circular) and `vendored` (owned upstream) are correctly
//! permanent. Ratcheting "what we are retiring" plus "what we are keeping"
//! yields a figure with no target, so no run of it says how far along the epic
//! is. `portable` — `contract` + `hook-entry`, whose logic moves into the
//! daemon behind a stub — has one: the `stub` glue a ported file leaves behind.
//!
//! Total is still checked, as a delta rather than an absolute: growth in the
//! permanent floor raises the finish line, so it should be deliberate.
//!
//! # Why the existing gates miss all of this
//!
//! `check-file-size-budget.sh` ratchets INDIVIDUAL files already over 1,000
//! lines; `check-shell-allowlist.sh` wants a category and reason per NEW
//! script. Neither constrains aggregate volume, and the aggregate is where the
//! growth was: 29,795 of the lines added since the 60-day mark arrived as 148
//! brand-new scripts, nearly all under the per-file threshold and each with a
//! defensible reason. `check-file-size-budget.sh`'s own header names the
//! failure mode — *"every individual addition is defensible while the aggregate
//! is the problem."*

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

/// `origin_rev` is the same fixed point as a revision (#8154). Review found the
/// docs claiming "neither may be regenerated … A test pins it" while only
/// `origin_portable` was actually pinned — the "a comment describes a check
/// nobody wrote" defect this gate keeps producing. This is that check.
#[test]
fn the_progress_denominator_pins_its_revision_too() {
    let root = repo_root();
    assert_eq!(
        shell_budget::read_origin_rev(&root).as_deref(),
        Some("143fe332^"),
        "origin_rev anchors the scan for declared floor growth to the epic's start. Moving it \
         silently re-scopes every cumulative figure the report prints."
    );
}

/// It must also still RESOLVE. A pinned string that no longer names a reachable
/// commit makes the cumulative figure silently read zero.
#[test]
fn the_pinned_origin_rev_resolves_in_this_checkout() {
    let root = repo_root();
    let rev = shell_budget::read_origin_rev(&root).expect("origin_rev must be present");

    // A shallow clone genuinely cannot reach the epic's first commit, and the
    // report is built to degrade to omitting the figure there. I wrote that
    // caveat into this assertion's own failure message and then asserted
    // anyway; CI clones shallow, so it failed on the first run. Honour it.
    let shallow = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--is-shallow-repository"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false);
    if shallow {
        eprintln!(
            "skipped: a shallow clone cannot reach {rev}, which the report handles by omitting \
             the cumulative figure"
        );
        return;
    }

    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .output()
        .expect("git rev-parse must run");
    assert!(
        out.status.success(),
        "origin_rev {rev:?} does not resolve in this FULL checkout, so the anchor is wrong."
    );
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
