//! Differential test: the Rust port of `merge-pr.sh`'s `champion:hold-state`
//! marker extraction, against the shell it replaced.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** The corpus below is a `&[&str]` in this
//! file, written to disk one entry at a time, and the shell reads the same
//! file — so the harness cannot lie about which side moved.
//!
//! The shell side runs the real retired pipeline, sourced from
//! `tests/fixtures/merge-pr-hold-state-retired.sh` — a frozen, byte-for-byte
//! copy of it as it stood immediately before the port. Reading it out of the
//! live `merge-pr.sh` stopped being possible the moment that file started
//! delegating, and reading it from git history would pin this to a moving ref.
//!
//! # What it proves
//!
//! Not "the Rust looks right" — the unit tests in
//! `src/merge_pr/hold_state/tests.rs` are for that. This proves the port
//! changed *which marker is read* only where it meant to. The port takes two
//! deliberate divergences ([`KNOWN_DIVERGENCES`]); everywhere else the two
//! implementations must pick the same SHA, including on the shapes nobody
//! wrote a unit test for.
//!
//! Both divergences move in the same direction — the shell loses a real
//! marker or believes a fake one, and the port does not — which is the
//! direction that matters: nothing else on the merge path duplicates this
//! warning, so a lost one is simply never said.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::hold_state;

/// Inputs drawn from the marker's grammar and from the shapes a PR comment
/// thread actually produces — the producer's exact line, the ways it gets
/// quoted, and the boundaries of the `[0-9a-f]` capture. Not random text,
/// which would exercise "no marker" on both sides and prove nothing.
const CORPUS: &[&str] = &[
    // --- 0-3: the producer's own shape ---
    "<!-- champion:merge-risk-hold -->\n<!-- champion:hold-state head=0123456789abcdef0123456789abcdef01234567 -->\n**Champion: Holding for Human Merge**\n",
    "<!-- champion:hold-state head=abc1234 -->\nSome other hold-state prose.",
    "    <!-- champion:hold-state head=abc1234 -->\n",
    "<!-- champion:hold-state head=deadbeef -->",
    // --- 4-6: multiple hold episodes; last wins ---
    "<!-- champion:hold-state head=aaaa1111 -->\nfirst\n<!-- champion:hold-state head=bbbb2222 -->\nsecond\n",
    "<!-- champion:hold-state head=aaaa1111 --> <!-- champion:hold-state head=bbbb2222 -->\n",
    "<!-- champion:hold-state head=bbbb2222 -->\n<!-- champion:hold-state head=aaaa1111 -->\n",
    // --- 7-9: the placeholder documentation line (divergence class A) ---
    "<!-- champion:hold-state head=<sha> -->\n",
    "<!-- champion:hold-state head=abc1234 -->\nHolding.\nThe hold records <!-- champion:hold-state head=<sha> --> in its notice.\n",
    "champion:hold-state head=\n",
    // --- 10-13: quoted, not recorded (divergence class B) ---
    "Champion writes champion:hold-state head=abc1234 into the hold notice.\n",
    "See the `champion:hold-state head=abc1234` marker.\n",
    "```\n<!-- champion:hold-state head=abc1234 -->\n```\n",
    "<!-- champion:hold-state head=abc1234 -->\nHolding.\nNote: champion:hold-state head=ffff9999 is the shape.\n",
    // --- 14-16: HTML-comment delimiters that do not close on the line ---
    "<!-- champion:hold-state head=abc1234\n-->\n",
    "<!--\nchampion:hold-state head=abc1234\n-->\n",
    "<!-- champion:hold-state head=abc1234 --\n",
    // --- 17-20: the capture's boundaries ---
    "<!-- champion:hold-state head=ABC1234 -->\n",
    "<!-- champion:hold-state head=abc1234-stale -->\n",
    "<!-- champion:hold-state head=abc1234 (episode 2) -->\n",
    "<!-- champion:hold-state  head=abc1234 -->\n",
    // --- 21-23: adjacent text and nesting ---
    "prefix <!-- champion:hold-state head=abc1234 --> suffix\n",
    "<!-- <!-- champion:hold-state head=abc1234 --> -->\n",
    "<!-- champion:hold-state head=abc1234 --><!-- champion:hold-state head=ffff9999 -->\n",
    // --- 24-27: degenerate ---
    "",
    "\n",
    "   \n",
    "Just a regular Judge approval comment, no marker here.\n",
    // --- 28-29: line endings ---
    "<!-- champion:hold-state head=abc1234 -->\r\n",
    "<!-- champion:hold-state head=aaaa1111 -->\r\n<!-- champion:hold-state head=bbbb2222 -->\r\n",
    // --- 30-31: a realistic multi-comment thread ---
    "LGTM, approving.\n\n<!-- champion:merge-risk-hold -->\n<!-- champion:hold-state head=fedcba9876543210fedcba9876543210fedcba98 -->\n**Champion: Holding for Human Merge**\n\n- touches a critical file\n\nOperator: cleared, merging by hand.\n",
    "## Review\n\n```sh\ngh pr view 1 --comments\n```\n\n<!-- champion:hold-state head=1234abc -->\n",
];

/// Where the port DELIBERATELY differs from the retired shell, and why.
///
/// Keyed by corpus index; the value is the shell's answer. The test asserts
/// the two sides DO differ on every listed index, so this table cannot rot
/// into a list of things that quietly started agreeing again — which is how a
/// divergence table stops being evidence.
///
/// Recognised by MECHANISM, not by a property of the input: each entry names
/// which of the retired pipeline's two steps produced the shell's answer
/// (`tail -1` over an empty `[0-9a-f]*` capture, or a bare substring match
/// outside any HTML comment), and the port's answer is asserted exactly rather
/// than merely "different". A port that changed for an unrelated reason —
/// taking the first marker instead of the last, say — would fail here rather
/// than be absorbed as expected.
const KNOWN_DIVERGENCES: &[(usize, Option<&str>, Option<&str>, &str)] = &[
    // (index, shell answer, port answer, class)
    //
    // CLASS A — "placeholder-mask". `[0-9a-f]*` matches the EMPTY string, so
    // the documentation line `head=<sha>` IS a match with an empty capture.
    // `tail -1` takes it, and `[[ -n "$hold_head" ]]` then returns silently.
    // A single comment quoting champion-pr-merge.md's own template therefore
    // disabled the check for the whole PR. The port requires `[0-9a-f]+`.
    (
        8,
        None,
        Some("abc1234"),
        "placeholder-mask: the shell's last match captured nothing and erased the real hold",
    ),
    //
    // CLASS B — "unanchored". The shell matched the marker text wherever it
    // appeared. The port requires it to sit inside an HTML comment that opens
    // and closes on one line — exactly the shape the producer writes, and the
    // same narrowing Champion's own reader applied to the sibling marker
    // (#5371). Prose (10), backticks (11) and a bare-but-shaped line (9's
    // empty capture aside) stop being recorded state.
    (10, Some("abc1234"), None, "unanchored: a prose mention is not recorded state"),
    (
        11,
        Some("abc1234"),
        None,
        "unanchored: a backticked mention is not recorded state",
    ),
    (
        13,
        Some("ffff9999"),
        Some("abc1234"),
        "unanchored: a trailing prose mention no longer outranks the real earlier marker",
    ),
    (
        14,
        Some("abc1234"),
        None,
        "unanchored: an HTML comment that does not close on its line is not a comment here",
    ),
    (
        15,
        Some("abc1234"),
        None,
        "unanchored: the marker is on its own line, outside any single-line comment",
    ),
    (
        16,
        Some("abc1234"),
        None,
        "unanchored: `--` is not `-->`, so nothing closes on this line",
    ),
];

fn known_divergence(
    i: usize,
) -> Option<(Option<&'static str>, Option<&'static str>, &'static str)> {
    KNOWN_DIVERGENCES
        .iter()
        .find(|(idx, ..)| *idx == i)
        .map(|(_, shell, rust, why)| (*shell, *rust, *why))
}

/// The frozen copy of the retired shell pipeline.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-hold-state-retired.sh")
}

/// Run the retired pipeline over `input`, returning its answer.
///
/// `set -euo pipefail` deliberately: that is what `merge-pr.sh` runs under,
/// and the retired assignment's `|| true` exists precisely because `grep`
/// exiting 1 on "no match" otherwise took the whole merge down (#7678).
/// Running the fixture under weaker options would hide that.
fn shell_answer(script: &Path, input: &str) -> Option<String> {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(input.as_bytes()).expect("write corpus entry");
    let tmp_path = tmp.path().to_path_buf();

    let out = Command::new("bash")
        // POSIX classes are locale-dependent; `C` is the locale the port
        // models, and pinning it here is why this comparison means the same
        // thing on a developer's box and in CI.
        .env("LC_ALL", "C")
        .arg("-c")
        .arg("set -euo pipefail\nsource \"$2\"\n_retired_hold_head < \"$1\"\n")
        .arg("bash")
        .arg(&tmp_path)
        .arg(script)
        .output()
        .expect("run the frozen retired pipeline");

    assert!(
        out.status.success(),
        "the retired pipeline must not fail under `set -euo pipefail` (that was #7678); \
stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim_end_matches('\n');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[test]
fn recorded_head_agrees_with_the_shell_except_where_the_port_meant_to_differ() {
    let script = fixture();
    assert!(script.is_file(), "the frozen fixture must exist at {script:?}");

    let mut compared = 0usize;
    let mut diverged = 0usize;

    for (i, input) in CORPUS.iter().enumerate() {
        let shell = shell_answer(&script, input);
        let rust = hold_state::recorded_head(input);
        compared += 1;

        match known_divergence(i) {
            Some((want_shell, want_rust, why)) => {
                diverged += 1;
                assert_eq!(
                    shell.as_deref(),
                    want_shell,
                    "corpus[{i}] ({why}): the RETIRED side moved. The fixture is a frozen record \
of behaviour that no longer exists anywhere else — if it changed, the fixture was edited. \
Input: {input:?}"
                );
                assert_eq!(
                    rust.as_deref(),
                    want_rust,
                    "corpus[{i}] ({why}): the PORT's answer is not the recorded one. Input: {input:?}"
                );
                assert_ne!(
                    shell.as_deref(),
                    rust.as_deref(),
                    "corpus[{i}] is listed as a divergence but the two sides now AGREE. Remove the \
entry rather than leaving a table that no longer records anything. Input: {input:?}"
                );
            }
            None => assert_eq!(
                shell.as_deref(),
                rust.as_deref(),
                "corpus[{i}]: undeclared divergence between the retired shell and the port. \
Either it is a bug, or it is a decision that belongs in KNOWN_DIVERGENCES with its mechanism \
named. Input: {input:?}"
            ),
        }
    }

    assert_eq!(compared, CORPUS.len(), "every corpus entry must be compared");
    assert_eq!(
        diverged,
        KNOWN_DIVERGENCES.len(),
        "every recorded divergence must have been exercised"
    );
}

/// Size says nothing about reach. A corpus that quietly stopped containing a
/// marker at all would make the test above green while comparing "no marker"
/// with "no marker" thirty-two times.
#[test]
fn the_corpus_discriminates() {
    let script = fixture();

    let mut shell_found = 0usize;
    let mut rust_found = 0usize;
    let mut distinct: Vec<String> = Vec::new();

    for input in CORPUS {
        if let Some(s) = shell_answer(&script, input) {
            shell_found += 1;
            if !distinct.contains(&s) {
                distinct.push(s);
            }
        }
        if hold_state::recorded_head(input).is_some() {
            rust_found += 1;
        }
    }

    assert!(
        shell_found >= 20,
        "the retired side must actually find markers in most of the corpus, got {shell_found}"
    );
    assert!(
        rust_found >= 14,
        "the port must actually find markers in most of the corpus, got {rust_found}"
    );
    assert!(
        distinct.len() >= 6,
        "the corpus must exercise more than one recorded SHA, got {distinct:?}"
    );
}

/// The comparison the shell wrapper makes once `recorded_head` has answered.
/// Kept here rather than only in the unit tests so the differential covers the
/// whole decision — "which SHA" and "is it stale" — not just the parse.
#[test]
fn a_recorded_head_equal_to_the_current_head_is_never_a_warning() {
    for input in CORPUS {
        let Some(recorded) = hold_state::recorded_head(input) else {
            assert_eq!(hold_state::assess("1", input, "whatever"), None);
            continue;
        };
        assert_eq!(
            hold_state::assess("1", input, &recorded),
            None,
            "a marker naming the head being merged must be silent; input {input:?}"
        );
        assert!(
            hold_state::assess("1", input, "0000000")
                .is_some_and(|m| m.contains(&format!("head={recorded}"))),
            "a marker naming a different head must warn and name it; input {input:?}"
        );
    }
}
