//! A machine-checkable audit of every `git fetch` / `git rebase` argument
//! vector in `loom-daemon/src` (#9479, closing the Rust blind spot #9474 left).
//!
//! # Why a scan and not a review note
//!
//! #9106's vector is an argv one: git's own ref validator accepts a
//! leading-dash name, so a forge-hosted branch called
//! `--upload-pack=/tmp/payload` is a legal ref, and handed to `git fetch` as a
//! bare operand it is re-parsed as a **switch** — on a path/`file://` origin,
//! arbitrary code execution. `std::process::Command` does not help: it blocks
//! *shell* injection, not *git-option* injection.
//!
//! #9474 closed every sink #9106 enumerated and shipped an awk scan over
//! `defaults/scripts/*.sh` to keep the shell half honest. The Rust half was
//! asserted by a single `grep -q 'refname::check_all'` against one file, so a
//! *new* unguarded Rust sink was invisible to CI — which is exactly how the
//! four #9479 names got there. This file is that missing scan.
//!
//! # The invariant, stated so a reviewer can re-derive it by eye
//!
//! > In every `loom-daemon/src/**/*.rs` array literal that is a `fetch` or
//! > `rebase` argument vector, if any element is **not** a plain string
//! > literal, the vector must contain a standalone `"--"` element **and** at
//! > least one non-literal element must follow it.
//!
//! `--` ends git's option parsing: everything after it is a ref operand, never
//! a switch. So the rule is "the computed operands live behind the separator".
//!
//! ## What the rule deliberately does NOT say
//!
//! It does not require `--` before *every* non-literal, because one slot
//! legitimately precedes it: the **remote** (`git fetch <remote> -- <ref>` —
//! a `--` before the remote would make the remote itself a refspec), and for
//! `rebase`, an option's value (`--onto <commit>`). Those slots are covered by
//! the other half of the mitigation — `refname::check_refname` / `check_all`
//! at the entry point — which the scan cannot locate mechanically to the same
//! precision (it legitimately sits at a different scope from its call site)
//! and which `defaults/scripts/tests/test-check-branch-name.sh` asserts per
//! known module by name instead.
//!
//! The scan is therefore necessary, not sufficient — the same standing
//! `#9474` had for the shell half.

use std::path::{Path, PathBuf};

// ───────────────────────────────────────────────────────────────────────────
// The scanner
// ───────────────────────────────────────────────────────────────────────────

/// One offending argument vector.
#[derive(Debug, PartialEq, Eq)]
struct Offender {
    file: String,
    line: usize,
    argv: String,
    why: &'static str,
}

impl std::fmt::Display for Offender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {} — [{}]", self.file, self.line, self.why, self.argv)
    }
}

/// If a literal starts at `i`, the index just past it; otherwise `None`.
///
/// Covers every form this crate actually contains — plain, raw (`r"…"`,
/// `r#"…"#`), byte (`b"…"`, `br#"…"#`) and char (`'x'`, `'\n'`, and crucially
/// `'"'`, which a naive quote scanner reads as the start of a string). A lone
/// `'` that is a lifetime (`&'a str`) returns `None` and is stepped over as an
/// ordinary byte.
fn skip_literal(b: &[u8], i: usize) -> Option<usize> {
    // Raw string, optionally byte-prefixed: [b]r#*"…"#*
    let mut j = i;
    if b[j] == b'b' {
        j += 1;
    }
    if j < b.len() && b[j] == b'r' {
        let mut hashes = 0usize;
        let mut k = j + 1;
        while k < b.len() && b[k] == b'#' {
            hashes += 1;
            k += 1;
        }
        if k < b.len() && b[k] == b'"' {
            let mut p = k + 1;
            while p < b.len() {
                if b[p] == b'"' {
                    let mut q = p + 1;
                    let mut seen = 0usize;
                    while q < b.len() && seen < hashes && b[q] == b'#' {
                        q += 1;
                        seen += 1;
                    }
                    if seen == hashes {
                        return Some(q);
                    }
                }
                p += 1;
            }
            return Some(b.len());
        }
    }

    // Plain or byte string.
    let mut j = i;
    if b[j] == b'b' && j + 1 < b.len() && b[j + 1] == b'"' {
        j += 1;
    }
    if b[j] == b'"' {
        let mut p = j + 1;
        while p < b.len() {
            if b[p] == b'\\' {
                p += 2;
                continue;
            }
            if b[p] == b'"' {
                return Some(p + 1);
            }
            p += 1;
        }
        return Some(b.len());
    }

    // Char literal — but not a lifetime.
    if b[j] == b'\'' {
        if j + 1 < b.len() && b[j + 1] == b'\\' {
            let mut p = j + 2;
            while p < b.len() && p < j + 12 {
                if b[p] == b'\'' {
                    return Some(p + 1);
                }
                p += 1;
            }
            return None;
        }
        if j + 2 < b.len() && b[j + 2] == b'\'' {
            return Some(j + 3);
        }
    }
    None
}

/// Replace every comment byte with a space, preserving length (so byte
/// offsets, and therefore line numbers, stay exact) and literals.
///
/// Without this, a doc comment that *quotes* a shell sink — several of these
/// modules carry one, e.g. ``/// `git fetch origin -- "$BRANCH"` `` — would be
/// parsed as code.
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = b.to_vec();
    let mut i = 0usize;
    while i < b.len() {
        if let Some(end) = skip_literal(b, i) {
            i = end;
            continue;
        }
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                let mut depth = 1usize;
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        continue;
                    }
                    if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        continue;
                    }
                    if b[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split the bracket group starting at `open` (the index of its `[`) into
/// top-level, comma-separated elements. `None` if the group is unterminated.
fn split_elements(src: &str, open: usize) -> Option<Vec<String>> {
    let b = src.as_bytes();
    let mut elements = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        if let Some(end) = skip_literal(b, i) {
            let end = end.min(src.len());
            current.push_str(&src[i..end]);
            i = end;
            continue;
        }
        let c = b[i] as char;
        match c {
            '[' | '(' | '{' => {
                depth += 1;
                if depth > 1 {
                    current.push(c);
                }
            }
            ']' | ')' | '}' => {
                depth -= 1;
                if depth == 0 {
                    elements.push(current.trim().to_string());
                    return Some(elements.into_iter().filter(|e| !e.is_empty()).collect());
                }
                current.push(c);
            }
            ',' if depth == 1 => {
                elements.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
        i += 1;
    }
    None
}

/// A plain string literal — the only element shape whose value is fixed at
/// compile time *and* visible to this scan. A `const` identifier is also
/// compile-time fixed but indistinguishable here from a runtime binding, so it
/// counts as non-literal and its call site carries a `--` like any other.
fn is_string_literal(e: &str) -> bool {
    e.len() >= 2 && e.starts_with('"') && e.ends_with('"') && !e[1..e.len() - 1].contains('"')
}

/// Every `fetch` / `rebase` argv in `src` that breaks the invariant.
fn scan_source(file: &str, src: &str) -> Vec<Offender> {
    let stripped = strip_comments(src);
    let mut out = Vec::new();

    for subcommand in ["\"fetch\"", "\"rebase\""] {
        let mut from = 0usize;
        while let Some(rel) = stripped[from..].find(subcommand) {
            let at = from + rel;
            from = at + subcommand.len();

            // The literal must sit inside an ARRAY. `Some("rebase")`,
            // `set.insert("rebase")`, `resolve_merge_method(Some("rebase"), …)`
            // are data, not argument vectors.
            let Some(open) = enclosing_array(&stripped, at) else {
                continue;
            };
            let Some(elements) = split_elements(&stripped, open) else {
                continue;
            };
            // Position of the subcommand inside its array, found by matching
            // the element text (the subcommand may be preceded by git's own
            // global options, e.g. `["-c", "maintenance.auto=false", "fetch", …]`).
            let Some(idx) = elements.iter().position(|e| e == subcommand) else {
                continue;
            };
            let operands = &elements[idx + 1..];
            if operands.iter().all(|e| is_string_literal(e)) {
                continue; // Nothing computed reaches git: nothing to protect.
            }
            let sep = operands.iter().position(|e| e == "\"--\"");
            let why = match sep {
                None => "no standalone \"--\" separator before the computed ref operands",
                Some(s) if !operands[s + 1..].iter().any(|e| !is_string_literal(e)) => {
                    "the \"--\" separator is present but every computed operand precedes it"
                }
                Some(_) => continue,
            };
            let line = stripped[..at].matches('\n').count() + 1;
            if annotated_vector_control(src, line) {
                continue;
            }
            out.push(Offender {
                file: file.to_string(),
                line,
                argv: elements.join(", "),
                why,
            });
        }
    }
    out
}

/// The one exemption, and it is per **call site**, not per directory.
///
/// A handful of tests must run the UNSEPARATED form on purpose — they are the
/// controls that prove the vector still reproduces on this host, without which
/// the guard tests beside them would be vacuous (see
/// `worktree_cli::existing::tests`'s dash-branch case and
/// `tests/reconcile_stack_refname_guard.rs`). Exempting whole test
/// directories would hide a real sink that happens to live in one; a marker
/// comment within six lines of the call exempts exactly the line that asked
/// for it, and is grep-able as an inventory of every deliberate offender.
const VECTOR_CONTROL_MARKER: &str = "LOOM-REF-SCAN: vector-control";

fn annotated_vector_control(src: &str, line: usize) -> bool {
    let lines: Vec<&str> = src.lines().collect();
    let hi = line.min(lines.len());
    let lo = line.saturating_sub(6).max(1);
    lines[lo - 1..hi]
        .iter()
        .any(|l| l.contains(VECTOR_CONTROL_MARKER))
}

/// The `[` of the innermost array containing byte `at`, or `None` when the
/// innermost enclosing delimiter is a `(` or `{`.
fn enclosing_array(src: &str, at: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut i = 0usize;
    while i < at {
        if let Some(end) = skip_literal(b, i) {
            i = end;
            continue;
        }
        match b[i] {
            c @ (b'[' | b'(' | b'{') => stack.push((c, i)),
            b']' | b')' | b'}' => {
                stack.pop();
            }
            _ => {}
        }
        i += 1;
    }
    match stack.last() {
        Some((b'[', open)) => Some(*open),
        _ => None,
    }
}

fn rust_sources(root: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push(path);
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Acceptance criterion 3: the scan FAILS on a synthetic offender…
// ───────────────────────────────────────────────────────────────────────────

/// A pattern test that cannot fire is decoration. Each case below is a shape
/// the four #9479 sinks actually had.
#[test]
fn the_scan_detects_synthetic_unguarded_sinks() {
    let cases: [(&str, &str); 5] = [
        (
            "merge_pr::stacked_children's shape",
            r#"let _ = git(repo_root, &["fetch", "--quiet", "origin", branch]);"#,
        ),
        (
            "merge_pr::version_policy's shape (two computed operands)",
            r#"git(repo, &["fetch", "--quiet", "origin", inputs.default_branch, inputs.branch]);"#,
        ),
        (
            "worktree_cli::upstream's shape",
            r#"git_discard(repo, &["fetch", "origin", branch]);"#,
        ),
        (
            "daemon_update::sync's shape (trailing option, no separator)",
            r#"util::git_ok(repo_root, &["fetch", "origin", &branch, "--quiet"]);"#,
        ),
        (
            "a rebase with computed ref operands",
            r#"git(dir, &["rebase", "--onto", &target, &parent, &child]);"#,
        ),
    ];

    for (what, line) in cases {
        let src = format!("fn f() {{\n    {line}\n}}\n");
        let hits = scan_source("offender.rs", &src);
        assert_eq!(hits.len(), 1, "{what}: the scan missed a synthetic sink\n{src}");
        assert_eq!(hits[0].line, 2, "{what}: wrong line reported");
    }
}

/// …and the same lines, separated, are clean. Without this, a scan that
/// flagged *everything* would pass the test above.
#[test]
fn the_scan_accepts_the_guarded_forms() {
    for line in [
        r#"let _ = git(repo_root, &["fetch", "--quiet", "origin", "--", branch]);"#,
        r#"git(repo, &["fetch", "--quiet", "origin", "--", a, b]);"#,
        r#"git(dir, &["rebase", "--onto", &target, "--", &parent, &child]);"#,
        r#"git(repo, &["-c", "maintenance.auto=false", "fetch", "origin", "--", key]);"#,
        // All-literal vectors have nothing computed to protect.
        r#"git(&repo, &["fetch", "--prune", "origin"]);"#,
        // `"rebase"`/`"fetch"` as DATA, not as an argv subcommand.
        r#"let m = match s { "rebase" => Some(resolve(flags(a, b))), _ => None };"#,
        r#"set.insert("rebase"); names.push("rebase");"#,
        r#"assert_eq!(resolve(Some("rebase"), flags(true, false)).unwrap(), "rebase");"#,
        r#"let alt = ["merge", "rebase", "squash"].iter().filter(|x| f(x));"#,
        // A comment that quotes a shell sink is prose, not code.
        r#"// git fetch origin "$BRANCH" — the shape this guard replaced"#,
    ] {
        let src = format!("fn f() {{\n    {line}\n}}\n");
        let hits = scan_source("clean.rs", &src);
        assert!(hits.is_empty(), "false positive on:\n  {line}\n  -> {hits:?}");
    }
}

/// The vector-control exemption must be narrow: it exempts the annotated call
/// site and nothing else in the file.
#[test]
fn the_vector_control_marker_exempts_only_its_own_call_site() {
    let mut src = String::from("fn control() {\n");
    src.push_str(&format!(
        "    // {VECTOR_CONTROL_MARKER} — the unseparated form must reproduce here.\n"
    ));
    src.push_str("    let _ = git(d, &[\"fetch\", \"origin\", evil]);\n}\n");
    let control_line = 3;
    // Far enough below that the marker's window cannot reach it — the point of
    // the assertion is that the exemption does not leak down the file.
    for _ in 0..10 {
        src.push('\n');
    }
    src.push_str("fn real() {\n    let _ = git(d, &[\"fetch\", \"origin\", branch]);\n}\n");
    let real_line = src[..src.find("branch]").unwrap()].matches('\n').count() + 1;

    let hits = scan_source("mixed.rs", &src);
    assert_eq!(hits.len(), 1, "exactly the unannotated sink should be reported: {hits:?}");
    assert_eq!(hits[0].line, real_line, "the annotated control must not be the one reported");
    assert_ne!(hits[0].line, control_line);
}

/// A `"--"` that is present but has nothing computed behind it is not a
/// separator, it is a decoy — the scan must say so rather than pass.
#[test]
fn a_trailing_separator_does_not_satisfy_the_rule() {
    let src = "fn f() {\n    git(d, &[\"fetch\", \"origin\", branch, \"--\"]);\n}\n";
    let hits = scan_source("decoy.rs", src);
    assert_eq!(hits.len(), 1, "a `--` after the operand must not count");
    assert!(hits[0].why.contains("precedes it"), "{:?}", hits[0]);
}

// ───────────────────────────────────────────────────────────────────────────
// …and PASSES on the tree
// ───────────────────────────────────────────────────────────────────────────

/// The standing assertion: no `loom-daemon/src` git sink hands a computed ref
/// to git ahead of the end-of-options separator.
#[test]
fn no_unguarded_fetch_or_rebase_sink_in_loom_daemon_src() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src_root, &mut files);
    assert!(
        files.len() > 50,
        "the scan found only {} source files under {} — it is not actually looking at the crate",
        files.len(),
        src_root.display()
    );

    let mut offenders = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let rel = file
            .strip_prefix(&src_root)
            .unwrap_or(file)
            .display()
            .to_string();
        offenders.extend(scan_source(&rel, &text));
    }

    assert!(
        offenders.is_empty(),
        "unguarded git fetch/rebase sink(s) — put the computed ref operands behind a \
         standalone \"--\" (#9106/#9479):\n{}",
        offenders
            .iter()
            .map(|o| format!("  {o}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The validator half, which the scan cannot locate mechanically: the modules
/// that receive a FORGE-derived name must still call `refname`. Listed by name
/// because this is the inventory a future change has to keep honest — an entry
/// silently losing its validator is precisely the #9106 regression.
#[test]
fn every_forge_facing_module_still_calls_the_refname_validator() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for module in [
        "reconcile_stack.rs",
        "merge_pr/version_policy.rs",
        "merge_pr/stacked_children.rs",
    ] {
        let text = std::fs::read_to_string(src_root.join(module))
            .unwrap_or_else(|e| panic!("{module} must exist to be audited: {e}"));
        assert!(
            text.contains("refname::check_refname") || text.contains("refname::check_all"),
            "{module} hands a forge-derived branch name to git but no longer validates it \
             (refname::check_refname / check_all) — #9106/#9479"
        );
    }
}
