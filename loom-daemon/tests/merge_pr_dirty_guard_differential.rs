//! Differential test: the Rust port of `merge-pr.sh`'s #5031 dirty-worktree
//! data-loss guard, against the shell it replaced.
//!
//! # Why this shape
//!
//! `defaults/docs/verification-recipes.md` §6: **generate the corpus ONCE and
//! feed both sides the same bytes.** The corpus below is a `&[&str]` in this
//! file, written to disk one entry at a time, and the shell reads the same file
//! — so the harness cannot lie about which side moved.
//!
//! The shell side runs the real retired predicates, sourced from
//! `tests/fixtures/merge-pr-dirty-guard-retired.sh` — a frozen, byte-for-byte
//! copy of them as they stood immediately before the port.
//!
//! # What it proves
//!
//! Not "the Rust looks right" — `src/merge_pr/dirty_guard/tests.rs` is for
//! that. This proves the port changed *which lines count as user work* only
//! where it meant to. The port takes two deliberate divergences
//! ([`KNOWN_FILTER_DIVERGENCES`]); everywhere else the two must select the same
//! lines, including on the shapes nobody wrote a unit test for.
//!
//! The classification (#5658) is compared **on a shared input** — the port's
//! own filtered list, fed to both sides — so a filter divergence cannot leak
//! into it and be mistaken for a classification change. It must agree
//! everywhere, with no divergence table at all: which of two advisory sentences
//! prints is not something this slice is licensed to change.
//!
//! # Why the direction of each divergence matters here
//!
//! This guard is the last thing standing between a branch-name collision and
//! `git worktree remove --force`. A line the filter WRONGLY drops is a line
//! whose loss authorizes an irreversible delete of somebody's uncommitted work;
//! a line it wrongly keeps costs one skipped cleanup, which `loom-clean`, the
//! daemon's reaper and the next merge all retry. Divergence A moves a real
//! tracked-file change out of the first category; divergence B moves one of
//! Loom's own breadcrumbs out of the second.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::dirty_guard;

/// `git status --porcelain` blocks, drawn from the shapes a managed worktree
/// actually produces: the clean case, each Loom runtime marker, every porcelain
/// status code the guard can meet, and the boundaries of the retired `[ /]…$`
/// alternation. Not random text, which would exercise "no dirt" on both sides
/// and prove nothing.
const CORPUS: &[&str] = &[
    // --- 0-3: the everyday cases ---
    "",
    " M README.md\n",
    "?? new_module.py\n",
    " M README.md\n?? new_module.py\nA  src/lib.rs\n",
    // --- 4-9: Loom's own runtime markers, alone and together ---
    "?? .loom-managed\n",
    "?? .loom-in-use\n",
    "?? .loom-checkpoint\n",
    "?? .no-changes-needed\n",
    "?? .loom-managed\n?? .loom-in-use\n?? .loom-checkpoint\n?? .no-changes-needed\n",
    " M .loom-managed\n",
    // --- 10-13: the .snapshots WIP directory ---
    "?? .snapshots/\n",
    "?? .snapshots/issue-1-123.patch\n",
    "?? sub/.snapshots/issue-1-123.patch\n",
    "?? .snapshots\n",
    // --- 14-17: markers in a subdirectory, and near-misses ---
    "?? sub/.loom-managed\n",
    "?? x.loom-managed\n",
    "?? a.snapshots/x\n",
    "?? docs/about-loom-managed\n",
    // --- 18-20: markers mixed with real work ---
    "?? .loom-managed\n M README.md\n",
    " M README.md\n?? .snapshots/p.patch\n?? new.py\n",
    "?? .loom-in-use\n?? .loom-managed\n",
    // --- 21-24: renames (divergence class A) ---
    "R  src/app.rs -> .loom-managed\n",
    "R  src/app.rs -> .loom-checkpoint\n",
    "R  old.rs -> new.rs\n",
    "R  src/app.rs -> .loom-managed\n M README.md\n",
    // --- 25-27: git-quoted paths (divergence class B) ---
    "?? \"sub\\303\\251/.loom-managed\"\n",
    "?? \"sub\\303\\251/report.md\"\n",
    "?? \".loom-managed\"\n",
    // --- 28-31: lockfile-shaped artifact churn (#5658) ---
    "A  some-lib-lock.json\n",
    " M Cargo.lock\n",
    " M package-lock.json\n M README.md\n",
    " M pnpm-lock.yaml\n",
    // --- 32-35: blank and whitespace-only lines ---
    "\n",
    "   \n",
    " M README.md\n\n\n?? new.py\n",
    "\t\n",
    // --- 36-39: degenerate / malformed lines ---
    "ab\n",
    "x\n",
    "UU both-modified.rs\n",
    "!! ignored-but-reported.txt\n",
    // --- 40-42: a realistic live-sibling worktree, and the clean one ---
    " M README.md\n?? .loom-managed\n?? .loom-in-use\n?? .snapshots/issue-5001-1.patch\nA  src/feature.rs\n",
    "?? .loom-managed\n?? .snapshots/\n",
    "?? .loom-managed\n M pnpm-lock.yaml\n",
];

/// Where the FILTER deliberately differs from the retired `grep -vE`, and why.
///
/// Keyed by corpus index; the values are both sides' answers in full. The test
/// asserts the two DO differ on every listed index, so this table cannot rot
/// into a list of things that quietly started agreeing again — which is how a
/// divergence table stops being evidence.
///
/// Recognised by MECHANISM, not by a property of the input: each entry names
/// which feature of the retired whole-line regex produced the shell's answer
/// (the `$` anchor reaching past a rename arrow, or the `$` anchor defeated by
/// a closing quote), and the port's answer is asserted exactly rather than
/// merely "different". A port that changed for an unrelated reason — dropping
/// `.snapshots/` filtering, say — would fail here rather than be absorbed as
/// expected.
#[allow(clippy::type_complexity)]
const KNOWN_FILTER_DIVERGENCES: &[(usize, &[&str], &[&str], &str)] = &[
    // (index, shell's kept lines, port's kept lines, class)
    //
    // CLASS A — "rename-swallowed". Porcelain renders a rename as
    // `R  old -> new`, so a rename INTO a marker name ends the line with
    // ` .loom-managed` and `[ /]\.loom-managed$` dropped it. A rename is a
    // tracked-file change — real work — and when it was the ONLY dirt the guard
    // saw a clean worktree and force-removed it. The port tests the path field
    // WITHOUT splitting the arrow, so `old -> .loom-managed` is nobody's marker
    // name. This is the only divergence that changes a REFUSE/REMOVE outcome,
    // and it moves toward refusing.
    (
        21,
        &[],
        &["R  src/app.rs -> .loom-managed"],
        "rename-swallowed: a rename into a marker name is a tracked-file change, not bookkeeping",
    ),
    (
        22,
        &[],
        &["R  src/app.rs -> .loom-checkpoint"],
        "rename-swallowed: every marker name in the alternation had the same hole",
    ),
    (
        24,
        &[" M README.md"],
        &["R  src/app.rs -> .loom-managed", " M README.md"],
        "rename-swallowed: the rename was lost even beside dirt that still refused",
    ),
    //
    // CLASS B — "quote-anchored". `git status` quotes a path containing
    // non-ASCII or control bytes, and the closing quote put the marker name
    // one character away from the `$` anchor — so Loom's own breadcrumb
    // counted as user work and blocked cleanup of that worktree forever. The
    // port strips surrounding quotes before testing, matching
    // `worktree_cli::remove::dirty_lines`. This one moves toward removing, but
    // only for a file that is unambiguously Loom's own bookkeeping.
    (
        25,
        &["?? \"sub\\303\\251/.loom-managed\""],
        &[],
        "quote-anchored: a marker inside a quoted path is still a marker",
    ),
    (
        27,
        &["?? \".loom-managed\""],
        &[],
        "quote-anchored: the same hole with no directory part",
    ),
];

#[allow(clippy::type_complexity)]
fn known_filter_divergence(
    i: usize,
) -> Option<(&'static [&'static str], &'static [&'static str], &'static str)> {
    KNOWN_FILTER_DIVERGENCES
        .iter()
        .find(|(idx, ..)| *idx == i)
        .map(|(_, shell, rust, why)| (*shell, *rust, *why))
}

/// The frozen copy of the retired shell predicates.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-dirty-guard-retired.sh")
}

/// Run one frozen predicate over `input`, returning its raw stdout.
///
/// `set -euo pipefail` deliberately: that is what `merge-pr.sh` runs under, and
/// the retired filter's `|| true` exists precisely because `grep` exiting 1 on
/// "no match" otherwise took the whole merge down. Running the fixture under
/// weaker options would hide that.
///
/// `LC_ALL=C` because POSIX character classes are locale-dependent and `C` is
/// the locale the port models — pinning it here is why this comparison means
/// the same thing on a developer's box and in CI.
fn shell_run(script: &Path, func: &str, input: &str) -> String {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(input.as_bytes()).expect("write corpus entry");
    let tmp_path = tmp.path().to_path_buf();

    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg("set -euo pipefail\nsource \"$2\"\n\"$3\" < \"$1\"\n")
        .arg("bash")
        .arg(&tmp_path)
        .arg(script)
        .arg(func)
        .output()
        .expect("run the frozen retired predicate");

    assert!(
        out.status.success(),
        "{func} must not fail under `set -euo pipefail`; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The retired filter's kept lines.
fn shell_dirty_lines(script: &Path, input: &str) -> Vec<String> {
    let out = shell_run(script, "_retired_dirty_lines", input);
    out.lines().map(std::string::ToString::to_string).collect()
}

/// The retired classification over an already-filtered list.
fn shell_has_real_work(script: &Path, filtered: &[&str]) -> bool {
    let mut joined = filtered.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    let out = shell_run(script, "_retired_has_real_work", &joined);
    match out.trim() {
        "true" => true,
        "false" => false,
        other => panic!("_retired_has_real_work printed {other:?}, expected true/false"),
    }
}

#[test]
fn the_filter_agrees_with_the_shell_except_where_the_port_meant_to_differ() {
    let script = fixture();
    assert!(script.is_file(), "the frozen fixture must exist at {script:?}");

    let mut compared = 0usize;
    let mut diverged = 0usize;

    for (i, input) in CORPUS.iter().enumerate() {
        let shell = shell_dirty_lines(&script, input);
        let shell: Vec<&str> = shell.iter().map(std::string::String::as_str).collect();
        let rust = dirty_guard::user_dirt(input);
        compared += 1;

        match known_filter_divergence(i) {
            Some((want_shell, want_rust, why)) => {
                diverged += 1;
                assert_eq!(
                    shell, want_shell,
                    "corpus[{i}] ({why}): the RETIRED side moved. The fixture is a frozen record \
of behaviour that no longer exists anywhere else — if it changed, the fixture was edited. \
Input: {input:?}"
                );
                assert_eq!(
                    rust, want_rust,
                    "corpus[{i}] ({why}): the PORT's answer is not the recorded one. Input: \
{input:?}"
                );
                assert_ne!(
                    shell, rust,
                    "corpus[{i}] is listed as a divergence but the two sides now AGREE. Remove \
the entry rather than leaving a table that no longer records anything. Input: {input:?}"
                );
            }
            None => assert_eq!(
                shell, rust,
                "corpus[{i}]: undeclared divergence between the retired shell and the port. \
Either it is a bug, or it is a decision that belongs in KNOWN_FILTER_DIVERGENCES with its \
mechanism named. Input: {input:?}"
            ),
        }
    }

    assert_eq!(compared, CORPUS.len(), "every corpus entry must be compared");
    assert_eq!(
        diverged,
        KNOWN_FILTER_DIVERGENCES.len(),
        "every recorded divergence must have been exercised"
    );
}

/// The #5658 classification must agree EVERYWHERE — it is fed the port's own
/// filtered list so the filter divergences above cannot bleed into it.
///
/// No divergence table on purpose: which of two advisory sentences prints is not
/// something a port is licensed to change, and the one gap worth fixing
/// (`pnpm-lock.yaml` matches neither retired glob, corpus 31/42) is left alone
/// here so it stays visible as #5658's own follow-up rather than being smuggled
/// in as "the port's behaviour".
#[test]
fn the_real_work_classification_agrees_with_the_shell_everywhere() {
    let script = fixture();

    for (i, input) in CORPUS.iter().enumerate() {
        let filtered = dirty_guard::user_dirt(input);
        let shell = shell_has_real_work(&script, &filtered);
        let rust = dirty_guard::has_real_work(&filtered);
        assert_eq!(
            shell, rust,
            "corpus[{i}]: the retired classification and the port disagree on whether this dirt \
looks like real work. Filtered: {filtered:?}"
        );
    }
}

/// Size says nothing about reach. A corpus that quietly stopped containing any
/// dirt would make both tests above green while comparing "no dirt" with "no
/// dirt" forty-three times.
#[test]
fn the_corpus_discriminates() {
    let script = fixture();

    let mut shell_refused = 0usize;
    let mut rust_refused = 0usize;
    let mut rust_clean = 0usize;
    let mut real_work = 0usize;
    let mut artifact_only = 0usize;

    for input in CORPUS {
        if !shell_dirty_lines(&script, input).is_empty() {
            shell_refused += 1;
        }
        let filtered = dirty_guard::user_dirt(input);
        if filtered.is_empty() {
            rust_clean += 1;
        } else {
            rust_refused += 1;
            if dirty_guard::has_real_work(&filtered) {
                real_work += 1;
            } else {
                artifact_only += 1;
            }
        }
    }

    assert!(
        shell_refused >= 20,
        "the retired side must actually find user work in much of the corpus, got {shell_refused}"
    );
    assert!(
        rust_refused >= 20,
        "the port must actually find user work in much of the corpus, got {rust_refused}"
    );
    assert!(
        rust_clean >= 8,
        "the corpus must also exercise the CLEAN path — the guard is surgical, not a blanket \
refuse — got {rust_clean}"
    );
    assert!(
        real_work >= 10,
        "the corpus must exercise the cross-host-dispatch hypothesis, got {real_work}"
    );
    assert!(
        artifact_only >= 2,
        "the corpus must exercise #5658's artifact-churn suppression, got {artifact_only}"
    );
}

/// The whole decision, not just the parse: an empty filtered set must produce
/// no refusal, a non-empty one must always produce a refusal that quotes every
/// kept line and ends with the pasteable remediation command.
#[test]
fn the_refusal_is_produced_exactly_when_the_filter_kept_something() {
    for (i, input) in CORPUS.iter().enumerate() {
        let filtered = dirty_guard::user_dirt(input);
        let ctx = dirty_guard::Context {
            worktree_path: "/wt",
            repo_root: "/repo",
            branch: "feature/issue-1",
        };
        let verdict = dirty_guard::assess(&ctx, input);

        if filtered.is_empty() {
            assert!(
                verdict.is_none(),
                "corpus[{i}]: nothing was kept, so the removal must be allowed. Input: {input:?}"
            );
            continue;
        }

        let records = verdict.unwrap_or_else(|| panic!("corpus[{i}] must refuse"));
        let texts: Vec<&str> = records.iter().map(|(_, m)| m.as_str()).collect();
        assert!(
            texts[0].contains("Refusing to remove worktree at /wt")
                && texts[0].contains("data-loss guard, #5031"),
            "corpus[{i}]: the refusal must lead with the #5031 message. Got {:?}",
            texts[0]
        );
        for line in &filtered {
            assert!(
                texts.contains(line),
                "corpus[{i}]: kept line {line:?} must be quoted verbatim in the refusal"
            );
        }
        let (level, last) = records.last().expect("at least one record");
        assert_eq!(
            *level,
            dirty_guard::Level::Plain,
            "corpus[{i}]: the remediation command must be uncolored so it pastes cleanly"
        );
        assert_eq!(
            last, "  git -C \"/repo\" worktree remove \"/wt\" --force",
            "corpus[{i}]: the remediation command is what an operator runs; it must be exact"
        );
    }
}
