//! Differential test: the Rust port of `merge-pr.sh`'s `git worktree list
//! --porcelain` parsers must agree with the retired `awk`, byte for byte, on a
//! shared corpus (#8191 slice).
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** Each side generating its own inputs from a
//! shared seed looks equivalent and is not — an earlier differential in this
//! epic reimplemented a PRNG twice, they diverged on the second draw, and the
//! run reported a divergence in the code when the *inputs* had diverged.
//!
//! Here the corpus is a `&[&str]` in this file, written to disk once per case,
//! and the shell reads the same file. The harness cannot lie about which side
//! moved.
//!
//! # What it proves
//!
//! Not "the Rust looks right" — the unit tests beside the module do that. This
//! proves the port did not change *which worktree, and which branch, the script
//! acts on*. Every consumer of these three answers is irreversible: `git
//! worktree remove --force` on the wrong path, `git branch -D` on the wrong
//! branch, or the #3710 primary-checkout guard failing to fire.
//!
//! The shell side runs the real `awk` bodies, sourced from
//! `tests/fixtures/merge-pr-worktrees-retired.sh` — a frozen copy of them as
//! they stood immediately before the port. Each case also asserts how many
//! entries were actually compared, so a harness that silently stops running the
//! shell fails instead of passing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::worktrees;

/// Porcelain shapes chosen to hit the three documented incidents and the
/// boundaries where a record-oriented parse most plausibly diverges — not
/// random text, which mostly exercises "no match" on both sides.
const CORPUS: &[&str] = &[
    // --- the ordinary shape: trailing blank record after every stanza ---
    "worktree /repo/main\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /repo/wt-a\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/feature/issue-42\n\nworktree /repo/wt-b\nHEAD 3333333333333333333333333333333333333333\nbranch refs/heads/other\n\n",
    // --- #3671: match MID-list, so `exit` transfers control to `END` with the
    //     condition still true. This is the fixture that used to double-print.
    "worktree /repo/main\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /repo/wt-a\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/feature/issue-42\n\nworktree /repo/wt-b\nHEAD 3333333333333333333333333333333333333333\nbranch refs/heads/other\n",
    // --- #3671: match in the LAST stanza with NO terminating blank record —
    //     only the END arm can catch it.
    "worktree /repo/main\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /repo/wt-a\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/feature/issue-42",
    // --- #3717: spaces in paths (the `$2` truncation) ---
    "worktree /Users/x/My Repos/loom\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /Users/x/My Repos/loom/.loom/worktrees/issue-7\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/feature/issue-7\n\n",
    // --- a path that is ONLY spaces, and one with a trailing space ---
    "worktree /repo/a b \nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/spacey\n\n",
    // --- detached and bare entries (no `branch` record) ---
    "worktree /repo/main\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /repo/detached\nHEAD 2222222222222222222222222222222222222222\ndetached\n\nworktree /repo/bare\nbare\n\n",
    // --- a detached stanza AFTER a branch stanza: does `br` leak forward? ---
    "worktree /repo/wt-a\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/feature/issue-42\n\nworktree /repo/detached\nHEAD 2222222222222222222222222222222222222222\ndetached\n\n",
    // --- non-heads refs ---
    "worktree /repo/wt\nHEAD 1111111111111111111111111111111111111111\nbranch refs/remotes/origin/main\n\n",
    "worktree /repo/wt\nHEAD 1111111111111111111111111111111111111111\nbranch refs/tags/v1\n\n",
    // --- a branch whose short name contains `refs/heads/` again ---
    "worktree /repo/wt\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/refs/heads/weird\n\n",
    // --- extra blank records, leading blank record, no trailing newline ---
    "\n\nworktree /repo/main\nbranch refs/heads/main\n\n\n\n",
    "worktree /repo/main\nbranch refs/heads/main",
    // --- a `branch` record with padding, and one with nothing after it ---
    "worktree /repo/main\nbranch   refs/heads/padded\n\n",
    "worktree /repo/main\nbranch \n\n",
    // --- lines that only LOOK like records ---
    "worktrees /repo/main\nbranching refs/heads/main\n\n",
    "worktree\nbranch\n\n",
    "  worktree /repo/main\n  branch refs/heads/main\n\n",
    // --- CRLF: `awk` neither strips `\r` from the record nor treats it as a
    //     field separator, which a `lines()`/`split_ascii_whitespace()` port
    //     would do in both places.
    "worktree /repo/wt\r\nbranch refs/heads/main\r\n\r\n",
    // --- degenerate ---
    "",
    "\n",
    "fatal: not a git repository (or any of the parent directories): .git\n",
    // --- a realistic three-worktree Loom layout ---
    "worktree /Users/x/GitHub/loom\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\n\nworktree /Users/x/GitHub/loom/.loom/worktrees/issue-8191\nHEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nbranch refs/heads/feature/issue-8191\n\nworktree /Users/x/GitHub/loom/.loom/worktrees/pr-8429\nHEAD cccccccccccccccccccccccccccccccccccccccc\nbranch refs/heads/feature/issue-8429\n\n",
];

/// Every path and branch the corpus mentions, plus a few that appear in none of
/// it. Each query runs against every corpus entry, so the comparison covers
/// both the hits and the misses — a differential that only ever asks questions
/// with answers never exercises the `END`-with-no-match arm.
const PATH_QUERIES: &[&str] = &[
    "/repo/main",
    "/repo/wt",
    "/repo/wt-a",
    "/repo/wt-b",
    "/repo/detached",
    "/repo/bare",
    "/repo/a b ",
    "/repo/a",
    "/Users/x/My Repos/loom",
    "/Users/x/My",
    "/Users/x/My Repos/loom/.loom/worktrees/issue-7",
    "/Users/x/GitHub/loom",
    "/Users/x/GitHub/loom/.loom/worktrees/issue-8191",
    "/repo/nope",
    "",
    "/repo/wt\r",
];

const BRANCH_QUERIES: &[&str] = &[
    "main",
    "other",
    "feature/issue-42",
    "feature/issue-7",
    "feature/issue-8191",
    "feature/issue-8429",
    "spacey",
    "padded",
    "refs/heads/weird",
    "weird",
    "no/such/branch",
    "",
    "main\r",
];

/// The frozen copy of the retired `awk` bodies.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-worktrees-retired.sh")
}

/// Run one retired shell function with `input` on stdin and `arg` as `$1`.
///
/// Returns the raw stdout, so a doubled answer (#3671's actual symptom) shows
/// up as the two-line string it was rather than being normalised away by a
/// line-splitting harness.
fn shell(script: &Path, func: &str, input: &str, arg: Option<&str>) -> Option<String> {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(input.as_bytes()).expect("write corpus entry");

    let call = match arg {
        Some(_) => format!("{func} \"$3\" < \"$1\""),
        None => format!("{func} < \"$1\""),
    };
    let prog = format!(
        r#"
set -uo pipefail
source "$2"
{call}
"#
    );

    let out = Command::new("bash")
        // POSIX character classes and field splitting are locale-dependent, and
        // `C` is the locale the port models. An unpinned harness compares
        // against whatever the developer's shell happens to be set to and can
        // differ silently between a Mac and CI.
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(&prog)
        .arg("bash")
        .arg(tmp.path())
        .arg(script)
        .arg(arg.unwrap_or(""))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The port's answer in the shell's shape: `awk` prints a trailing newline and
/// nothing at all for "no match".
fn as_shell_output(answer: Option<&str>) -> String {
    answer.map_or_else(String::new, |a| format!("{a}\n"))
}

#[test]
fn primary_path_agrees_with_the_retired_awk() {
    let script = fixture();
    assert!(script.is_file(), "the frozen fixture must exist at {script:?}");

    let mut compared = 0usize;
    for (i, input) in CORPUS.iter().enumerate() {
        let Some(want) = shell(&script, "retired_primary_worktree_path", input, None) else {
            continue;
        };
        let got = as_shell_output(worktrees::primary_path(input));
        assert_eq!(
            got, want,
            "entry {i} diverged.\n  input: {input:?}\n  shell: {want:?}\n  rust:  {got:?}"
        );
        compared += 1;
    }
    assert_eq!(
        compared,
        CORPUS.len(),
        "only {compared}/{} entries were actually compared — the shell harness is not running",
        CORPUS.len()
    );
}

#[test]
fn branch_for_path_agrees_with_the_retired_awk() {
    let script = fixture();
    let mut compared = 0usize;
    for (i, input) in CORPUS.iter().enumerate() {
        for path in PATH_QUERIES {
            let Some(want) = shell(&script, "retired_worktree_branch_for", input, Some(path))
            else {
                continue;
            };
            let got = as_shell_output(worktrees::branch_for_path(input, path).as_deref());
            assert_eq!(
                got, want,
                "entry {i} query {path:?} diverged.\n  input: {input:?}\n  \
                 shell: {want:?}\n  rust:  {got:?}"
            );
            compared += 1;
        }
    }
    assert_eq!(
        compared,
        CORPUS.len() * PATH_QUERIES.len(),
        "only {compared} comparisons ran — the shell harness is not running"
    );
}

#[test]
fn find_by_branch_agrees_with_the_retired_awk() {
    let script = fixture();
    let mut compared = 0usize;
    for (i, input) in CORPUS.iter().enumerate() {
        for branch in BRANCH_QUERIES {
            let Some(want) = shell(&script, "retired_find_worktree_by_branch", input, Some(branch))
            else {
                continue;
            };
            let got = as_shell_output(worktrees::find_by_branch(input, branch));
            assert_eq!(
                got, want,
                "entry {i} query {branch:?} diverged.\n  input: {input:?}\n  \
                 shell: {want:?}\n  rust:  {got:?}"
            );
            compared += 1;
        }
    }
    assert_eq!(
        compared,
        CORPUS.len() * BRANCH_QUERIES.len(),
        "only {compared} comparisons ran — the shell harness is not running"
    );
}

/// The corpus must actually contain the cases it claims to. A differential
/// whose inputs all miss compares `""` with `""` everywhere and passes
/// regardless of what either side does — which is the "silently compared
/// nothing" failure this epic has hit before, one level up from the
/// `compared ==` counts above.
#[test]
fn the_corpus_produces_real_matches_on_both_sides() {
    let script = fixture();
    let mut hits = 0usize;
    for input in CORPUS {
        for branch in BRANCH_QUERIES {
            if worktrees::find_by_branch(input, branch).is_some() {
                hits += 1;
            }
        }
        for path in PATH_QUERIES {
            if worktrees::branch_for_path(input, path).is_some() {
                hits += 1;
            }
        }
    }
    assert!(
        hits > 20,
        "the corpus yields only {hits} matches — it is not exercising the hit path"
    );

    // And the shell agrees that they are matches, so "both sides answer
    // nothing" cannot be what the agreement above is made of.
    let midlist = CORPUS[1];
    let shell_hit =
        shell(&script, "retired_find_worktree_by_branch", midlist, Some("feature/issue-42"))
            .expect("the shell fixture must run");
    assert_eq!(shell_hit, "/repo/wt-a\n");
}

/// #3671 in its original form: the pre-fix `awk` doubles a mid-list match. Kept
/// as a sanity check on the corpus, exactly as the retained shell suite's
/// `Test 4` did — if this ever stops doubling, entry 1 no longer exercises the
/// `exit`-triggers-`END` path and the agreement above is about nothing.
#[test]
fn the_pre_fix_awk_still_doubles_on_the_corpus_entry_that_pins_it() {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(CORPUS[1].as_bytes()).expect("write");

    let prog = r#"
set -uo pipefail
awk -v want="refs/heads/feature/issue-42" '
  /^worktree / { wt=substr($0, 10); br=""; next }
  /^branch /   { br=$2 }
  /^$/         { if (br == want) { print wt; exit } }
  END          { if (br == want) { print wt } }
' < "$1"
"#;
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(prog)
        .arg("bash")
        .arg(tmp.path())
        .output()
        .expect("bash");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "/repo/wt-a\n/repo/wt-a\n",
        "the un-guarded awk no longer doubles — corpus entry 1 has stopped covering #3671"
    );

    // The port cannot express that shape at all: one answer, or none.
    assert_eq!(worktrees::find_by_branch(CORPUS[1], "feature/issue-42"), Some("/repo/wt-a"));
}
