//! Differential test: the Rust port of `merge-pr.sh`'s merge-retry
//! classification ladder must agree with the retired `grep` ladder on a corpus
//! generated from the grammar, not from memory (#8191 slice).
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6, three rules in particular:
//!
//! - **Generate the corpus ONCE and feed both sides the same bytes.** [`corpus`]
//!   enumerates deterministically — no PRNG, nothing to keep synchronised — and
//!   each case is handed to the shell as the same `String` the Rust receives.
//! - **Generate the whole ALPHABET, not a representative sample.** Every one of
//!   the five retired patterns appears in every case shape, in every case-fold
//!   variant, and in every ordered pair with every other, because the *pairs*
//!   are where the precedence lives.
//! - **Pin `LC_ALL=C`.** `grep -i` folds according to the locale. The port folds
//!   ASCII-only, which is what `C` means; an unpinned harness would compare
//!   against whatever the developer's shell is set to.
//!
//! # What it proves
//!
//! Not "the Rust looks right" — the unit tests beside the module do that. This
//! proves the port did not change **which route a failed merge takes**. The two
//! middle routes are opposites: `base-modified` answers the failure with
//! `forge_update_branch` and another merge attempt, `head-mismatch` refuses to
//! retry-and-merge at all (#5579). Swapping them on a body that names both is a
//! one-word diff that no verdict-level assertion elsewhere would see.
//!
//! # The one divergence class, named up front
//!
//! The corpus is UTF-8 and NUL-free **by construction**, because the retired
//! implementation took its input through a shell variable (`echo "$1"`) and a
//! shell variable cannot hold a NUL byte at all. There is therefore no possible
//! differential evidence about NUL or invalid UTF-8; the port's behaviour on
//! both is pinned by unit test (`response::tests::tolerances_are_preserved`)
//! against `grep`'s documented byte-orientation instead. That is a gap in *this*
//! harness, stated rather than papered over — not a gap in coverage.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::response::{classify, MergeResponseKind};

/// The five literal markers the retired ladder tested for, in ladder order.
///
/// Spelled out here rather than imported from the port: a differential whose
/// corpus is generated from the implementation under test can only find bugs
/// the implementation already agrees with itself about. These are transcribed
/// from the retired `grep` patterns in
/// `tests/fixtures/merge-pr-response-retired.sh`.
const MARKERS: &[(&str, &str)] = &[
    ("405", "Merge already in progress"),
    ("head-rest", "Head branch was modified."),
    ("head-gitea", "head out of date"),
    ("head-graphql", "expectedHeadOid"),
    ("base", "Base branch was modified"),
];

/// Near-misses: one edit away from a marker, chosen so each edit targets a
/// *specific* thing the tool contributed rather than being random mutation.
fn near_misses() -> Vec<String> {
    let mut v = Vec::new();
    for (_, marker) in MARKERS {
        // The escaped `\.` in the head-REST pattern is a literal dot. These
        // probe whether the port widened it to a wildcard, and whether the
        // base pattern (which has no trailing period) is unaffected.
        v.push(marker.replace('.', ","));
        v.push(marker.replace('.', "!"));
        v.push(marker.replace('.', "x"));
        v.push(marker.trim_end_matches('.').to_string());
        // `grep` is line-oriented and every pattern is newline-free, so a
        // marker split at any interior space must match on NEITHER side.
        for (i, ch) in marker.char_indices() {
            if ch == ' ' {
                v.push(format!("{}\n{}", &marker[..i], &marker[i + 1..]));
            }
        }
        // Doubled internal space: a literal pattern does not tolerate it.
        v.push(marker.replace(' ', "  "));
        // U+00A0 NO-BREAK SPACE — a POSIX space class does not match it and a
        // Unicode-aware `\s` does. No pattern here uses a class, so both sides
        // must MISS; the case is kept because this is the exact divergence
        // §6 records costing an earlier slice a silent per-host difference.
        v.push(marker.replace(' ', "\u{a0}"));
        // Unicode case-folding probes: `to_lowercase()` / `(?i)` fold these,
        // ASCII folding does not.
        v.push(marker.replace('s', "\u{17f}")); // LATIN SMALL LETTER LONG S
        v.push(marker.replace('d', "\u{130}")); // LATIN CAPITAL I WITH DOT ABOVE
        v.push(marker.replace('k', "\u{212a}")); // KELVIN SIGN
    }
    v
}

/// Case-fold variants of a fragment. `grep -Ei` applied to the head-mismatch
/// alternation only; the other two matchers were case-SENSITIVE, so every one
/// of these must be compared for all five markers to pin that asymmetry.
fn case_variants(s: &str) -> Vec<String> {
    let alternating: String = s
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if i % 2 == 0 {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    vec![
        s.to_string(),
        s.to_ascii_uppercase(),
        s.to_ascii_lowercase(),
        alternating,
    ]
}

/// How a fragment sits inside a real captured response. `forge_merge_pr` is
/// captured with `2>&1`, so the marker routinely arrives wrapped in `gh`
/// diagnostics and on a line of its own partway down.
fn templates(fragment: &str) -> Vec<String> {
    vec![
        fragment.to_string(),
        format!("Error: {fragment} (HTTP 409)"),
        format!("gh: request failed\n{fragment}\nretrying...\n"),
        format!("{fragment}\n"),
        format!("\n\n{fragment}"),
        // The marker at the very end of a long line, to rule out any
        // window/offset arithmetic error at the buffer's tail.
        format!("{}{fragment}", "x".repeat(300)),
    ]
}

/// The corpus: singles (every marker and near-miss, in every case variant, in
/// every template) plus every ORDERED PAIR of markers under three separators.
///
/// The pairs are the point. A body naming two markers is the only input whose
/// answer depends on the ladder's order, and both orderings of each pair are
/// generated so a correct answer cannot be an artifact of which marker the
/// input happens to mention first.
fn corpus() -> Vec<String> {
    let mut cases: Vec<String> = Vec::new();

    // --- degenerate and non-marker inputs -----------------------------------
    for base in [
        "",
        "\n",
        "x",
        "Error:",
        "fatal: not a git repository\n",
        "gh: HTTP 502 Bad Gateway",
        // bash's `echo` swallows an argument that is exactly one of these. The
        // port's shell wrapper uses `printf '%s'`, which does not — so these
        // three pin that the substitution changes no answer.
        "-n",
        "-e",
        "-E",
        "-neE",
        // Leading `-` with a marker attached: NOT a bare flag, so `echo`
        // prints it and the marker must be found.
        "-n Head branch was modified.",
        "--Base branch was modified",
    ] {
        cases.push(base.to_string());
    }

    // --- every marker and near-miss × case variants × templates -------------
    let mut fragments: Vec<String> = MARKERS.iter().map(|(_, m)| (*m).to_string()).collect();
    fragments.extend(near_misses());
    for fragment in &fragments {
        for variant in case_variants(fragment) {
            cases.extend(templates(&variant));
        }
    }

    // --- every ordered pair of markers, under three separators --------------
    for (_, a) in MARKERS {
        for (_, b) in MARKERS {
            for sep in [" ", "\n", "\t"] {
                cases.push(format!("Error: {a}{sep}{b} (HTTP 409)"));
            }
        }
    }

    // --- all three SHA-shaped routes at once, plus the 405 ------------------
    cases.push(
        "Merge already in progress. Head branch was modified. Base branch was modified."
            .to_string(),
    );
    cases.push(
        "Base branch was modified\nHead branch was modified.\nMerge already in progress"
            .to_string(),
    );

    cases
}

/// The frozen copy of the retired ladder.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-response-retired.sh")
}

/// The only four answers the retired ladder can print. Anything else on stdout
/// means the oracle did not run the ladder to completion.
const ROUTES: &[&str] = &[
    "merge-in-progress",
    "head-mismatch",
    "base-modified",
    "other",
];

/// Run the retired ladder over `input`, passed exactly as the retry loop passed
/// it: as an argv argument, read back through `echo "$1"`.
///
/// Self-validating (#10189): a spawn error, a nonzero exit, ANY stderr, or a
/// stdout that is not exactly one of [`ROUTES`] panics with `oracle did not
/// run`. Every caller therefore either gets a real classification or stops —
/// no case can be skipped, and no harness failure can be read as a route.
///
/// # Why `set -u` and NOT `set -uo pipefail` (#10189, main CI run 37178206556)
///
/// That run failed with case 159, `"Merge already in progress\n"`, classified
/// `other` by the shell. The cause is a SIGPIPE race that `pipefail` turns
/// into a wrong answer:
///
/// 1. bash line-buffers its stdout, so `echo "$1"` of an input that ends in a
///    newline makes TWO `write(2)`s: `"Merge already in progress\n"`, then the
///    echo's own `"\n"` (confirmed with `strace -e trace=write`).
/// 2. `grep -q` exits at its first match — i.e. possibly right after the first
///    write. If it is gone before the second write, the echo subshell dies of
///    SIGPIPE (status 141).
/// 3. Under `pipefail` the pipeline's status is then 141, the `if` is false,
///    and the ladder falls through to the next rung — which does not match —
///    and finally prints `other`, exiting 0. Nothing on stderr: bash does not
///    report SIGPIPE deaths of pipeline members.
///
/// It only fires when the scheduler runs `grep` between the two writes, which
/// a CPU-starved CI shard under nextest parallelism can do and an idle laptop
/// essentially never does (0/3000 locally, pinned or not). Forcing the window
/// with `strace -f -e inject=write:delay_enter=300000:when=2` reproduces it
/// deterministically: `set -uo pipefail` prints `other`, `set -u` prints
/// `merge-in-progress`.
///
/// Production `merge-pr.sh` did run under `set -euo pipefail`, so the retired
/// ladder carried this same latent race; the Rust port (no pipe) does not. The
/// oracle exists to model the ladder's *matching and precedence*, not a
/// scheduler-dependent misfire, so the harness drops `pipefail`: without it
/// the pipeline's status is `grep`'s alone, which is exactly the match result.
/// The fixture itself stays frozen and verbatim.
fn shell(script: &Path, input: &str) -> String {
    let prog = r#"
set -u
source "$1"
retired_classify_merge_response "$2"
"#;
    let out = Command::new("bash")
        // `grep -i` folds per the locale; the port folds ASCII-only, which is
        // exactly what `C` means. Unpinned, this harness would compare against
        // the developer's ambient locale and could differ between a Mac and CI.
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(prog)
        .arg("bash")
        .arg(script)
        .arg(input)
        .output()
        .unwrap_or_else(|e| panic!("oracle did not run: could not spawn bash: {e}\n  input: {input:?}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let answer = stdout.strip_suffix('\n').unwrap_or(&stdout);
    if !out.status.success() || !stderr.is_empty() || !ROUTES.contains(&answer) {
        panic!(
            "oracle did not run: the retired ladder did not produce a classification.\n  \
             input:  {input:?}\n  status: {}\n  stdout: {stdout:?}\n  stderr: {stderr:?}",
            out.status
        );
    }
    answer.to_string()
}

#[test]
fn every_route_agrees_with_the_retired_grep_ladder() {
    let script = fixture();
    assert!(script.is_file(), "the frozen fixture must exist at {script:?}");

    // `shell` panics rather than returning a non-answer, so every case is
    // compared; there is no skip path left for a silent harness to hide in.
    for (i, input) in corpus().iter().enumerate() {
        let want = shell(&script, input);
        let got = classify(input.as_bytes()).token();
        assert_eq!(
            got, want,
            "case {i} diverged.\n  input: {input:?}\n  shell: {want:?}\n  rust:  {got:?}"
        );
    }
}

/// The oracle's self-check must actually trip: a fixture that cannot be
/// sourced has to fail as `oracle did not run`, not as a divergence or a pass.
#[test]
#[should_panic(expected = "oracle did not run")]
fn a_broken_oracle_fails_loudly_instead_of_classifying() {
    shell(
        Path::new("/nonexistent/merge-pr-response-retired.sh"),
        "Merge already in progress",
    );
}

/// The corpus must have discriminating power, not merely size. `700 cases` says
/// nothing about which branches they reach — §6's sharpest warning. So assert a
/// floor on each route's population: a corpus that quietly stopped producing
/// head-mismatches would otherwise keep passing while proving nothing about the
/// one route whose misclassification is dangerous.
#[test]
fn the_corpus_reaches_every_route_from_both_sides() {
    let script = fixture();
    let cases = corpus();

    let mut counts = [0usize; 4];
    for input in &cases {
        let want = shell(&script, input);
        let idx = ROUTES
            .iter()
            .position(|r| *r == want)
            .expect("shell() only returns a member of ROUTES");
        counts[idx] += 1;
    }

    // Floors, not exact counts: the corpus is meant to be extended. Each is
    // comfortably below the current population and comfortably above zero.
    for (idx, (name, floor)) in [
        ("merge-in-progress", 20usize),
        ("head-mismatch", 60),
        ("base-modified", 20),
        ("other", 60),
    ]
    .iter()
    .enumerate()
    {
        assert!(
            counts[idx] >= *floor,
            "the SHELL classified only {} case(s) as {name} (floor {floor}) — \
             the corpus has stopped exercising that route",
            counts[idx]
        );
    }
}

/// Make the harness go RED on purpose (§6, Cause 1: "before believing a green
/// number, make it red"). If the ladder's two SHA-shaped routes were swapped,
/// at least one corpus case must notice. Proving that *here* means the
/// per-case `assert_eq!` above is measuring something.
#[test]
fn swapping_the_two_sha_routes_would_be_caught() {
    // A deliberately wrong classifier: base-modified tested BEFORE
    // head-mismatch, which is the reorder the retired `awk` source scan existed
    // to prevent.
    fn wrong(response: &[u8]) -> MergeResponseKind {
        let has = |needle: &str| {
            response
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
        };
        if has("Merge already in progress") {
            MergeResponseKind::MergeInProgress
        } else if has("Base branch was modified") {
            MergeResponseKind::BaseModified
        } else if has("Head branch was modified.")
            || has("head out of date")
            || has("expectedHeadOid")
        {
            MergeResponseKind::HeadMismatch
        } else {
            MergeResponseKind::Other
        }
    }

    let caught = corpus()
        .iter()
        .filter(|input| wrong(input.as_bytes()) != classify(input.as_bytes()))
        .count();
    assert!(
        caught > 0,
        "the corpus cannot distinguish the correct ladder from a reordered one — \
         it is not testing the precedence it claims to"
    );
}
