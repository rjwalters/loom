//! Structural guard: the writer App is never used for reads (#12394).
//!
//! Operator ruling (2026-10-16): "the writer should not be used for reads."
//! The writer installation has a fixed hourly budget the fleet's claims,
//! merges and label flips spend; reads that spilled onto it tripped
//! `rate_limit_breaker` on every worker at once.
//!
//! Two source-level rules over non-test daemon source:
//!
//! 1. The reader → writer fallback does not exist: nothing may name the
//!    retired `IdentityRole::WriterFallback`, `reader_then_writer`,
//!    `writer_fallback` or the escalated-tick `allow_writer`, and the
//!    `LOOM_READ_SHED` kill switch (which restored the fallback) is gone.
//! 2. A read reaches the writer only by an explicit
//!    `GhInvocation::writer_identity()` (or `IdentityRole::Writer`) marker,
//!    and every file carrying one is listed in [`READ_ON_WRITER`] with the
//!    reason. A new site fails here until someone writes down why a read
//!    needs the writer; a listed file that no longer carries a marker fails
//!    too (a ratchet: the list only ever gets shorter).
//!
//! The runtime half is `gh_invocation::accounting::is_unmarked_read_on_writer`,
//! which logs an error for any read that reaches the writer unmarked.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const BASE: &str = "read-back of the daemon's own write (a reader may lag it)";
const ASKER: &str = "asker-dependent probe (the answer depends on WHO asks)";
const WRITE: &str = "write-intent call (not a read)";
const NOPOOL: &str = "host with no reader pool: the writer is the only credential";
const PLUMBING: &str = "names the role (defines or inspects it); sends no read";
const MERGE: &str = "merge settings hidden from a reader App; the merging writer reads them";

/// `(file relative to loom-daemon/src, reason)` pairs allowed to pin a read
/// to the writer. Only ever remove entries.
const READ_ON_WRITER: &[(&str, &str)] = &[
    ("branch_update.rs", MERGE),
    ("claim_reconciliation/gh_call.rs", BASE),
    (
        "cli/duplicate_closed_search.rs",
        "FOLLOW-UP: an untargeted search pinned to the writer; no reader route yet",
    ),
    ("cli/lease_release.rs", WRITE),
    ("cli/pr_lease.rs", BASE),
    ("credential_preflight/pool.rs", ASKER),
    ("forge_bucket_book/probe.rs", ASKER),
    ("forge_check_claim.rs", BASE),
    ("forge_comment.rs", BASE),
    ("forge_etag_store/deferrable.rs", NOPOOL),
    ("forge_identity.rs", PLUMBING),
    ("forge_merge_config.rs", MERGE),
    ("forge_merge_method.rs", MERGE),
    ("forge_repo_facts/crosscheck.rs", ASKER),
    ("forge_repo_facts/installation.rs", ASKER),
    ("forge_rerun.rs", WRITE),
    ("gh_invocation/accounting.rs", PLUMBING),
    ("gh_invocation/reader_route.rs", PLUMBING),
    ("gh_invocation/reader_route_v2.rs", NOPOOL),
    ("merge_group_ci/eligibility.rs", ASKER),
    ("merge_pr/head_sync/fetch.rs", BASE),
    ("observability/ops/ratelimit.rs", ASKER),
    ("sweep_registry/guards.rs", BASE),
    ("sweep_registry/guards/gh_exec.rs", BASE),
    ("write_scope/probe.rs", ASKER),
];

/// Names that must not appear in non-test source at all.
const RETIRED: &[&str] = &[
    "WriterFallback",
    "reader_then_writer",
    "writer_fallback",
    "allow_writer",
    "LOOM_READ_SHED",
    "READ_SHED_ENV",
];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && !path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with("_tests") || s.ends_with("_test") || s == "tests")
        {
            out.push(path);
        }
    }
}

/// Code lines of `path`: comments dropped, and `#[cfg(test)] mod … { … }`
/// blocks skipped (a lone `#[cfg(test)]` item is kept).
fn code_lines(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).expect("read source");
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == "#[cfg(test)]" {
            let indent = &line[..line.len() - line.trim_start().len()];
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim_start().starts_with("#[") {
                j += 1;
            }
            let is_mod = lines.get(j).is_some_and(|l| {
                let t = l.trim_start();
                t.starts_with("mod ")
                    || t.starts_with("pub mod ")
                    || t.starts_with("pub(crate) mod ")
            });
            if is_mod {
                if lines[j].trim_end().ends_with(';') {
                    i = j + 1;
                    continue;
                }
                let close = format!("{indent}}}");
                if let Some(end) = (j + 1..lines.len()).find(|&k| lines[k].trim_end() == close) {
                    i = end + 1;
                    continue;
                }
            }
        }
        if !line.trim_start().starts_with("//") {
            out.push(line.to_string());
        }
        i += 1;
    }
    out
}

#[test]
fn the_reader_to_writer_fallback_is_gone() {
    let mut files = Vec::new();
    rust_files(&src_root(), &mut files);
    let mut hits = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(src_root()).unwrap().display().to_string();
        // This guard's own names live in the doc/comment text only.
        for (n, line) in code_lines(f).iter().enumerate() {
            for name in RETIRED {
                if line.contains(name) {
                    hits.push(format!("{rel}:{}: {name}", n + 1));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "reads must never fall back to the writer (#12394); retired names found:\n{}",
        hits.join("\n")
    );
}

#[test]
fn a_read_is_pinned_to_the_writer_only_at_a_listed_site() {
    let mut files = Vec::new();
    rust_files(&src_root(), &mut files);
    let allowed: BTreeSet<&str> = READ_ON_WRITER.iter().map(|(f, _)| *f).collect();
    let mut found = BTreeSet::new();
    for f in &files {
        let rel = f.strip_prefix(src_root()).unwrap().display().to_string();
        let marked = code_lines(f)
            .iter()
            .any(|l| l.contains(".writer_identity()") || l.contains("IdentityRole::Writer"));
        if marked {
            found.insert(rel);
        }
    }
    let unlisted: Vec<&String> = found
        .iter()
        .filter(|f| !allowed.contains(f.as_str()))
        .collect();
    assert!(
        unlisted.is_empty(),
        "a read is pinned to the writer in a file not listed in READ_ON_WRITER (#12394: the \
         writer is never used for reads; list the file with the reason it needs the writer, \
         or route the read to a reader): {unlisted:?}"
    );
    let stale: Vec<&&str> = allowed.iter().filter(|f| !found.contains(**f)).collect();
    assert!(
        stale.is_empty(),
        "READ_ON_WRITER lists files with no writer marker left; remove them (ratchet): {stale:?}"
    );
}
