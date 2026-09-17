//! Aggregate ratchet on production shell volume (#8084, epic #7810).
//!
//! # Why
//!
//! Epic #7810 ported four scripts and deleted roughly 2,200 lines of production
//! shell. Over the same window the repo's total production shell went UP:
//!
//! | when | files | production shell code lines |
//! |---|---|---|
//! | 90 days ago | 93 | 14,958 |
//! | 60 days ago | 96 | 15,568 |
//! | 30 days ago | 186 | 40,735 |
//! | now | 234 | 51,814 |
//!
//! Immediately before the epic's first port commit the number was 51,749. It is
//! now 51,814 — the epic is a net **+65**. Of the 98 files appearing between the
//! 60- and 30-day marks, 96 did not exist anywhere in the tree at 60 days, so
//! this is new code, not a relocation.
//!
//! Neither existing gate sees it. `check-file-size-budget.sh` ratchets
//! INDIVIDUAL files already over 1,000 lines; `check-shell-allowlist.sh` wants a
//! category and reason for each NEW script. But 29,795 of those lines arrived as
//! 148 brand-new scripts, nearly all under the per-file threshold and each with
//! a defensible reason. `check-file-size-budget.sh`'s own header names the
//! failure mode: "every individual addition is defensible while the aggregate is
//! the problem." That is true of shell volume, and nothing measured it.
//!
//! # What this is not
//!
//! Not a ban, and not a per-file limit. Growth stays possible — it just has to
//! be spelled out as a changed number in `scripts/shell-budget-baseline.txt`,
//! visible in the diff, instead of arriving invisibly across 148 files. Same
//! bargain the file-size ratchet strikes.
//!
//! # Why this is Rust and not a shell script
//!
//! A `.sh` enforcing "stop adding shell" would have to exempt itself from its
//! own count, and the language policy (`.loom/docs/shell-language-policy.md`)
//! already says new executable logic belongs in the daemon. It lives as an
//! integration test so it runs under the existing `cargo nextest run
//! --workspace` with no new CI job.
//!
//! # Updating the baseline
//!
//! ```text
//! UPDATE_SHELL_BUDGET=1 cargo test -p loom-daemon --test shell_budget_ratchet
//! ```
//!
//! Legitimate for recording shrinkage (the ratchet tightens) or for a reviewed
//! decision to admit growth. A reviewer should treat an update that RAISES the
//! number as the thing to ask about — that is the ratchet slipping, which is the
//! whole point of the gate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Repo root: this crate's manifest dir is `<root>/loom-daemon`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon must have a parent directory")
        .to_path_buf()
}

fn baseline_path() -> PathBuf {
    repo_root().join("scripts/shell-budget-baseline.txt")
}

/// Whether a tracked `.sh` path counts as PRODUCTION shell.
///
/// Three exclusions, each with a reason:
///
/// - **`tests/` path segments and `test-*.sh` basenames.** The retained
///   black-box suite method this epic runs on *requires* test shell to grow as
///   production shell shrinks — a port keeps the old suite and runs its
///   assertions against the Rust. Counting tests here would make the correct
///   move look like a regression and the gate would be gamed or deleted.
/// - **`.loom/`.** Installed mirrors of `defaults/`. Counting both double-counts
///   the same source, most visibly the 3,808-line vendored guard.
/// - Nothing else. Vendored shell still counts: it is shell we ship and it is
///   part of the volume an agent has to work around, whoever wrote it.
fn is_production_shell(path: &str) -> bool {
    if path.starts_with(".loom/") {
        return false;
    }
    if path.split('/').any(|seg| seg == "tests" || seg == "test") {
        return false;
    }
    let basename = path.rsplit('/').next().unwrap_or(path);
    if basename.starts_with("test-") {
        return false;
    }
    true
}

/// Code lines: leading whitespace stripped, blanks skipped, `#`-leading lines
/// skipped. Byte-for-byte the rule `check-file-size-budget.sh` applies to `.sh`
/// (its `measure_all` awk pass), so the two gates never disagree about a file.
fn code_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#')
        })
        .count()
}

fn tracked_shell_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.sh"])
        .output()
        .unwrap_or_else(|e| panic!("could not run `git ls-files` in {}: {e}", root.display()));

    assert!(
        out.status.success(),
        "`git ls-files` failed in {}: {}",
        root.display(),
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

struct Measurement {
    total: usize,
    files: usize,
    per_file: BTreeMap<String, usize>,
}

fn measure() -> Measurement {
    let root = repo_root();
    let tracked = tracked_shell_files(&root);

    // A discovery failure must not read as "no shell left". Without this, a
    // broken pathspec or a non-git checkout silently passes the gate at 0.
    assert!(
        tracked.len() >= 400,
        "expected 400+ tracked .sh files, found {} — shell discovery is broken, and a gate \
         that measures nothing passes for the wrong reason",
        tracked.len()
    );

    let mut per_file = BTreeMap::new();
    let mut total = 0usize;
    for rel in tracked.iter().filter(|p| is_production_shell(p)) {
        let full = root.join(rel);
        let Ok(text) = std::fs::read_to_string(&full) else {
            // A tracked path that will not read as UTF-8 text is not something
            // to skip quietly; say so and keep it out of the count explicitly.
            panic!("tracked shell file {rel} could not be read as text");
        };
        let n = code_lines(&text);
        total += n;
        per_file.insert(rel.clone(), n);
    }

    assert!(
        per_file.len() >= 150,
        "only {} production shell files survived the filter — the exclusion rule is too broad",
        per_file.len()
    );

    Measurement {
        total,
        files: per_file.len(),
        per_file,
    }
}

fn read_baseline() -> (usize, usize) {
    let path = baseline_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline not found at {}: {e}", path.display()));
    let mut lines = None;
    let mut files = None;
    for l in text.lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let mut parts = l.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("lines"), Some(v)) => lines = v.parse().ok(),
            (Some("files"), Some(v)) => files = v.parse().ok(),
            _ => panic!("unrecognised baseline line: {l:?}"),
        }
    }
    (
        lines.expect("baseline is missing a `lines <N>` entry"),
        files.expect("baseline is missing a `files <N>` entry"),
    )
}

fn write_baseline(m: &Measurement) {
    let body = format!(
        "# shell-budget-baseline.txt — the aggregate production-shell ratchet.\n\
         #\n\
         # Regenerate deliberately:\n\
         #   UPDATE_SHELL_BUDGET=1 cargo test -p loom-daemon --test shell_budget_ratchet\n\
         #\n\
         # `lines` may go DOWN freely and must not go UP. See\n\
         # loom-daemon/tests/shell_budget_ratchet.rs for the scope rule (what counts\n\
         # as production shell) and for why raising this number is the thing a\n\
         # reviewer should ask about.\n\
         #\n\
         # `files` is a companion sanity value, not a second ratchet: it exists so a\n\
         # filter regression that silently stops counting whole directories shows up\n\
         # as a suspicious drop rather than as a free win.\n\
         lines {}\n\
         files {}\n",
        m.total, m.files
    );
    std::fs::write(baseline_path(), body).expect("could not write baseline");
}

#[test]
fn production_shell_does_not_grow() {
    let m = measure();

    if std::env::var_os("UPDATE_SHELL_BUDGET").is_some() {
        write_baseline(&m);
        eprintln!("shell-budget-baseline.txt updated: lines {} files {}", m.total, m.files);
        return;
    }

    let (baseline_lines, baseline_files) = read_baseline();

    println!(
        "production shell: {} code lines across {} files (baseline {} / {})",
        m.total, m.files, baseline_lines, baseline_files
    );

    if m.total > baseline_lines {
        let mut largest: Vec<_> = m.per_file.iter().collect();
        largest.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let top = largest
            .iter()
            .take(10)
            .map(|(p, n)| format!("  {n:>6}  {p}"))
            .collect::<Vec<_>>()
            .join("\n");

        panic!(
            "production shell grew by {} code lines ({} -> {}), across {} files (baseline {}).\n\
             \n\
             Epic #7810 is retiring shell. Adding to the aggregate works against that, so the\n\
             gate asks for the growth to be deliberate rather than invisible. Options, best\n\
             first:\n\
             \n\
               1. Put the new logic in the daemon instead — that is the language policy\n\
                  (.loom/docs/shell-language-policy.md) and it makes this gate a non-event.\n\
               2. Remove shell elsewhere to pay for it.\n\
               3. If the growth is genuinely right, record it:\n\
                    UPDATE_SHELL_BUDGET=1 cargo test -p loom-daemon --test shell_budget_ratchet\n\
                  and say WHY in the commit. A reviewer will see the raised number.\n\
             \n\
             Largest production scripts right now:\n{}",
            m.total - baseline_lines,
            baseline_lines,
            m.total,
            m.files,
            baseline_files,
            top
        );
    }
}

/// The counting rule itself, so a refactor of `code_lines` cannot drift from
/// `check-file-size-budget.sh` unnoticed.
#[test]
fn code_lines_counts_what_the_file_size_ratchet_counts() {
    assert_eq!(code_lines(""), 0);
    assert_eq!(code_lines("\n\n   \n\t\n"), 0, "blank lines never count");
    assert_eq!(
        code_lines("#!/usr/bin/env bash\n# a comment\n"),
        0,
        "comment-only lines never count"
    );
    assert_eq!(code_lines("   # indented comment\n"), 0, "leading whitespace is stripped first");
    assert_eq!(code_lines("echo hi\n"), 1);
    assert_eq!(code_lines("  echo hi   # trailing comment\n"), 1, "a trailing comment is code");
    assert_eq!(
        code_lines("#!/usr/bin/env bash\n\nset -e\n# note\nfoo() {\n  bar\n}\n"),
        4,
        "shebang and comment excluded; the four body lines counted"
    );
}

/// The scope rule, stated as assertions rather than left to a glob.
#[test]
fn production_scope_rule_is_explicit() {
    assert!(is_production_shell("defaults/scripts/merge-pr.sh"));
    assert!(is_production_shell("scripts/install-loom.sh"));
    assert!(is_production_shell("install.sh"));
    assert!(
        is_production_shell("defaults/hooks/guard-destructive-generic.sh"),
        "vendored shell still ships and still counts"
    );

    assert!(!is_production_shell("defaults/scripts/tests/test-merge-pr.sh"));
    assert!(!is_production_shell("tests/install/test-provision-daemon.sh"));
    assert!(!is_production_shell("scripts/test-installer.sh"), "test-* basename anywhere");
    assert!(
        !is_production_shell(".loom/hooks/guard-destructive-generic.sh"),
        "installed mirror"
    );

    // Not fooled by a substring: `latest/` is not `tests/`, `testable.sh` is
    // not `test-*.sh`.
    assert!(is_production_shell("defaults/scripts/latest/thing.sh"));
    assert!(is_production_shell("defaults/scripts/testable.sh"));
}
