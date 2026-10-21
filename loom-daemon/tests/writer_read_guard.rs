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
//! 3. The in-process daemon read helpers (#12555) —
//!    `claim_reconciliation::gh_call::read` and
//!    `SweepRegistry::gh_inv(.., AccessIntent::Read, ..)` — build
//!    `GhInvocation::reader_only()` reads, which `writer_identity()` cannot
//!    pin and which are shed rather than sent to the writer when no reader
//!    route exists. The writer-pinned "read own write" helpers are gone.
//!    The behaviour is unit-tested in `gh_invocation/reader_only_tests.rs`;
//!    this rule keeps a refactor from dropping the marker.
//!
//! 4. **Every** read is reader-only, whatever builder made it (#12584):
//!    `GhInvocation::is_reader_only()` holds for any forge read its site did
//!    not pin with `writer_identity()` — a bare `GhInvocation::new`, an
//!    `Operation::new` site, a `.inv(..)` / `.invocation(.., cred)` helper —
//!    and a credential a site hands such a read (`gh_config_dir`,
//!    `without_token_env`, a writer `identity_role`) is set aside so the
//!    read is routed to a reader or shed; it is restored only on a host with
//!    no reader pool. This file asserts the public builder answers and the
//!    wiring that applies them; `gh_invocation/reader_only_tests.rs` and
//!    `fleet_store/gh_reader_tests.rs` run them against a stub `gh`.
//!
//! The runtime half is `gh_invocation::accounting::is_unmarked_read_on_writer`,
//! which logs an error for any read that reaches the writer unmarked.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const ASKER: &str = "asker-dependent probe (the answer depends on WHO asks)";
const WRITE: &str = "write-intent call (not a read)";
const NOPOOL: &str = "host with no reader pool: the writer is the only credential";
const PLUMBING: &str = "names the role (defines or inspects it); sends no read";
const MERGE: &str = "merge settings hidden from a reader App; the merging writer reads them";
const PROTECTION: &str = "classic branch protection (GraphQL) is asker-dependent; a reader that \
                          cannot see it would answer 'no required contexts' and fail the merge \
                          guard open";

/// `(file relative to loom-daemon/src, reason)` pairs allowed to pin a read
/// to the writer. Only ever remove entries.
const READ_ON_WRITER: &[(&str, &str)] = &[
    ("branch_update.rs", MERGE),
    ("cli/lease_release.rs", WRITE),
    ("credential_preflight/pool.rs", ASKER),
    ("fleet_store/gh.rs", NOPOOL),
    ("forge_bucket_book/probe.rs", ASKER),
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
    ("merge_pr/stale_checks/fetch.rs", PROTECTION),
    ("observability/ops/ratelimit.rs", ASKER),
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

/// `(file relative to loom-daemon/src, builder)`: the in-process daemon read
/// builders, each of which must mark its reads reader-only (#12555).
const READER_ONLY_BUILDERS: &[(&str, &str)] = &[
    ("claim_reconciliation/gh_call.rs", "fn build("),
    ("sweep_registry/guards/gh_exec.rs", "fn gh_inv("),
];

/// Writer-pinned read helpers retired by #12555 and #12584.
const RETIRED_READ_ON_WRITER_HELPERS: &[&str] = &[
    "fn read_own_write",
    "fn gh_read_own_write",
    "fn get_as_writer",
    "reader_failed",
];

#[test]
fn the_daemon_read_builders_are_reader_only() {
    for (file, builder) in READER_ONLY_BUILDERS {
        let code = code_lines(&src_root().join(file)).join("\n");
        let at = code.find(builder).unwrap_or_else(|| {
            panic!("{file}: `{builder}` not found; update READER_ONLY_BUILDERS")
        });
        let body: String = code[at..].chars().take(1200).collect();
        assert!(
            body.contains("AccessIntent::Read") && body.contains(".reader_only()"),
            "{file}: `{builder}` must mark a read `.reader_only()` (#12555: an in-process daemon \
             read never mints the writer)"
        );
    }
    let mut files = Vec::new();
    rust_files(&src_root(), &mut files);
    let mut hits = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(src_root()).unwrap().display().to_string();
        for line in code_lines(f) {
            for name in RETIRED_READ_ON_WRITER_HELPERS {
                if line.contains(name) {
                    hits.push(format!("{rel}: {name}"));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "a writer-pinned read-own-write helper came back (#12555): {hits:?}"
    );
}

/// #12584: whatever builder made it, a read answers `is_reader_only()` unless
/// its site pinned the writer; a credential handed to it does not exempt it.
#[test]
fn every_read_builder_is_reader_only_unless_the_writer_is_pinned() {
    use loom_daemon::forge_identity::IdentityRole;
    use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    use std::time::Duration;
    let dir = std::env::temp_dir();
    let read = |op: &'static str| {
        GhInvocation::new(
            Operation::new(op),
            AccessIntent::Read,
            GhTarget::None,
            Duration::from_secs(1),
        )
        .args(["api", "repos/{owner}/{repo}/issues/1/comments"])
    };
    // Every builder shape a site uses for a read.
    let reads = [
        ("bare", read("t.bare")),
        ("explicit config dir", read("t.config").gh_config_dir(Some(&dir))),
        ("no token env", read("t.strip").without_token_env()),
        ("writer role", read("t.role").identity_role(IdentityRole::Writer)),
        (
            "writer credential, all of it",
            read("t.all")
                .gh_config_dir(Some(&dir))
                .without_token_env()
                .identity_role(IdentityRole::Writer),
        ),
        ("targeted", {
            GhInvocation::new(
                Operation::new("t.targeted"),
                AccessIntent::Read,
                GhTarget::repo("acme/widget").unwrap(),
                Duration::from_secs(1),
            )
            .args(["api", "repos/acme/widget/pulls/1"])
        }),
        (
            "graphql",
            read("t.graphql")
                .args(["graphql"])
                .reader_repo("acme/widget"),
        ),
        (
            "explicit reader_only beats a writer pin",
            read("t.both").writer_identity().reader_only(),
        ),
        (
            "explicit reader_only, pin after",
            read("t.both2").reader_only().writer_identity(),
        ),
    ];
    for (what, inv) in reads {
        assert!(
            inv.is_reader_only(),
            "{what}: a read must be reader-only unless the writer is pinned (#12394, #12584)"
        );
    }
    // The only exits: a listed writer pin, a write, and argv that sends no
    // forge read (a local command, the free `rate_limit` bucket reading).
    assert!(!read("t.pinned").writer_identity().is_reader_only());
    assert!(!GhInvocation::new(
        Operation::new("t.write"),
        AccessIntent::Write,
        GhTarget::None,
        Duration::from_secs(1)
    )
    .args(["api", "-X", "POST", "repos/acme/widget/issues/1/comments"])
    .is_reader_only());
    for local in [["--version"].as_slice(), ["auth", "status"].as_slice()] {
        let inv = GhInvocation::new(
            Operation::new("t.local"),
            AccessIntent::Read,
            GhTarget::None,
            Duration::from_secs(1),
        )
        .args(local);
        assert!(!inv.is_reader_only(), "{local:?} sends no forge read");
    }
    let bucket = GhInvocation::new(
        Operation::new("t.bucket"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(1),
    )
    .args(["api", "--include", "rate_limit"]);
    assert!(!bucket.is_reader_only(), "a bucket reading is the asker's own");
}

/// #12584: the wiring that makes rule 4 hold at execution: the route step
/// sets a site's writer credential aside, the reader-only test is not
/// limited to the explicit marker, and the no-pool path is the only one that
/// restores the credential.
#[test]
fn a_site_chosen_credential_is_routed_and_only_restored_without_a_pool() {
    let gh = |f: &str| code_lines(&src_root().join("gh_invocation").join(f)).join("\n");
    let route = gh("cwd_route.rs");
    let at = route
        .find("fn with_derived_route_in(")
        .expect("cwd_route.rs: with_derived_route_in");
    let body: String = route[at..].chars().take(600).collect();
    assert!(
        body.contains(".route_site_credential()"),
        "every execution must set a site's writer credential aside before routing (#12584)"
    );
    let chain = gh("reader_route_v2.rs");
    assert!(
        chain.contains("self.is_reader_only()"),
        "the unroutable-read shed must apply to every reader-only read, not only the marker"
    );
    assert_eq!(
        chain.matches("restore_site_credential()").count(),
        1,
        "only the no-reader-pool path may restore a site's credential"
    );
    let at = chain
        .find("fn writer_only_read(")
        .expect("reader_route_v2.rs: writer_only_read");
    let body: String = chain[at..].chars().take(400).collect();
    assert!(body.contains("restore_site_credential()"));
    // Every invocation comes from `GhInvocation::new` (its fields are
    // private to `gh_invocation`), so the rules above see every read.
}
