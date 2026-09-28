//! Differential test: the Rust port of `merge-pr.sh`'s head-mismatch
//! classifier must agree with the shell on a shared corpus.
//!
//! # What it proved then, and what it proves now
//!
//! Slice 4 of #8191 did **not** delete `_is_head_mismatch_response`: the shell
//! kept a cheap local copy to decide whether to consult the daemon at all,
//! while the daemon needed its own so a caller could not authorize a retry by
//! mislabelling an arbitrary error as a head mismatch. Two copies of one
//! predicate is a drift hazard, and this test converted it into a CI failure
//! instead — the moment either side gained a pattern the other lacked, the run
//! went red.
//!
//! **#8191's classify-response slice ended that arrangement.** The shell copy
//! is gone, and the two Rust copies were converged onto one definition:
//! `merge_pr::response::is_head_mismatch`, which
//! `merge_pr::head_sync::is_head_mismatch` now delegates to. The drift this
//! test policed is therefore unrepresentable rather than merely detected, and
//! the test is NOT retired, because what survives is the stronger half of what
//! it was always doing: pinning the surviving predicate against the **retired
//! `grep`** it replaced.
//!
//! That is the obligation this header used to hand forward ("the slice that
//! finally deletes it inherits that obligation, and this header is where to
//! look"). It is discharged here: the shell side now sources the frozen copy
//! at `tests/fixtures/merge-pr-response-retired.sh` instead of extracting from
//! the live `merge-pr.sh`. Reading the live script would now compare the port
//! against itself — a differential that measures nothing, which is exactly the
//! failure `defaults/docs/verification-recipes.md` §6 names.
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! The corpus is generated ONCE, in this file, and written to disk; the shell
//! reads the same bytes. An earlier differential in this epic had each side
//! generate its own inputs and reported a divergence in the code when the
//! *inputs* had diverged — nothing about the code was measured.
//!
//! # What the corpus is built from
//!
//! The grammar the predicate parses: each of the three forge strings, the
//! case variations `grep -Ei` accepts, the near-misses each pattern's regex
//! metacharacters make possible (the escaped `.` in particular), and the
//! sibling string that must NOT match — `Base branch was modified`, whose
//! conflation with this one is the documented reason the two matchers are
//! separate.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use loom_daemon::merge_pr::head_sync::is_head_mismatch;

/// One case per line. Newlines are the record separator on the shell side, so
/// no case may contain one — the multi-line JSON blobs a forge returns are
/// exercised by the unit tests, which have no such constraint.
const CORPUS: &[&str] = &[
    // --- the three verified forge strings, as they actually arrive ---
    "Error: Head branch was modified. Review and try the merge again. (HTTP 409)",
    "PR #8164: {\"message\":\"Head branch was modified. Review and try the merge again.\",\"status\":\"409\"}",
    "{\"message\":\"head out of date\",\"url\":\"https://gitea.example.com/api/v1/...\"}",
    "could not enable auto-merge: expectedHeadOid does not match current head",
    // --- case, which `grep -Ei` ignores ---
    "HEAD BRANCH WAS MODIFIED.",
    "head branch was modified.",
    "Head Branch Was Modified.",
    "HEAD OUT OF DATE",
    "Head Out Of Date",
    "EXPECTEDHEADOID",
    "expectedheadoid",
    "ExpectedHeadOid",
    // --- the escaped dot, and what it excludes ---
    "Head branch was modified.",
    "Head branch was modified",
    "Head branch was modified by a later push",
    "the head branch was modified, so we re-queued",
    "Head branch was modifiedX",
    // --- the sibling that must never match ---
    "Error: Base branch was modified. Review and try the merge again. (HTTP 409)",
    "Base branch was modified.",
    "base branch was modified",
    // --- other merge-loop responses that reach the same call site ---
    "Merge already in progress",
    "Pull request Pull request is in clean status (enablePullRequestAutoMerge)",
    "Pull request Pull request is in unstable status (enablePullRequestAutoMerge)",
    "Pull Request is not mergeable",
    "GraphQL: Protected branch rule violations found",
    "gh: Not Found (HTTP 404)",
    // --- near-misses around each keyword ---
    "outofdate",
    "out of date",
    "the head is out of date",
    "head_out_of_date",
    "expected head oid",
    "expectedHeadOID mismatch",
    "unexpectedHeadOid",
    "headbranchwasmodified.",
    // --- substring-vs-word-boundary probes: the shell uses no \\b here, so
    //     an embedded match DOES fire on both sides. Pinned deliberately.
    "xxHead branch was modified.xx",
    "prefix-expectedHeadOid-suffix",
    // --- degenerate ---
    "",
    " ",
    "\t",
    "409",
    "{}",
    "null",
    // --- quoting hazards for the shell side (single quotes, backticks, $) ---
    "it's the head branch was modified. case",
    "`Head branch was modified.`",
    "$HEAD branch was modified.",
    "\"head out of date\"",
    "100% head out of date",
    "a\\backslash head out of date",
];

/// Run the RETIRED `_is_head_mismatch_response` over every corpus line,
/// returning one `y`/`n` per line.
///
/// Sourced from the frozen copy, not the live `merge-pr.sh`: since #8191's
/// classify-response slice the live script has no such function, and reading
/// whatever replaced it would compare the port against itself. The frozen file
/// is the only form of this oracle that keeps saying something true now that
/// the shell is gone.
fn shell_answers(corpus_path: &std::path::Path) -> Vec<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/merge-pr-response-retired.sh");
    assert!(script.exists(), "frozen fixture not found at {script:?}");

    // Still guarded rather than a bare `.`: a fixture whose function was
    // renamed must fail loudly instead of silently comparing nothing.
    let program = r#"set -euo pipefail
. "$1"
declare -F _is_head_mismatch_response >/dev/null || { echo "FIXTURE MISSING _is_head_mismatch_response" >&2; exit 2; }
while IFS= read -r line; do
  if _is_head_mismatch_response "$line"; then echo y; else echo n; fi
done < "$2"
"#;

    let out = Command::new("bash")
        // `grep -i` folds per the locale; the port folds ASCII-only, which is
        // what `C` means. Unpinned, this harness compares against whatever the
        // developer's shell is set to and can differ between a Mac and CI —
        // verification-recipes.md §6, Cause 3's last row.
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(program)
        .arg("bash")
        .arg(&script)
        .arg(corpus_path)
        .output()
        .expect("could not run bash");

    assert!(
        out.status.success(),
        "shell side failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn the_rust_predicate_agrees_with_the_shell_on_every_case() {
    // One corpus file, read by both sides — the harness cannot lie about
    // which side moved.
    let path = std::env::temp_dir().join(format!("loom-head-sync-corpus-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&path).expect("corpus file");
        for case in CORPUS {
            assert!(!case.contains('\n'), "corpus lines are newline-separated on the shell side");
            writeln!(f, "{case}").expect("write corpus");
        }
    }

    let shell = shell_answers(&path);
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        shell.len(),
        CORPUS.len(),
        "the shell must answer for every case — a short read is a harness bug, \
         and it is exactly how a differential quietly compares nothing"
    );

    let mut divergences = Vec::new();
    for (case, shell_says) in CORPUS.iter().zip(shell.iter()) {
        let rust_says = if is_head_mismatch(case) { "y" } else { "n" };
        if rust_says != shell_says {
            divergences.push(format!("{case:?}: shell={shell_says} rust={rust_says}"));
        }
    }
    assert!(
        divergences.is_empty(),
        "the port disagrees with the shell on {} case(s):\n{}",
        divergences.len(),
        divergences.join("\n")
    );

    // Discriminating power, not just size: a corpus that matched nothing
    // (or everything) would agree trivially. Both answers must be present,
    // and in numbers that make an all-constant implementation impossible.
    let yes = shell.iter().filter(|a| *a == "y").count();
    let no = shell.len() - yes;
    assert!(
        yes >= 10 && no >= 10,
        "corpus lost its discriminating power: {yes} matches, {no} non-matches"
    );
}
