//! Differential test: the Rust port of `merge-pr.sh`'s closing-reference
//! analysis must agree with the shell, byte for byte, on a shared corpus.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** The obvious alternative — each side
//! generating its own inputs from a shared seed — looks equivalent and is not.
//! An earlier differential in this epic reimplemented a PRNG in bash and in
//! Rust; they diverged on the second draw and the run reported a divergence
//! *in the code under test* when the inputs had diverged instead. Nothing
//! about the code was measured.
//!
//! Here the corpus is a `&[&str]` in this file, written to disk once per run,
//! and the shell reads the same file. The harness cannot lie about which side
//! moved.
//!
//! # What it proves
//!
//! Not "the Rust looks right" — that is what the unit tests are for. This
//! proves the port did not silently change *which references are seen*, which
//! is the only property that matters: a false partial-increment reopens a
//! correctly closed issue, and a missed closing reference closes one that is
//! not finished.
//!
//! The shell side runs the real functions, sourced from
//! `tests/fixtures/merge-pr-refs-retired.sh` — a frozen, byte-for-byte copy of
//! them as they stood immediately before the port. Reading them from the live
//! `merge-pr.sh` stopped being possible the moment it began delegating, and
//! reading them from git history would pin this to a moving ref. The fixture
//! is what keeps this a real comparison once the shell is gone, rather than a
//! test that quietly compares nothing — which is why each case below also
//! asserts how many entries were actually compared.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::refs;

/// Inputs chosen to hit the documented incidents and the boundaries where a
/// regex port most plausibly diverges — not random text, which mostly
/// exercises "no match" on both sides and proves little.
const CORPUS: &[&str] = &[
    // --- the two incidents ---
    "Closes #4600\n\nIf you would rather I attribute this differently, say so\nand I will switch the reference to `Part of #4574` instead.\n",
    "Closes #3\n\n3. Part of #789\n",
    // --- list markers and blockquotes ---
    "Part of #1\n- Part of #2\n* Part of #3\n+ Part of #4\n> Part of #5\n12. Part of #6\n",
    "  Part of #7\n\t Contributes to #8\n",
    // --- case ---
    "part of #9\nPART OF #10\nContributes To #11\ncLoSeS #12\n",
    // --- closing keyword forms and the \b guard ---
    "close #1 closes #2 closed #3 fix #4 fixes #5 fixed #6 resolve #7 resolves #8 resolved #9\n",
    "Discloses #5\nForeclosed #6\nprefixes #7\nunresolved #8\n",
    // --- fences ---
    "```\nPart of #111\nCloses #112\n```\nPart of #222\n",
    "```rust\nPart of #333\n```\n",
    "   ```\nPart of #444\n```\n",
    "Part of #1\n```\nPart of #2\nPart of #3\n", // unclosed fence
    "```\n```\nPart of #5\n",
    // --- inline code spans ---
    "`Part of #10` and Part of #11\n",
    "a ` b ` c `Part of #12` d\n",
    "unbalanced ` backtick Part of #13\n",
    // --- ordering / dedup / numeric sort ---
    "Closes #10\nfixes #9\nresolves #10\nclose #100\nCloses #2\n",
    "Part of #10\nPart of #9\nPart of #100\n",
    // --- adjacency and separators ---
    "Closes  #1\nCloses\t#2\nCloses\n#3\nCloses#4\n",
    "Part of#5\nPart  of #6\n",
    // --- degenerate ---
    "",
    "\n",
    "   \n",
    "#\n#0\n# 1\n",
    "```",
    "`",
    // --- mixed realistic bodies ---
    "## Summary\n\nDoes a thing.\n\nCloses #8191\nPart of #7810\n",
    "Fixes #1, closes #2, and resolves #3.\n\n> Part of #4\n\n```sh\ncloses #5\n```\n",
    // --- a number too large for u64 ---
    "Closes #99999999999999999999999999\n",
    // --- CRLF ---
    "Closes #1\r\nPart of #2\r\n",
];

/// Inputs where the port DELIBERATELY differs from the retired shell, with the
/// reason. Per `verification-recipes.md` §6 a surviving divergence is written
/// down rather than normalised away — the previous harness parsed both sides
/// through `u64` and made this one invisible.
///
/// Keyed by corpus index. The value is the shell's answer; the port's is
/// asserted to differ from it, so this table cannot rot into a list of things
/// that silently started agreeing again.
const KNOWN_DIVERGENCES: &[(usize, &[&str])] = &[
    // `Closes #99999999999999999999999999` — the shell is text-only and emits
    // the digit run verbatim; the port parses to u64 and drops what cannot fit.
    //
    // Kept rather than matched: every consumer of this list compares against a
    // real issue number, so a value no issue can have is noise either way, and
    // carrying it as a string would push u64 parsing onto callers that all
    // need integers. The shell's behaviour here is an artifact of grep, not a
    // decision anyone made.
    (27, &["99999999999999999999999999"]),
];

/// The shell's answer for `i`, if this is a recorded divergence.
fn known_divergence(i: usize) -> Option<&'static [&'static str]> {
    KNOWN_DIVERGENCES
        .iter()
        .find(|(idx, _)| *idx == i)
        .map(|(_, v)| *v)
}

/// The frozen copy of the retired shell functions.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-refs-retired.sh")
}

/// Run one retired shell function against `input`.
///
/// Sourced from `tests/fixtures/merge-pr-refs-retired.sh` — a frozen copy of
/// these six functions as they stood immediately before the port — rather than
/// from the live `merge-pr.sh`, which now delegates and no longer defines
/// them. The fixture is what keeps this a real differential after the shell is
/// gone, instead of a test that silently compares nothing.
fn shell_fn(script: &Path, func: &str, input: &str, extra_arg: Option<&str>) -> Option<String> {
    let call = match extra_arg {
        Some(a) => format!("{func} \"$(cat \"$1\")\" {a}"),
        None => format!("{func} \"$(cat \"$1\")\""),
    };
    // `_closing_refs_stdin` reads stdin rather than $1.
    let call = if func == "_closing_refs_stdin" {
        format!("{func} < \"$1\"")
    } else {
        call
    };

    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(input.as_bytes()).expect("write corpus entry");
    let tmp_path = tmp.path().to_path_buf();

    let prog = format!(
        r#"
set -uo pipefail
source "$2"
{call}
"#
    );

    let out = Command::new("bash")
        // F5: POSIX classes are LOCALE-DEPENDENT. Under a UTF-8 locale the
        // shell's `[[:space:]]` also matches U+2003 EM SPACE (and U+00A0 on
        // BSD), which the Rust port's ASCII class does not — so an unpinned
        // harness compares against whatever locale the developer happens to
        // run, and silently differs between a Mac and CI. `C` is the locale
        // the port models, and saying so here is the point.
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(&prog)
        .arg("bash")
        .arg(&tmp_path)
        .arg(script)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The shell's answer as RAW LINES, not parsed integers.
///
/// F6: parsing through `parse::<u64>().ok()` silently discarded any line the
/// port and the shell genuinely disagreed about. Corpus entry 27
/// (`Closes #99999999999999999999999999`) is exactly that: the shell emits the
/// digit string, the port emits nothing, and the parsing harness compared
/// `[]` with `[]` and called it agreement. A differential that normalises its
/// inputs is measuring its own normaliser.
fn shell_lines(script: &Path, func: &str, input: &str) -> Option<Vec<String>> {
    let raw = shell_fn(script, func, input, None)?;
    Some(
        raw.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The port's answer in the same shape, so the comparison is string-to-string.
fn rust_lines(v: &[u64]) -> Vec<String> {
    v.iter().map(u64::to_string).collect()
}

#[test]
fn partial_increment_refs_agrees_with_the_shell_on_every_corpus_entry() {
    let script = fixture();
    assert!(script.is_file(), "the frozen fixture must exist at {script:?}");

    let mut compared = 0usize;
    for (i, input) in CORPUS.iter().enumerate() {
        let Some(want) = shell_lines(&script, "_partial_increment_refs", input) else {
            continue;
        };
        let got = rust_lines(&refs::partial_increment_refs(input));
        assert_eq!(
            got, want,
            "entry {i} diverged.\n  input: {input:?}\n  shell: {want:?}\n  rust:  {got:?}"
        );
        compared += 1;
    }
    // A differential that silently compared nothing is the failure mode this
    // whole file exists to avoid.
    assert!(
        compared == CORPUS.len(),
        "only {compared}/{} entries were actually compared — the shell harness is not running",
        CORPUS.len()
    );
}

#[test]
fn closing_refs_agrees_with_the_shell_on_every_corpus_entry() {
    let script = fixture();
    let mut compared = 0usize;
    for (i, input) in CORPUS.iter().enumerate() {
        let Some(want) = shell_lines(&script, "_body_closing_refs", input) else {
            continue;
        };
        let got = rust_lines(&refs::closing_refs(input));
        if let Some(shell_said) = known_divergence(i) {
            assert_eq!(want, shell_said, "entry {i}: the recorded shell answer changed");
            assert_ne!(
                got, want,
                "entry {i} is recorded as a KNOWN divergence but the two now agree — \
                 remove it from KNOWN_DIVERGENCES rather than leaving a stale exemption"
            );
        } else {
            assert_eq!(
                got, want,
                "entry {i} diverged.\n  input: {input:?}\n  shell: {want:?}\n  rust:  {got:?}"
            );
        }
        compared += 1;
    }
    assert!(
        compared == CORPUS.len(),
        "only {compared}/{} entries were actually compared",
        CORPUS.len()
    );
}

#[test]
fn snippet_rendering_agrees_with_the_shell() {
    let script = fixture();
    let mut compared = 0usize;
    for (i, input) in CORPUS.iter().enumerate() {
        // Query every issue number either side found, so the snippet
        // comparison covers exactly the references that actually occur.
        let mut ids = refs::closing_refs(input);
        ids.extend(refs::partial_increment_refs(input));
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            if let Some(want) =
                shell_fn(&script, "_closing_ref_snippets", input, Some(&id.to_string()))
            {
                let got = refs::closing_ref_snippets(input, id);
                assert_eq!(
                    got,
                    want.trim_end_matches('\n'),
                    "entry {i} closing snippet for #{id} diverged.\n  input: {input:?}"
                );
                compared += 1;
            }
            if let Some(want) =
                shell_fn(&script, "_partial_increment_ref_snippets", input, Some(&id.to_string()))
            {
                let got = refs::partial_increment_ref_snippets(input, id);
                assert_eq!(
                    got,
                    want.trim_end_matches('\n'),
                    "entry {i} partial snippet for #{id} diverged.\n  input: {input:?}"
                );
                compared += 1;
            }
        }
    }
    assert!(
        compared > 20,
        "only {compared} snippet comparisons ran — the harness is not exercising the shell"
    );
}
