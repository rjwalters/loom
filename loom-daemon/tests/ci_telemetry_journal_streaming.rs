//! Tree invariant (#11045): no code path loads the CI telemetry journal
//! whole.
//!
//! `.loom/logs/ci-telemetry.jsonl` reached 5.4 GB on the fleet captain, and
//! startup read it into memory several times at once (`read_to_end` in the
//! torn-tail repair, `std::fs::read` in the export backfill and in the
//! crash-recovery replay) until the daemon was OOM-killed in a loop. The fix
//! streams every read; this scan keeps a whole-file read from coming back.
//!
//! # What is scanned
//!
//! - **The journal's own modules** (`journal.rs`, `export.rs`, `ledger.rs`,
//!   `rotation.rs` under `src/ci_telemetry/`): no whole-file read of any
//!   kind — `read_to_end`, `read_to_string`, `std::fs::read(` or
//!   `fs::read(`. These modules only ever stream, seek, or read a bounded
//!   tail block, so there is no legitimate exception to allow.
//! - **Every other production file under `src/ci_telemetry/`**, plus the
//!   observability backfill (`src/observability/backfill.rs`) that drives
//!   the export: no whole-file read whose argument names the journal
//!   (`journal_path`), and no call to the retired whole-journal helpers
//!   (`.identities()`, `read_repaired_lines(`/`read_complete_lines(` on the
//!   journal). They may read their own small files (status, caches, the
//!   sweep-outcome journal) as before.
//!
//! Test code (`src/ci_telemetry/tests*`, `#[cfg(test)]` blocks) is exempt —
//! a test may read a small fixture journal whole to assert on it.
//!
//! # Why a Rust test and not a `scripts/check-*.sh`
//!
//! Same reason as `gh_spawn_choke_point.rs`: the shell-language policy
//! (ADR-0018) admits no new lint script, and a `#[test]` here already runs
//! in CI.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

const JOURNAL_MODULES: &[&str] = &[
    "src/ci_telemetry/journal.rs",
    "src/ci_telemetry/export.rs",
    "src/ci_telemetry/ledger.rs",
    "src/ci_telemetry/rotation.rs",
];

const BACKFILL: &str = "src/observability/backfill.rs";

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn whole_read() -> Regex {
    Regex::new(r"\bread_to_end\s*\(|\bread_to_string\s*\(|\bfs::read\s*\(").expect("valid regex")
}

fn journal_whole_read() -> Regex {
    Regex::new(
        r"(?:\bread_to_end|\bread_to_string|\bfs::read|\bread_repaired_lines|\bread_complete_lines)\s*\([^;]*journal_path|\.identities\s*\(\s*\)",
    )
    .expect("valid regex")
}

/// The production lines of `path`: comments dropped, and the item after each
/// `#[cfg(test)]` skipped — a braced item (`mod tests { … }`,
/// `thread_local! { … }`) through its closing brace, anything else for one
/// line. Brace counting ignores string contents; good enough for a scan.
fn production_lines(path: &Path) -> Vec<(usize, String)> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = Vec::new();
    let mut skip_next = false;
    let mut depth: i64 = 0;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if depth > 0 {
            depth += brace_delta(line);
            continue;
        }
        if trimmed.starts_with("#[cfg(test)]") {
            skip_next = true;
            continue;
        }
        if skip_next {
            skip_next = false;
            depth = brace_delta(line).max(0);
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        let code = line.split(" // ").next().unwrap_or(line);
        lines.push((index + 1, code.to_string()));
    }
    lines
}

fn brace_delta(line: &str) -> i64 {
    let opens = line.matches('{').count();
    let closes = line.matches('}').count();
    i64::try_from(opens).unwrap() - i64::try_from(closes).unwrap()
}

fn production_files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "tests" || name == "tests.rs" {
            continue;
        }
        if path.is_dir() {
            production_files_under(&path, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

fn violations(files: &[PathBuf], pattern: &Regex) -> Vec<String> {
    let root = crate_root();
    let mut found = Vec::new();
    for file in files {
        for (number, line) in production_lines(file) {
            if pattern.is_match(&line) {
                let shown = file.strip_prefix(&root).unwrap_or(file).display();
                found.push(format!("{shown}:{number}: {}", line.trim()));
            }
        }
    }
    found
}

#[test]
fn the_journal_modules_never_read_a_whole_file() {
    let files: Vec<PathBuf> = JOURNAL_MODULES
        .iter()
        .map(|f| crate_root().join(f))
        .collect();
    for file in &files {
        assert!(file.exists(), "scanned module moved: {}", file.display());
    }
    let found = violations(&files, &whole_read());
    assert!(
        found.is_empty(),
        "#11045: the CI telemetry journal modules must stream (seek + bounded line reads), \
         never load a file whole:\n{}",
        found.join("\n")
    );
}

#[test]
fn nothing_else_reads_the_journal_whole() {
    let mut files = Vec::new();
    production_files_under(&crate_root().join("src/ci_telemetry"), &mut files);
    files.push(crate_root().join(BACKFILL));
    assert!(files.len() > JOURNAL_MODULES.len(), "the scan found the module tree");
    let found = violations(&files, &journal_whole_read());
    assert!(
        found.is_empty(),
        "#11045: the CI telemetry journal must never be read whole — use \
         `Journal::for_each`/`Journal::missing` or `ledger::scan_lines`:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_scan_patterns_catch_what_they_forbid() {
    let whole = whole_read();
    for bad in [
        "    file.read_to_end(&mut bytes)?;",
        "let Ok(bytes) = std::fs::read(journal_path(root)) else {",
        "let text = std::fs::read_to_string(path)?;",
    ] {
        assert!(whole.is_match(bad), "missed: {bad}");
    }
    assert!(!whole.is_match("reader.read_until(b'\\n', &mut line)?;"));

    let journal = journal_whole_read();
    for bad in [
        "let Ok(bytes) = std::fs::read(journal_path(root)) else {",
        "let text = std::fs::read_to_string(super::journal_path(&root))?;",
        "let present = journal.identities()?;",
        "read_complete_lines(&journal_path(root))",
    ] {
        assert!(journal.is_match(bad), "missed: {bad}");
    }
    assert!(!journal.is_match("let text = std::fs::read_to_string(path)?;"));
    assert!(!journal.is_match("let journal = Journal::open(journal_path(ctx.root))?;"));
}
