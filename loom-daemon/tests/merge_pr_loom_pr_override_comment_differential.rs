//! Differential test: the Rust port of `merge-pr.sh`'s `--allow-unapproved`
//! override audit-comment body must render the same bytes the retired shell
//! did (#8191 slice, #7419).
//!
//! # Why a differential for text
//!
//! Same reasoning as `merge_pr_partial_comment_differential.rs`: this body's
//! whole value IS the bytes, and until this slice nothing asserted them.
//! `test-merge-pr-loom-pr-label-guard.sh`'s T3/T4 check that A comment is
//! posted (and that a dry-run posts none), via its own recording stub; they
//! do not pin the body against a frozen oracle, which is this file's job.
//!
//! # Rules from `defaults/docs/verification-recipes.md` §6 this obeys
//!
//! - **Generate the corpus ONCE and feed both sides the same bytes.** The
//!   corpora below are deterministic literals; every case is handed to `bash`
//!   as the same `&str` the Rust receives.
//! - **Generate the ALPHABET, not a sample.** `LABELS` is not a list of
//!   plausible label sets — it is the shapes that separate "both sides
//!   substitute `$PR_LABELS` the same way" from "both sides happen to agree on
//!   one label": empty (the `<none>` trigger), a single label, a
//!   newline-joined multi-label set (`jq -r '.labels[]?.name'`'s actual
//!   shape), whitespace-only (NOT empty — the one divergence from this
//!   guard's own `shown()` helper, named in `override_comment`'s doc comment),
//!   and labels carrying `$`/backtick/`%`/`{}` (hazards if either side ever
//!   routes the value through a second round of expansion).
//! - **Say WHICH implementation your copy models.** The fixture's header
//!   states exactly what is verbatim (the body text) versus reconstructed
//!   (the function boundary) — see its own comment.
//! - **Pin `LC_ALL=C`.** The body contains em dashes and curly-quote-free
//!   straight quotes only, but the sibling differentials pin it on principle
//!   (locale-dependent `printf`/`date` behaviour is the recurring hazard
//!   class), and this one matches them.
//!
//! # The one divergence, named up front
//!
//! NUL bytes. The retired implementation carried the body through shell
//! *variables* built from `$PR_NUMBER`/`$PR_HEAD_SHA`/`$PR_LABELS`, none of
//! which can hold a NUL; the port's `&str` parameters cannot either without
//! the caller having put one there, and `merge-pr.sh` passes `--pr`/
//! `--head-sha` as clap arguments (no NUL in argv) and labels over stdin
//! (where a NUL predates UTF-8 validity, which `read_to_string` already
//! refuses). There is nothing to diverge.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::loom_pr_guard::override_comment;

/// PR numbers and head SHAs sweep the same hazard alphabet
/// `merge_pr_partial_comment_differential.rs` uses for its own two
/// identifier-shaped parameters.
const IDENTIFIERS: &[&str] = &[
    "1",
    "999",
    "0",
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
];

/// The label-set alphabet: `$PR_LABELS` verbatim, as `jq -r
/// '.labels[]?.name // empty'` renders it (newline-joined, no trailing
/// newline) — plus the shapes that separate "substitutes `<none>`" from
/// "renders verbatim".
const LABELS: &[&str] = &[
    "",    // the ONLY `<none>` trigger
    "   ", // whitespace-only — NOT empty, must render verbatim
    "loom:pr",
    "loom:review-requested\nloom:operator",
    "label`with`backticks",
    "label$with$dollar",
    "label%with%percent",
    "label{with}braces",
    "  loom:pr  ", // leading/trailing space on a real label
];

const TIMESTAMPS: &[&str] = &[
    "2026-09-30T12:00:00Z",
    "",
    "not a date",
    "1970-01-01T00:00:00Z",
];

/// The case list, built ONCE and fed to both sides. Each alphabet is swept
/// independently against fixed partners, plus adversarial combinations that
/// falsify the "parameters substitute independently" assumption rather than
/// assume it — same discipline as the partial-comment differential.
fn cases() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    let mut out: Vec<(&str, &str, &str, &str)> = Vec::new();
    let ts0 = TIMESTAMPS[0];
    let pr0 = "999";
    let sha0 = "deadbeef";
    let labels0 = "loom:review-requested";
    for pr in IDENTIFIERS {
        out.push((pr, sha0, labels0, ts0));
    }
    for sha in IDENTIFIERS {
        out.push((pr0, sha, labels0, ts0));
    }
    for labels in LABELS {
        out.push((pr0, sha0, labels, ts0));
    }
    for ts in TIMESTAMPS {
        out.push((pr0, sha0, labels0, ts));
    }
    // Independence falsifiers: several adversarial values in the same render.
    out.push(("a`b", "x$y", "label`with`backticks", ""));
    out.push(("", "", "", ""));
    out.push(("{z}", "p%q", "label%with%percent", "not a date"));
    out.push(("  spaced  ", "é—ü", "line1\nline2", "1970-01-01T00:00:00Z"));
    out
}

/// The frozen copy of `_check_loom_pr_label`'s override-comment statements.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/merge-pr-loom-pr-override-comment-retired.sh")
}

/// Recording `forge_gh_comment_rl_safe` + a fixed `date`. `$1` = fixture,
/// `$2` = PR number, `$3` = head SHA, `$4` = labels, `$5` = timestamp.
///
/// `forge_gh_comment_rl_safe` echoes its THIRD argument — the retired
/// `$override_comment` local — with `printf '%s'`, which appends nothing, so
/// the transcript is the body and only the body.
const HARNESS: &str = r#"set -euo pipefail
FIXTURE="$1"; PR="$2"; SHA="$3"; LABELS="$4"; TS="$5"
date() { printf '%s\n' "$TS"; }
REPO_NWO="owner/repo"; PR_NUMBER="$PR"; PR_HEAD_SHA="$SHA"; PR_LABELS="$LABELS"
forge_gh_comment_rl_safe() { printf '%s' "$3"; }
source "$FIXTURE"
_frozen_loom_pr_override_comment
"#;

/// How many times one query may be re-issued after an *environmental* fault
/// before the harness gives up and fails loudly. Mirrors the sibling
/// differentials.
const ORACLE_ATTEMPTS: usize = 4;

/// Did bash fail to create a process, rather than the frozen function
/// producing a body? `LC_ALL=C` is pinned below so these strings are not
/// localized.
fn is_process_creation_failure(stderr: &str) -> bool {
    stderr.contains(": fork:")
        || stderr.contains("Resource temporarily unavailable")
        || stderr.contains("Cannot allocate memory")
}

/// One raw run of the harness. `Ok` is a body; `Err` says this invocation did
/// not answer at all.
fn run_once(args: &[String]) -> Result<String, String> {
    let mut cmd = Command::new("bash");
    cmd.env("LC_ALL", "C").arg("-c").arg(HARNESS).arg("bash");
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
fn shell_body(args: &[String]) -> String {
    let mut faults: Vec<String> = Vec::new();
    for attempt in 0..ORACLE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(150 * attempt as u64));
        }
        match run_once(args) {
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

#[test]
fn the_override_comment_matches_the_retired_shell_byte_for_byte() {
    let fixture_path = fixture().to_string_lossy().into_owned();
    let corpus = cases();
    let mut checked = 0usize;
    for (pr, sha, labels, ts) in &corpus {
        let shell = shell_body(&args(&[&fixture_path, pr, sha, labels, ts]));
        let rust = override_comment(pr, sha, labels, ts);
        assert_eq!(
            rust, shell,
            "divergence for pr={pr:?} sha={sha:?} labels={labels:?} ts={ts:?}\n\
             --- rust ---\n{rust}\n--- shell ---\n{shell}\n"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        corpus.len(),
        "the corpus shrank — a `continue` or an early return is skipping cases"
    );
    assert!(corpus.len() >= IDENTIFIERS.len() + LABELS.len(), "the alphabets were not swept");
}

/// The retired `comment="…"` ended at its closing quote. Asserted against the
/// SHELL, not just the Rust: command substitution strips trailing newlines, so
/// a `$(...)`-based comparison would hide a port that added one.
#[test]
fn the_retired_body_did_not_end_with_a_newline() {
    let body = shell_body(&args(&[
        &fixture().to_string_lossy(),
        "1",
        "deadbeef",
        "loom:pr",
        "2026-09-30T12:00:00Z",
    ]));
    assert!(!body.is_empty(), "the recording stub captured nothing");
    assert!(
        !body.ends_with('\n'),
        "the retired body ended at its closing quote, not a newline"
    );
}

/// Only the EMPTY string triggers `${PR_LABELS:-<none>}`'s placeholder.
/// Asserted against the shell directly, so a future edit to the fixture that
/// quietly started trimming cannot pass unnoticed just because the Rust
/// (correctly) also does not trim.
#[test]
fn only_the_empty_label_set_renders_the_none_placeholder_in_the_retired_shell() {
    let empty = shell_body(&args(&[&fixture().to_string_lossy(), "1", "sha", "", "TS"]));
    assert!(empty.contains("<none>"), "empty labels must render <none>: {empty:?}");
    let whitespace = shell_body(&args(&[&fixture().to_string_lossy(), "1", "sha", "   ", "TS"]));
    assert!(
        !whitespace.contains("<none>"),
        "whitespace-only labels are NOT empty to `${{VAR:-default}}` and must render verbatim: {whitespace:?}"
    );
}

/// The fixture must be the frozen copy this file names, not something that
/// grew a `loom-daemon` call and now compares the port against itself.
#[test]
fn the_fixture_does_not_delegate_back_to_the_port() {
    let path = fixture();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
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

/// `merge-pr.sh` must still be able to obtain this body. The wrapper that
/// replaced the inline text is asserted by
/// `defaults/scripts/tests/test-merge-pr-loom-pr-label-guard.sh`; what is
/// checked here is the half that lives on this side of the seam — that the
/// live script no longer carries the body, so there is exactly one place the
/// text can be edited.
///
/// The markers are BODY fragments, not the topic. `merge-pr.sh`'s #8896 dedup
/// comment legitimately *names* the comment it dedupes ("the identical `Merge
/// Proceeded Without loom:pr` comment both times") and must stay readable; a
/// bare `"Merge Proceeded Without"` search would read that prose as a second
/// home for the text. So each marker below is a span that only the rendered
/// body can contain — the markdown heading with its backticks, the field
/// label, the sign-off — the same "fragment, not subject" discipline
/// `merge_pr_partial_comment_differential.rs` applies to its own two bodies.
#[test]
fn the_live_script_no_longer_carries_the_body() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/merge-pr.sh");
    let Ok(text) = std::fs::read_to_string(&script) else {
        // Consumer repos vendor `.loom/scripts` without `defaults/`; nothing
        // to check there, and a missing file is not a failing port.
        return;
    };
    for marker in [
        "## Merge Proceeded Without",
        "- **Labels at merge time**:",
        "no forge-visible Judge review signal existed",
        "asserted responsibility for this override",
        "*Recorded by merge-pr.sh at",
    ] {
        assert!(
            !text.contains(marker),
            "merge-pr.sh still contains the body fragment {marker:?} — the text now has two homes"
        );
    }
    assert!(
        text.contains("merge-pr loom-pr-override-comment"),
        "merge-pr.sh does not invoke the verb that replaced the inline body"
    );
}
