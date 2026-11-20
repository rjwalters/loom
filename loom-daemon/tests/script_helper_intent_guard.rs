//! Structural guard (#13744): a `gh` call through the script-helper family
//! cannot be a raw spawn, and its **declared** intent must match what its
//! argv provably is.
//!
//! The raw entry points (`run_gh`, `gh_query`) are gone: they spawned `gh`
//! with the ambient credential — no access intent, no reader routing, no
//! shed, no `invoke github` span — which is how helper reads landed on the
//! writer and exhausted its budget ~35 minutes of every hour (2026-11-10).
//! Their replacements ([`loom_daemon::script_helpers::gh_read`],
//! `gh_read_json`, `gh_write`) declare the intent and route through the
//! facade. Two rules keep that true:
//!
//! 1. **The retired names may not come back** anywhere in non-test daemon
//!    source — not as a call, not as a re-added helper. A caller cannot use
//!    a raw entry point that does not exist.
//! 2. **A declared intent must match a provably-shaped argv.** The runtime
//!    guard (`script_helpers::gh_route::argv_access`) refuses a mislabelled
//!    call before anything is spawned, however the argv was assembled; this
//!    lint is the review-time half — it reads the call's **literal** argv
//!    and fails CI on a contradiction, in every non-test source file (a new
//!    file and an already-inventoried one alike).
//!
//! What the lint can prove from literals mirrors the runtime classifier:
//! a REST `gh api` without a write method (`GET`, no `-f` fields) is a read;
//! `… view` / `… list` / `pr diff|checks|status` are reads; a GraphQL
//! document beginning `query`/`{` is a read **even though `gh api graphql`
//! POSTs it** — the HTTP method alone does not identify a mutation;
//! `-X POST/PUT/PATCH/DELETE`, request fields, and the mutating subcommands
//! are writes. A call whose literals prove neither side is left to its
//! declaration; a wrapper that hides its argv behind a parameter is caught
//! by the runtime guard the first time it runs.
//!
//! The one sanctioned read on the writer — an asker-dependent probe built
//! directly on the facade with `GhInvocation::writer_identity()`, listed in
//! `tests/writer_read_guard.rs` — does not go through this family and passes
//! untouched.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Names the raw helper must never answer to again (#13744).
const RETIRED: &[&str] = &["run_gh(", "gh_query(", "fn run_gh(", "fn gh_query("];

/// The typed entry points whose declarations this lint checks.
const ENTRY_POINTS: &[&str] = &["gh_read_for_repo", "gh_read_json", "gh_read", "gh_write"];

/// Porcelain verbs that fetch state.
const READ_VERBS: &[&str] = &["view", "list", "diff", "checks", "status", "download"];
/// Porcelain verbs that mutate it.
const WRITE_VERBS: &[&str] = &[
    "edit",
    "comment",
    "create",
    "close",
    "reopen",
    "merge",
    "ready",
    "lock",
    "unlock",
    "pin",
    "unpin",
    "review",
    "delete",
    "rename",
    "archive",
    "unarchive",
    "upload",
    "transfer",
];
/// HTTP methods that mutate (case-normalised before matching).
const WRITE_METHODS: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display())) {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn is_test_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.ends_with("tests.rs")
        || name.contains("_tests")
        || rel.contains("/tests/")
        || rel.starts_with("tests/")
        || rel.contains("test_support")
}

/// Code lines of `text`: `//` comments dropped and `#[cfg(test)] mod … { … }`
/// blocks skipped (the same two rules `tests/writer_read_guard.rs` applies).
fn code_lines(text: &str) -> Vec<String> {
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

/// One string literal (unescaped body) and its byte offset.
fn string_literals(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == b'"' {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() {
                out.push((start, text[start..j].replace("\\\"", "\"").replace("\\\\", "\\")));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The byte range of the first `(`-group after `name`: skips whitespace and
/// a `::<…>` turbofish, then matches parentheses by depth. `None` when no
/// call paren follows (a `use` item, a definition's own name, prose).
fn call_group(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = from;
    let mut skipped = 0;
    while i < bytes.len() && skipped < 80 {
        let c = bytes[i] as char;
        match c {
            c if c.is_whitespace() => i += 1,
            ':' if text[i..].starts_with("::") => {
                i += 2;
                // A turbofish: skip to the matching '>'.
                let mut depth = 0;
                while i < bytes.len() {
                    match bytes[i] as char {
                        '<' => depth += 1,
                        '>' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        '(' => return None, // a turbofish has no parentheses
                        _ => {}
                    }
                    i += 1;
                }
            }
            '(' => {
                let start = i;
                let mut depth = 0;
                while i < bytes.len() {
                    match bytes[i] as char {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some((start, i));
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                return None;
            }
            _ => return None,
        }
        skipped += 1;
    }
    None
}

/// What the literals of one call prove. Mirrors the shapes
/// `loom_daemon::script_helpers::gh_route::argv_access` proves from an
/// assembled argv; `None` is "not provable from these literals".
fn literal_shape(literals: &[String]) -> Option<&'static str> {
    let mut read = false;
    let mut write = false;
    // A literal `api` names the REST subcommand: without a write method or
    // request fields the call is a GET (the runtime classifier's rule).
    let mut rest_api = false;
    let mut graphql = false;
    let mut fields = false;
    for lit in literals {
        if lit == "api" {
            rest_api = true;
        }
        if lit == "graphql" {
            graphql = true;
        }
        if matches!(lit.as_str(), "-f" | "-F" | "--field" | "--raw-field" | "--input") {
            fields = true;
        }
        if WRITE_METHODS.contains(&lit.to_ascii_uppercase().as_str()) {
            write = true;
        }
        if let Some(m) = lit.strip_prefix("--method=") {
            if WRITE_METHODS.contains(&m.to_ascii_uppercase().as_str()) {
                write = true;
            } else if m.eq_ignore_ascii_case("GET") || m.eq_ignore_ascii_case("HEAD") {
                read = true;
            }
        }
        if let Some(m) = lit.strip_prefix("-X") {
            if WRITE_METHODS.contains(&m.to_ascii_uppercase().as_str()) {
                write = true;
            }
        }
        if lit == "GET" || lit == "HEAD" {
            read = true;
        }
        if READ_VERBS.contains(&lit.as_str()) {
            read = true;
        }
        if WRITE_VERBS.contains(&lit.as_str()) {
            write = true;
        }
        if let Some(doc) = lit.strip_prefix("query=") {
            let doc = doc.trim_start();
            if doc.starts_with("mutation") {
                write = true;
            } else if doc.starts_with("query") || doc.starts_with('{') {
                read = true;
            }
        }
    }
    // A REST call the literals can see in full: a GET unless a write marker
    // fired. A `graphql` endpoint keeps whatever its (possibly invisible)
    // document says — including nothing; its `-f` fields are the transport,
    // not a mutation claim (the HTTP method alone does not identify one).
    if rest_api && !graphql && !write {
        read = true;
    }
    if fields && rest_api && !graphql && !write {
        write = true;
        read = false;
    }
    match (read, write) {
        (true, false) => Some("read"),
        (false, true) => Some("write"),
        _ => None,
    }
}

/// Every intent mismatch and retired-name hit in `files`
/// (`(path relative to src/, text)`).
fn lint(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (rel, text) in files {
        let joined = code_lines(text).join("\n");
        for name in RETIRED {
            if joined.contains(name) {
                problems.push(format!(
                    "{rel}: the raw helper name `{name}` may not come back (#13744); route \
                     the call through `script_helpers::gh_read`/`gh_write`/`gh_read_json`"
                ));
            }
        }
        for ep in ENTRY_POINTS {
            let mut from = 0;
            while let Some(at) = joined[from..].find(ep).map(|i| from + i) {
                from = at + ep.len();
                // `gh_read` must not be the prefix of a longer name.
                if *ep == "gh_read"
                    && joined[at..]
                        .trim_start_matches("gh_read")
                        .starts_with(|c: char| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                let Some((open, close)) = call_group(&joined, at + ep.len()) else {
                    continue;
                };
                let span = &joined[open + 1..close];
                let mut lits: Vec<String> =
                    string_literals(span).into_iter().map(|(_, l)| l).collect();
                // The first argument is the operation name, not argv.
                if !lits.is_empty() {
                    lits.remove(0);
                }
                let Some(shape) = literal_shape(&lits) else {
                    continue;
                };
                let declared = if *ep == "gh_write" { "write" } else { "read" };
                if shape != declared {
                    problems.push(format!(
                        "{rel}: a `{ep}` call declares {declared} but its argv literals are \
                         provably a {shape} (#13744); the label is how a read would reach the \
                         writer — declare the intent the argv has",
                    ));
                }
            }
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

fn daemon_sources() -> Vec<(String, String)> {
    let mut files = Vec::new();
    collect_rust_files(&src_root(), &mut files);
    files
        .into_iter()
        .filter(|f| {
            let rel = f
                .strip_prefix(src_root())
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            !is_test_file(&rel)
        })
        .map(|f| {
            let rel = f
                .strip_prefix(src_root())
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&f)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", f.display()));
            (rel, text)
        })
        .collect()
}

/// The raw entry points are gone and every declared intent matches its argv.
#[test]
fn no_raw_helper_and_no_mislabelled_intent_remains() {
    let files = daemon_sources();
    assert!(
        files.len() > 100,
        "expected to scan the whole crate; only found {} file(s) — the scan's own premise \
         is broken, not the tree",
        files.len()
    );
    let problems = lint(&files);
    assert!(
        problems.is_empty(),
        "the script-helper `gh` family declares its intent (#13744):\n  {}",
        problems.join("\n  ")
    );
}

/// The scan really reads the whole tree: the files that carry the entry
/// points are in it (so a mismatch there could not hide).
#[test]
fn the_scan_covers_the_helper_and_its_callers() {
    let names: BTreeSet<String> = daemon_sources().into_iter().map(|(rel, _)| rel).collect();
    for expected in [
        "script_helpers/gh_route.rs",
        "script_helpers/validate_phase.rs",
        "cli/forge_verdict_cmd.rs",
    ] {
        assert!(names.contains(expected), "{expected} must be scanned");
    }
}

fn file(rel: &str, text: &str) -> Vec<(String, String)> {
    vec![(rel.to_string(), text.to_string())]
}

const READ_LABELLED_WRITE: &str = r#"
fn probe(root: &Path) -> CmdOutcome {
    gh_write("t.probe", &["api", "repos/o/r"], root, false)
}
"#;

#[test]
fn a_rest_get_declared_write_fails_in_a_new_file_and_an_inventoried_one() {
    // A brand-new file…
    let problems = lint(&file("new/module.rs", READ_LABELLED_WRITE));
    assert!(problems.len() == 1 && problems[0].contains("provably a read"), "{problems:?}");
    // …and a file that already exists and is on the inventory — the scan
    // does not allowlist anything.
    let problems = lint(&file("cli/forge_verdict_cmd.rs", READ_LABELLED_WRITE));
    assert!(problems.len() == 1 && problems[0].contains("provably a read"), "{problems:?}");
    // Qualified and imported spellings are the same call.
    let qualified = r#"
fn probe(root: &Path) -> CmdOutcome {
    crate::script_helpers::gh_write("t.probe", &["api", "user"], root, false)
}
"#;
    let problems = lint(&file("new/qualified.rs", qualified));
    assert!(problems.len() == 1 && problems[0].contains("gh_write"), "{problems:?}");
}

#[test]
fn view_and_list_reads_declared_write_fail() {
    for argv in [
        r#"gh_write("t.probe", &["pr", "view", "7"], root, false)"#,
        r#"gh_write("t.probe", &["pr", "list", "--state", "open"], root, false)"#,
        r#"gh_write("t.probe", &["issue", "view", "7", "--json", "state"], root, false)"#,
    ] {
        let src = format!("fn f(root: &Path) {{ {argv} }}\n");
        let problems = lint(&file("new/porcelain.rs", &src));
        assert!(
            problems.len() == 1 && problems[0].contains("provably a read"),
            "{argv}: {problems:?}"
        );
    }
}

#[test]
fn a_graphql_query_posted_but_declared_write_fails() {
    // `gh api graphql` sends the document with POST — the transport says
    // mutation, the document says read. The document decides.
    let src = r#"
fn who(root: &Path) -> CmdOutcome {
    gh_write("t.probe", &["api", "graphql", "-f", "query={ viewer { login } }"], root, false)
}
"#;
    let problems = lint(&file("new/graphql.rs", src));
    assert!(problems.len() == 1 && problems[0].contains("provably a read"), "{problems:?}");
    // A mutation document declared write is the honest pairing.
    let src =
        src.replace("query={ viewer { login } }", "query=mutation($i:ID!){updateIssue(input:$i)}");
    assert_eq!(lint(&file("new/graphql.rs", &src)), Vec::<String>::new());
}

#[test]
fn a_mutation_declared_read_fails() {
    for argv in [
        r#"gh_read("t.probe", &["issue", "edit", "7", "--add-label", "x"], root, false)"#,
        r#"gh_read("t.probe", &["api", "-X", "DELETE", "repos/o/r"], root, false)"#,
        r#"gh_read("t.probe", &["api", "-X", "POST", "repos/o/r", "-f", "labels[]=x"], root, false)"#,
        r#"gh_read_for_repo("t.probe", &["api", "graphql", "-f", "query=mutation{updateIssue}"], root, "o/r")"#,
    ] {
        let src = format!("fn f(root: &Path) {{ {argv} }}\n");
        let problems = lint(&file("new/mislabel.rs", &src));
        assert!(
            problems.len() == 1 && problems[0].contains("provably a write"),
            "{argv}: {problems:?}"
        );
    }
}

#[test]
fn honest_calls_and_the_audited_writer_probe_pass() {
    let honest = r#"
fn calls(root: &Path) {
    gh_read("t.listing", &["api", "repos/o/r/issues/7/comments", "--paginate"], root, false);
    gh_read("t.labels", &["issue", "view", "7", "--json", "labels"], root, true);
    gh_read_json("t.rows", &["pr", "list", "--state", "open"], root, false, |v: &Vec<u8>| false);
    gh_read_for_repo("t.graphql", &["api", "graphql", "-f", "query={ viewer { login } }"], root, "o/r");
    gh_write("t.comment", &["issue", "comment", "7", "--body", "b"], root, false);
    gh_write("t.label", &["api", "-X", "POST", "repos/o/r/issues/7/labels", "-f", "labels[]=x"], root, false);
    gh_write("t.withdraw", &["api", "-X", "DELETE", "repos/o/r/issues/comments/9"], root, false);
    // An asker-dependent probe: the facade's audited writer pin, listed in
    // tests/writer_read_guard.rs — not this family's business.
    let user = GhInvocation::new(op(), AccessIntent::Read, GhTarget::None, T)
        .writer_identity()
        .args(["api", "-i", "user"])
        .current_dir(root)
        .run();
    // An argv whose literals prove neither side is left to its declaration.
    gh_read("t.pull", &["api", &format!("repos/{repo}/pulls/{n}")], root, false);
    let argv: Vec<&str> = vec![];
    gh_write("t.built", &argv, root, false);
}
"#;
    assert_eq!(lint(&file("new/honest.rs", honest)), Vec::<String>::new());
}

#[test]
fn the_retired_names_fail_wherever_they_reappear() {
    for src in [
        "fn f(root: &Path) { run_gh(&[\"api\", \"user\"], root, false) }\n",
        "fn q(root: &Path) { gh_query(&[\"pr\", \"list\"], root, false, |v: &Vec<u8>| false) }\n",
        "fn run_gh(args: &[&str], root: &Path) -> CmdOutcome { todo!() }\n",
    ] {
        let problems = lint(&file("new/relapse.rs", src));
        assert!(problems.iter().any(|p| p.contains("may not come back")), "{src}: {problems:?}");
    }
}

#[test]
fn comments_and_test_modules_are_not_call_sites() {
    let prose = "// a `run_gh(` mention and \"pr\", \"view\" in prose\nfn f() {}\n";
    assert_eq!(lint(&file("new/prose.rs", prose)), Vec::<String>::new());
    let in_test_mod = "#[cfg(test)]\nmod tests {\n    fn f(root: &Path) { gh_write(\"t.p\", &[\"pr\", \"view\"], root, false) }\n}\n";
    assert_eq!(lint(&file("new/inline_tests.rs", in_test_mod)), Vec::<String>::new());
}
