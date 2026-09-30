//! Differential test: the Rust port of `merge-pr.sh`'s two post-merge
//! partial-increment audit comments must render the same bytes the retired
//! shell did (#8191 slice, #3667 / #4569).
//!
//! # Why a differential for text
//!
//! Because these two bodies are the only thing in the family whose whole value
//! IS the bytes. [`loom_daemon::merge_pr::partial_comment`]'s unit tests can
//! say the conditional bullet lands in the right place and the sign-off names
//! the right issue; they cannot say the paragraph an operator reads is the
//! paragraph that was there before the port. Nothing else can either: the
//! retained suite stubs the comment post, and
//! `merge_pr_partial_reset_differential.rs` stubs `forge_gh_comment_rl_safe`
//! to `:` so the decision under test is not drowned in prose. Before this
//! file, both bodies were unasserted text in a 3,000-line shell script.
//!
//! # The two oracles, and why they are different files
//!
//! * `tests/fixtures/merge-pr-partial-reset-retired.sh` — already on `main`,
//!   frozen by the partial-reset slice, and it happens to carry the
//!   `## Partial Increment Merged` body inline because it froze
//!   `_reset_one_partial_issue` WHOLE. So this file drives that fixture rather
//!   than copying its text: [`HARNESS_PARTIAL_MERGED`] differs from that
//!   differential's harness in exactly one line — a RECORDING
//!   `forge_gh_comment_rl_safe` where it has a silent one.
//! * `tests/fixtures/merge-pr-partial-comment-retired.sh` — new here, because
//!   `_post_premature_close_comment` was frozen nowhere: the partial-reset
//!   fixture calls it and that harness defines it as `{ :; }`.
//!
//! Two frozen copies of one text can drift apart; one cannot. That is the
//! whole reason the first bullet reuses rather than re-freezes.
//!
//! # Rules from `defaults/docs/verification-recipes.md` §6 this obeys
//!
//! - **Generate the corpus ONCE and feed both sides the same bytes.**
//!   [`NUMBERS`] and [`TIMESTAMPS`] are deterministic literals; every case is
//!   handed to `bash` as the same `&str` the Rust receives.
//! - **Generate the ALPHABET, not a sample.** The number corpus is not a list
//!   of plausible issue numbers — `merge-pr.sh` only ever passes `[0-9]+`. It
//!   is the list of ways a *substitution* can go wrong: empty, embedded
//!   newline, backtick, `$`, `%`, `{}`, backslash, leading/trailing space,
//!   non-ASCII. Those are the inputs that separate "both sides substitute" from
//!   "both sides happen to agree on digits".
//! - **Say WHICH implementation a copied pattern models.** Nothing is
//!   transcribed here; both fixtures state verbatim-vs-reconstructed in their
//!   own headers.
//! - **Pin `LC_ALL=C`.** The bodies contain em dashes, a horizontal ellipsis
//!   and curly quotes. An unpinned harness would compare through the
//!   developer's ambient locale.
//!
//! # The one divergence, named up front
//!
//! NUL bytes. The retired implementation carried both bodies through shell
//! *variables* and built them from `$1` / `$PR_NUMBER`, which cannot hold a
//! NUL, so no differential evidence about NUL is obtainable here. The port
//! takes `&str`, which cannot hold one either without the caller having put it
//! there; `merge-pr.sh` passes clap arguments, and an argv element cannot
//! contain a NUL on either side. There is nothing to diverge.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::partial_comment::{partial_merged_comment, premature_close_comment};

/// The substitution alphabet. Not plausible issue numbers — `merge-pr.sh`
/// matched `[0-9]+` upstream — but the shapes that tell a real substitution
/// apart from a coincidence, in both directions:
///
/// * things bash re-expands if the body is ever re-evaluated (`$`, backtick),
/// * things `printf`/`format!` treat specially if either side ever routes the
///   value through a format string (`%`, `{`, `}`),
/// * things a naive trim would eat (leading/trailing space, newline),
/// * things a byte-oriented vs char-oriented render disagree on (non-ASCII),
/// * and the empty string, which is the shape a caller bug produces.
const NUMBERS: &[&str] = &[
    "1",
    "999",
    "0",
    "007",
    "18446744073709551616",
    "",
    "4242",
    "#5",
    "a`b",
    "x$y",
    "p%q",
    "{z}",
    "back\\slash",
    "  spaced  ",
    "line1\nline2",
    "é—ü",
    "issue-with-dash",
];

/// Sign-off instants. The retired shell read `date -u +%Y-%m-%dT%H:%M:%SZ`;
/// the harness stubs `date`, so this corpus also covers what happens when the
/// clock answers something unexpected.
const TIMESTAMPS: &[&str] = &[
    "2026-09-30T12:00:00Z",
    "",
    "not a date",
    "1970-01-01T00:00:00Z",
];

/// The case list, built ONCE and fed to both sides.
///
/// Deliberately not the full `NUMBERS × NUMBERS × TIMESTAMPS` cross product:
/// each case costs a `bash` process (and, on the partial-merged side, a `jq`
/// too), and the product buys nothing here. Both implementations substitute
/// each parameter INDEPENDENTLY — there is no code path in either where the PR
/// number changes how the issue number renders — so the alphabet is swept once
/// per position, against a fixed partner, plus a handful of cases where two
/// adversarial values meet to falsify that independence claim rather than
/// assume it.
fn cases() -> Vec<(&'static str, &'static str, &'static str)> {
    let mut out: Vec<(&str, &str, &str)> = Vec::new();
    let ts0 = TIMESTAMPS[0];
    for n in NUMBERS {
        out.push((n, "999", ts0));
        out.push(("4242", n, ts0));
    }
    for ts in TIMESTAMPS {
        out.push(("4242", "999", ts));
    }
    // Independence falsifiers: two adversarial values in the same render.
    out.push(("line1\nline2", "a`b", ""));
    out.push(("", "", ""));
    out.push(("p%q", "{z}", "not a date"));
    out.push(("  spaced  ", "é—ü", "1970-01-01T00:00:00Z"));
    out
}

/// The frozen copy of `_post_premature_close_comment`.
fn premature_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-partial-comment-retired.sh")
}

/// The already-frozen copy of `_reset_one_partial_issue`, whose SWAP arm
/// builds the `## Partial Increment Merged` body inline.
fn partial_reset_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-partial-reset-retired.sh")
}

/// Recording stubs + the frozen `_post_premature_close_comment`.
///
/// `forge_gh_comment_rl_safe` echoes its THIRD argument — the `$comment`
/// local — with `printf '%s'`, which appends nothing, so the transcript is the
/// body and only the body. `date` is stubbed so the sign-off is deterministic
/// and the corpus, not the clock, decides it.
///
/// `$1` = fixture, `$2` = issue number, `$3` = PR number, `$4` = timestamp.
const HARNESS_PREMATURE: &str = r#"set -euo pipefail
FIXTURE="$1"; ISSUE="$2"; PR="$3"; TS="$4"
date() { printf '%s\n' "$TS"; }
warning() { :; }
forge_gh_comment_rl_safe() { printf '%s' "$3"; }
REPO_NWO="owner/repo"; PR_NUMBER="$PR"
source "$FIXTURE"
_post_premature_close_comment "$ISSUE"
"#;

/// Recording stubs + the frozen `_reset_one_partial_issue`, driven to its SWAP
/// arm so the `## Partial Increment Merged` body is built and posted.
///
/// This is `merge_pr_partial_reset_differential.rs`'s harness with ONE line
/// changed: its `forge_gh_comment_rl_safe() { :; }` becomes a recording stub.
/// Everything else — including the `|| true` the production call site wraps
/// the pass in, which is what keeps `errexit` disabled inside the function
/// body exactly as production had it — is kept so the frozen function runs
/// under the conditions it ran under.
///
/// `$1` = fixture, `$2` = issue number, `$3` = PR number, `$4` = timestamp,
/// `$5` = the `gh api` response body, `$6` = the pre-merge conflicted flag.
const HARNESS_PARTIAL_MERGED: &str = r#"set -euo pipefail
FIXTURE="$1"; ISSUE="$2"; PR="$3"; TS="$4"; BODY="$5"; CONFLICTED="$6"
gh() { printf '%s' "$BODY"; }
date() { printf '%s\n' "$TS"; }
info() { :; }
warning() { :; }
success() { :; }
forge_gh_reopen_issue_rl_safe() { :; }
forge_gh_swap_label_rl_safe() { :; }
forge_gh_comment_rl_safe() { printf '%s' "$3"; }
_post_premature_close_comment() { :; }
_partial_ref_is_conflicted() { [[ "$CONFLICTED" == 1 ]]; }
_partial_ref_was_open_before_merge() { return 1; }
REPO_NWO="owner/repo"; PR_NUMBER="$PR"
source "$FIXTURE"
_reset_one_partial_issue "$ISSUE" || true
"#;

/// An OPEN issue still carrying `loom:building`: the retired SWAP arm with
/// `reopened=false`.
const OPEN_BUILDING: &str = r#"{"state":"open","labels":[{"name":"loom:building"}]}"#;
/// A CLOSED issue the pre-merge guard attributed to this PR: the retired
/// REOPEN-then-SWAP path, i.e. `reopened=true`.
const CLOSED_BUILDING: &str = r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#;

/// How many times one query may be re-issued after an *environmental* fault
/// before the harness gives up and fails loudly. Mirrors
/// `merge_pr_partial_reset_differential.rs`.
const ORACLE_ATTEMPTS: usize = 4;

/// Did bash fail to create a process, rather than the frozen function
/// producing a body? See `merge_pr_partial_reset_differential.rs` for the full
/// argument; `LC_ALL=C` is pinned below so these strings are not localized.
fn is_process_creation_failure(stderr: &str) -> bool {
    stderr.contains(": fork:")
        || stderr.contains("Resource temporarily unavailable")
        || stderr.contains("Cannot allocate memory")
}

/// One raw run of a harness. `Ok` is a body; `Err` says this invocation did
/// not answer at all.
fn run_once(harness: &str, args: &[String]) -> Result<String, String> {
    let mut cmd = Command::new("bash");
    cmd.env("LC_ALL", "C").arg("-c").arg(harness).arg("bash");
    for a in args {
        cmd.arg(a);
    }
    let out = match cmd.output() {
        Ok(out) => out,
        Err(e) => return Err(format!("could not run the frozen retired function: {e}")),
    };
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(format!(
            "exited {:?} under `set -euo pipefail`; stderr: {stderr}",
            out.status.code()
        ));
    }
    if is_process_creation_failure(&stderr) {
        return Err(format!(
            "a command inside the frozen function could not be created; stderr: {stderr}"
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The frozen shell's body, from an invocation that actually ran.
///
/// A deterministic failure — the frozen function genuinely aborting — fails
/// every attempt and still panics, as it should.
fn shell_body(harness: &str, args: &[String]) -> String {
    let mut faults: Vec<String> = Vec::new();
    for attempt in 0..ORACLE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(150 * attempt as u64));
        }
        match run_once(harness, args) {
            Ok(body) => return body,
            Err(why) => faults.push(format!("attempt {}: {why}", attempt + 1)),
        }
    }
    panic!(
        "the frozen retired function never answered for args {args:?}:\n  {}",
        faults.join("\n  ")
    );
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// The `## Premature Auto-Close Reverted` body, over the full substitution
/// alphabet × every sign-off instant.
#[test]
fn the_premature_close_body_matches_the_retired_shell_byte_for_byte() {
    let fixture = premature_fixture().to_string_lossy().into_owned();
    let corpus = cases();
    let mut checked = 0usize;
    for (issue, pr, ts) in &corpus {
        let shell = shell_body(HARNESS_PREMATURE, &args(&[&fixture, issue, pr, ts]));
        let rust = premature_close_comment(issue, pr, ts);
        assert_eq!(
            rust, shell,
            "divergence for issue={issue:?} pr={pr:?} ts={ts:?}\n\
             --- rust ---\n{rust}\n--- shell ---\n{shell}\n"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        corpus.len(),
        "the corpus shrank — a `continue` or an early return is skipping cases"
    );
    assert!(corpus.len() >= NUMBERS.len(), "the alphabet was not swept");
}

/// The `## Partial Increment Merged` body, in BOTH arms of its one
/// conditional, driven through the already-frozen `_reset_one_partial_issue`.
///
/// The issue number is varied too even though the body does not name it: the
/// point is to prove it does not *start* naming it, which is exactly the kind
/// of accidental text change a port invites.
#[test]
fn the_partial_merged_body_matches_the_retired_shell_byte_for_byte() {
    let fixture = partial_reset_fixture().to_string_lossy().into_owned();
    let corpus = cases();
    let mut checked = 0usize;
    for (body, conflicted, reopened) in [(OPEN_BUILDING, "0", false), (CLOSED_BUILDING, "1", true)]
    {
        for (issue, pr, ts) in &corpus {
            let shell = shell_body(
                HARNESS_PARTIAL_MERGED,
                &args(&[&fixture, issue, pr, ts, body, conflicted]),
            );
            let rust = partial_merged_comment(pr, reopened, ts);
            assert_eq!(
                rust, shell,
                "divergence for reopened={reopened} issue={issue:?} pr={pr:?} ts={ts:?}\n\
                 --- rust ---\n{rust}\n--- shell ---\n{shell}\n"
            );
            checked += 1;
        }
    }
    assert_eq!(
        checked,
        2 * corpus.len(),
        "the corpus shrank — a `continue` or an early return is skipping cases"
    );
}

/// The harness must actually reach the two arms it claims to. A fixture whose
/// SWAP arm stopped being reachable would make the test above compare an empty
/// string to an empty string forever — §6's "measured nothing" failure.
#[test]
fn both_arms_of_the_partial_merged_conditional_are_actually_reached() {
    let fixture = partial_reset_fixture().to_string_lossy().into_owned();
    let not_reopened =
        shell_body(HARNESS_PARTIAL_MERGED, &args(&[&fixture, "1", "2", "TS", OPEN_BUILDING, "0"]));
    let reopened = shell_body(
        HARNESS_PARTIAL_MERGED,
        &args(&[&fixture, "1", "2", "TS", CLOSED_BUILDING, "1"]),
    );
    assert!(
        not_reopened.starts_with("## Partial Increment Merged"),
        "the open/loom:building case must reach the SWAP arm, got: {not_reopened:?}"
    );
    assert!(
        !not_reopened.contains("**Reopened**"),
        "…without the reopen bullet: {not_reopened}"
    );
    assert!(
        reopened.contains("- **Reopened** this issue"),
        "the closed+conflicted case must reach the SWAP arm WITH the reopen bullet, got: {reopened:?}"
    );
}

/// The retired `comment="…"` ended at its closing quote. This is asserted
/// against the SHELL, not just the Rust, because it is the one property a
/// `$(...)`-based comparison would have hidden: command substitution strips
/// trailing newlines, so a port that added one would have looked identical
/// through every capture in the script.
#[test]
fn neither_retired_body_ended_with_a_newline() {
    let premature = shell_body(
        HARNESS_PREMATURE,
        &args(&[
            &premature_fixture().to_string_lossy(),
            "1",
            "2",
            "2026-09-30T12:00:00Z",
        ]),
    );
    let merged = shell_body(
        HARNESS_PARTIAL_MERGED,
        &args(&[
            &partial_reset_fixture().to_string_lossy(),
            "1",
            "2",
            "TS",
            OPEN_BUILDING,
            "0",
        ]),
    );
    for (name, body) in [("premature-close", &premature), ("partial-merged", &merged)] {
        assert!(!body.is_empty(), "{name}: the recording stub captured nothing");
        assert!(
            !body.ends_with('\n'),
            "{name}: the retired body ended at its closing quote, not with a newline"
        );
    }
}

/// The fixtures must be the frozen copies this file names, not something that
/// grew a `loom-daemon` call and now compares the port against itself — §6's
/// "measured nothing" failure in its other form.
#[test]
fn neither_fixture_delegates_back_to_the_port() {
    for path in [premature_fixture(), partial_reset_fixture()] {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        // Comment lines are excluded on purpose: both headers *name*
        // loom-daemon paths while explaining what they are frozen against.
        // What must not appear is an executable reference to it.
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("loom-daemon"),
            "{} executes loom-daemon — a frozen oracle must not call the thing it is the oracle for",
            path.display()
        );
        assert!(text.contains("FROZEN COPY"), "{} lost its FROZEN COPY header", path.display());
    }
}

/// `merge-pr.sh` must still be able to obtain both bodies. The wrapper that
/// replaced the inline text is asserted by
/// `defaults/scripts/tests/test-merge-pr-partial-increment.sh`; what is
/// checked here is the half that lives on this side of the seam — that the
/// live script no longer carries either body, so there is exactly one place
/// the text can be edited.
#[test]
fn the_live_script_no_longer_carries_either_body() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/merge-pr.sh");
    let Ok(text) = std::fs::read_to_string(&script) else {
        // Consumer repos vendor `.loom/scripts` without `defaults/`; nothing
        // to check there, and a missing file is not a failing port.
        return;
    };
    for marker in [
        "This issue is now available for the next increment",
        "GitHub honors a closing keyword",
    ] {
        assert!(
            !text.contains(marker),
            "merge-pr.sh still contains the body fragment {marker:?} — the text now has two homes"
        );
    }
    assert!(
        text.contains("merge-pr partial-comment"),
        "merge-pr.sh does not invoke the verb that replaced the inline bodies"
    );
}
