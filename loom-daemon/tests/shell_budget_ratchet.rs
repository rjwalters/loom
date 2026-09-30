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

/// #8237 constraint 1, against the REAL tree rather than a fixture.
///
/// The unit tests pin the arithmetic; this pins that the arithmetic is wired to
/// the manifest actually shipped. Un-settling the whole population — folding
/// every `settled` line back into `contract`, which is where each of them came
/// from — must leave `net vs epic start` exactly where it was. If it does not,
/// the category has been made to pay progress for an allowlist edit.
#[test]
fn settling_scripts_cannot_improve_the_epic_figure_on_the_real_tree() {
    let root = repo_root();
    let budget = shell_budget::measure(&root).expect("measure");
    let origin = shell_budget::read_origin_portable(&root).expect("origin_portable");

    let settled = budget.settled();
    assert!(
        settled > 0,
        "the manifest carries no `settled` entries, so this invariant is untested rather than \
         upheld — see .loom/docs/shell-language-policy.md"
    );

    // What the tree would have measured with the category never introduced.
    let mut unsettled = budget.clone();
    unsettled.by_category.remove(shell_budget::SETTLED);
    *unsettled
        .by_category
        .entry("contract".to_string())
        .or_default() += settled;

    assert_eq!(
        i128::from(budget.comparable()) - i128::from(origin),
        i128::from(unsettled.comparable()) - i128::from(origin),
        "reclassifying {settled} lines as `settled` changed the epic figure — descoping is not \
         retiring, and the report must not say otherwise"
    );
    assert_eq!(
        unsettled.portable(),
        budget.comparable(),
        "every settled script came out of the portable pool, so folding them back must \
         reproduce `comparable()` exactly"
    );
}

// --- #9297: the `Shell-Budget-Callout:` carve-out, through the real binary ---
//
// The unit tests in `shell_budget/{tests,callout/tests}.rs` pin the decision
// function. These pin that the decision is WIRED — that `loom-daemon
// shell-budget --check` collects the trailer, enumerates its own clap registry,
// measures the diff, and exits on the answer. Every one of those four steps
// lives outside `check_against_rev`, so no unit test can see them.

use std::path::Path;
use std::process::{Command, Output};

/// Run a git command in `dir`, with identity supplied per-invocation.
///
/// Never `git config --global`: the fleet shares this host, and a global write
/// from a test would outlive the test.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=shell-budget-test@example.invalid",
            "-c",
            "user.name=Shell Budget Test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} could not run: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const BASE_SCRIPT: &str = "#!/usr/bin/env bash\nset -euo pipefail\necho \"a\"\n";

/// A two-script repo on `main`, plus one commit on `feature` that appends
/// `added` to the `contract` script and carries `message`.
///
/// Returns the gate's own output for `--check --base main`.
fn run_gate(added: &str, message: &str) -> Output {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("scripts")).expect("mkdir");

    // Both floors in `measure` compare the allowlist's non-`test` entries
    // against what discovery found, so every script here must be listed.
    std::fs::write(
        root.join("scripts/shell-allowlist.txt"),
        "scripts/a.sh contract name-is-consumed-elsewhere\n\
         scripts/b.sh bootstrap runs-before-the-binary-exists\n",
    )
    .expect("write allowlist");
    std::fs::write(root.join("scripts/a.sh"), BASE_SCRIPT).expect("write a.sh");
    std::fs::write(root.join("scripts/b.sh"), BASE_SCRIPT).expect("write b.sh");

    git(root, &["init", "-b", "main"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "chore: base tree"]);
    git(root, &["checkout", "-b", "feature"]);

    std::fs::write(root.join("scripts/a.sh"), format!("{BASE_SCRIPT}{added}"))
        .expect("append to a.sh");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", message]);

    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["shell-budget", "--check", "--base", "main", "--root"])
        .arg(root)
        .output()
        .expect("the gate binary must run")
}

/// PR #8314's shape: the logic moved into a subcommand, and what stayed behind
/// finds the binary, calls it, and reads the result.
const CALL_SITE: &str = "\
if command -v loom-daemon >/dev/null 2>&1; then
    _out=\"$(loom-daemon shell-budget --json)\"
    if [[ -n \"$_out\" ]]; then
        printf '%s\\n' \"$_out\"
    fi
fi
";

/// Five lines of plain shell — the same GROWTH, none of it a call-site.
const NOT_A_CALL_SITE: &str = "_p=1\n_q=2\n_r=3\n_s=4\n_t=5\n";

#[test]
fn a_declared_call_site_passes_the_real_gate() {
    // `+6` against 5 measured lines also pins the `min(declared, measured)`
    // rule end to end: over-declaring is allowed and buys nothing.
    let out = run_gate(
        CALL_SITE,
        "feat: call the ported subcommand\n\nShell-Budget-Callout: shell-budget +6",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr:\n{stderr}\nstdout:\n{stdout}");
    assert!(
        stdout.contains("call-site lines GRANTED"),
        "a granted callout must never be silent:\n{stdout}"
    );
}

#[test]
fn the_same_lines_without_a_trailer_are_refused_by_the_real_gate() {
    let out = run_gate(CALL_SITE, "feat: call the ported subcommand");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must refuse:\n{stderr}");
    assert!(stderr.contains("PORTABLE"), "{stderr}");
    // …and the refusal must teach the trailer that would have worked.
    assert!(stderr.contains("Shell-Budget-Callout:"), "{stderr}");
}

#[test]
fn a_trailer_naming_a_subcommand_the_binary_does_not_have_is_refused() {
    // The check reads the running binary's own clap registry, so this cannot
    // be satisfied by a hand-kept list drifting out of date.
    let out = run_gate(
        CALL_SITE,
        "feat: call a subcommand that does not exist\n\n\
         Shell-Budget-Callout: no-such-subcommand +6",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must refuse:\n{stderr}");
    assert!(stderr.contains("not a `loom-daemon` subcommand"), "{stderr}");
}

#[test]
fn a_declaration_over_the_cap_is_refused_by_the_real_gate() {
    let out = run_gate(
        CALL_SITE,
        "feat: call the ported subcommand\n\nShell-Budget-Callout: shell-budget +41",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must refuse:\n{stderr}");
    assert!(stderr.contains("exceeds the per-subcommand cap"), "{stderr}");
}

#[test]
fn a_trailer_with_no_call_site_behind_it_is_refused_by_the_real_gate() {
    // Same line count, same trailer, no invocation. The trailer is a claim
    // about the diff, and the diff is what settles it.
    let out = run_gate(
        NOT_A_CALL_SITE,
        "feat: grow some shell\n\nShell-Budget-Callout: shell-budget +6",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must refuse:\n{stderr}");
    assert!(stderr.contains("no added line of PORTABLE shell"), "{stderr}");
}

#[test]
fn the_two_trailers_do_not_pay_each_others_bills() {
    // Non-interaction, end to end. This change does BOTH: it adds a genuine
    // call-site to the `contract` script (portable growth, the callout's
    // business) and grows the `bootstrap` script (floor growth, the growth
    // trailer's business). The callout must cover the first and NOT the second.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("scripts")).expect("mkdir");
    std::fs::write(
        root.join("scripts/shell-allowlist.txt"),
        "scripts/a.sh contract name-is-consumed-elsewhere\n\
         scripts/b.sh bootstrap runs-before-the-binary-exists\n",
    )
    .expect("write allowlist");
    std::fs::write(root.join("scripts/a.sh"), BASE_SCRIPT).expect("write a.sh");
    std::fs::write(root.join("scripts/b.sh"), BASE_SCRIPT).expect("write b.sh");
    git(root, &["init", "-b", "main"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "chore: base tree"]);
    git(root, &["checkout", "-b", "feature"]);
    std::fs::write(root.join("scripts/a.sh"), format!("{BASE_SCRIPT}{CALL_SITE}"))
        .expect("append to a.sh");
    std::fs::write(root.join("scripts/b.sh"), format!("{BASE_SCRIPT}{NOT_A_CALL_SITE}"))
        .expect("append to b.sh");
    git(root, &["add", "-A"]);

    let gate = |root: &Path| {
        Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
            .args(["shell-budget", "--check", "--base", "main", "--root"])
            .arg(root)
            .output()
            .expect("the gate binary must run")
    };

    // Callout alone: the call-site is admitted, the floor's five lines are not.
    git(
        root,
        &[
            "commit",
            "-m",
            "feat: port the logic and grow the floor\n\n\
             Shell-Budget-Callout: shell-budget +6",
        ],
    );
    let out = gate(root);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a callout must not pay for floor growth:\n{stderr}");
    assert!(stderr.contains("permanent floor"), "{stderr}");
    assert!(
        !stderr.contains("PORTABLE shell"),
        "the portable leg was covered by the callout and must not be the complaint:\n{stderr}"
    );

    // Both trailers: each pays its own bill, and the change lands.
    git(
        root,
        &[
            "commit",
            "--amend",
            "-m",
            "feat: port the logic and grow the floor\n\n\
             Shell-Budget-Callout: shell-budget +6\n\
             Shell-Budget-Growth: 5 lines — must stay shell (#9297)",
        ],
    );
    let out = gate(root);
    assert!(out.status.success(), "stderr:\n{}", String::from_utf8_lossy(&out.stderr));
}
