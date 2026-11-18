//! Gate for the `loom_config_env` lock-order invariant (#14404).
//!
//! `loom-daemon`'s lib-test binary nests two `serial_test` locks in many places:
//! `loom_config_env` is the **outer** lock and the crate-default unkeyed
//! `#[serial]` group is the **inner** one. The order is mandatory — a site that
//! acquired them the other way round would form a lock cycle, and a cycle makes
//! the test binary *hang*, not fail.
//!
//! The property that keeps the order consistent is mechanical: a function that
//! carries `loom_config_env` in a `serial`/`parallel` attribute must itself be a
//! test (`#[test]`, `#[tokio::test]`, …), i.e. the key is only ever taken by the
//! outermost frame. This module scans `loom-daemon/src` for a violation, so a
//! reverse-order nesting fails here — loudly, on the PR that adds it — instead of
//! hanging CI.
//!
//! Out of scope: a body-level `fn inner()` nesting is invisible to an attribute
//! scan, and so is anything a macro generates beyond the attributes it spells
//! out (the `env_test!` macro in `host_identity_tests.rs` is covered because its
//! attributes are literal source lines).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// The key whose lock must only ever be the outermost one.
const OUTER_KEY: &str = "loom_config_env";

/// A function that holds the outer lock without being a test.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    /// 1-based line of the offending `serial`/`parallel` attribute.
    line: usize,
    /// The attribute text, whitespace-collapsed.
    attribute: String,
}

/// Result of scanning one source file.
#[derive(Debug, Default)]
struct Scan {
    /// How many attributes naming [`OUTER_KEY`] were seen.
    attributes: usize,
    violations: Vec<Violation>,
}

/// Is this attribute a `serial`/`parallel` lock attribute naming [`OUTER_KEY`]?
///
/// Matches the bare, `serial_test::`-pathed, single-key, multi-key and
/// `parallel` forms, and tolerates the attribute being split across lines
/// (the caller has already collapsed whitespace).
fn is_outer_lock_attribute(attr: &str) -> bool {
    let Some(inner) = attr.strip_prefix("#[").and_then(|a| a.strip_suffix(']')) else {
        return false;
    };
    let name = inner.split('(').next().unwrap_or("").trim();
    let name = name.strip_prefix("serial_test::").unwrap_or(name);
    if name != "serial" && name != "parallel" {
        return false;
    }
    let Some(args) = inner.split_once('(').map(|(_, a)| a) else {
        return false;
    };
    args.trim_end_matches(')')
        .split(',')
        .any(|k| k.trim() == OUTER_KEY)
}

/// Is this attribute a test marker (`#[test]`, `#[tokio::test]`,
/// `#[tokio::test(flavor = "...")]`, …)?
fn is_test_attribute(attr: &str) -> bool {
    let Some(inner) = attr.strip_prefix("#[").and_then(|a| a.strip_suffix(']')) else {
        return false;
    };
    let name = inner.split('(').next().unwrap_or("").trim();
    name == "test" || name.ends_with("::test")
}

/// Does this line declare a function (`fn`, `pub fn`, `async fn`, …)?
fn declares_fn(line: &str) -> bool {
    let mut words = line.split(|c: char| !(c.is_alphanumeric() || c == '_'));
    words.any(|w| w == "fn")
}

/// Scan one file's source for a non-test function holding [`OUTER_KEY`].
///
/// Pure over `&str`, so the bite tests below need no files. Line-wise: attribute
/// lines (joined across line breaks until their brackets balance) accumulate into
/// a stack; comments and blank lines are transparent; the first other line ends
/// the stack, and if it declares a `fn`, the stack is judged.
fn scan_source(src: &str) -> Scan {
    let mut scan = Scan::default();
    // (1-based line, collapsed attribute text) for the attribute stack in progress.
    let mut stack: Vec<(usize, String)> = Vec::new();
    // An attribute still being read across lines: (start line, text, bracket depth).
    let mut open: Option<(usize, String, i32)> = None;

    for (idx, raw) in src.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();

        if let Some((start, mut text, mut depth)) = open.take() {
            text.push(' ');
            text.push_str(line);
            depth += bracket_delta(line);
            if depth > 0 {
                open = Some((start, text, depth));
            } else {
                stack.push((start, collapse(&text)));
            }
            continue;
        }

        if line.starts_with("#[") {
            let depth = bracket_delta(line);
            if depth > 0 {
                open = Some((lineno, line.to_string(), depth));
            } else {
                stack.push((lineno, collapse(line)));
            }
            continue;
        }

        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        if declares_fn(line) {
            let is_test = stack.iter().any(|(_, a)| is_test_attribute(a));
            for (attr_line, attr) in &stack {
                if is_outer_lock_attribute(attr) {
                    scan.attributes += 1;
                    if !is_test {
                        scan.violations.push(Violation {
                            line: *attr_line,
                            attribute: attr.clone(),
                        });
                    }
                }
            }
        } else {
            // An attribute stack that does not lead to a `fn` (a `mod`, a `use`,
            // a struct field, …) still names the key; count it so the sanity
            // floor measures every occurrence, but it is not a function lock.
            scan.attributes += stack
                .iter()
                .filter(|(_, a)| is_outer_lock_attribute(a))
                .count();
        }
        stack.clear();
    }
    scan
}

/// Net `[` minus `]` on a line; a positive total means the attribute is unfinished.
fn bracket_delta(line: &str) -> i32 {
    line.chars().fold(0, |d, c| match c {
        '[' => d + 1,
        ']' => d - 1,
        _ => d,
    })
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The repository root: `loom-daemon/`'s parent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ has a parent")
        .to_path_buf()
}

fn tracked_files(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// The gate, run against every tracked `loom-daemon/src/**/*.rs` file.
///
/// Fails if a function that is not a test carries `loom_config_env` in a
/// `serial`/`parallel` attribute. The lock order is `loom_config_env` OUTER,
/// crate-default `#[serial]` group INNER; a non-test function holding the outer
/// key can be called from inside the inner lock, which is the reverse order and
/// hangs the test binary.
#[test]
fn loom_config_env_is_only_ever_the_outer_lock() {
    let root = repo_root();
    if !root.join(".git").exists() {
        // A source tarball has no git index to enumerate; say so rather than
        // passing silently on an unmeasured tree.
        eprintln!("skipping: {} is not a git checkout", root.display());
        return;
    }
    let Some(files) = tracked_files(&root) else {
        eprintln!("skipping: `git ls-files` unavailable");
        return;
    };
    assert!(files.len() > 100, "`git ls-files` returned only {} path(s)", files.len());

    let mut attributes = 0;
    let mut problems = Vec::new();
    for rel in files
        .iter()
        .filter(|f| f.starts_with("loom-daemon/src/") && f.ends_with(".rs"))
    {
        // A tracked file deleted in the working tree is not this gate's concern.
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        let scan = scan_source(&src);
        attributes += scan.attributes;
        for v in scan.violations {
            problems.push(format!("{rel}:{} {}", v.line, v.attribute));
        }
    }

    assert!(
        attributes > 100,
        "scanned only {attributes} `{OUTER_KEY}` attribute(s) — the scan is broken, not the tree"
    );
    assert!(
        problems.is_empty(),
        "`{OUTER_KEY}` must only ever be the OUTER lock: it may be taken only by a \
         test function itself (`#[test]` / `#[tokio::test]`), never by a helper. The \
         lock order is `{OUTER_KEY}` outer, the crate-default `#[serial]` group inner; \
         a helper holding `{OUTER_KEY}` can be reached from inside the default group, \
         which is a lock cycle that hangs the test binary instead of failing it.\n  {}",
        problems.join("\n  ")
    );
}

// ============================================================================
// The scan bites: pure rule tests over in-memory source
// ============================================================================

#[test]
fn a_reverse_order_nesting_fails_the_scan() {
    // Single-line literals on purpose: a raw multi-line string here would put a
    // line starting with `#[serial(...)]` in THIS file, which the gate above
    // would then (correctly) flag.
    let src =
        "#[serial]\nfn outer() {\n    inner();\n}\n\n#[serial(loom_config_env)]\nfn inner() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 6,
            attribute: "#[serial(loom_config_env)]".to_string()
        }]
    );
}

#[test]
fn a_normal_test_site_passes() {
    let src = "#[test]\n#[serial(loom_config_env)]\nfn ok() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}

#[test]
fn attribute_order_and_intervening_docs_do_not_hide_the_test_marker() {
    let src = "#[serial(loom_config_env)]\n/// doc\n#[test]\nfn ok() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}

#[test]
fn path_multi_key_and_parallel_forms_are_all_seen() {
    for attr in [
        "#[serial_test::serial(loom_config_env)]",
        "#[serial_test::serial(codesign_identity_env, loom_config_env)]",
        "#[serial(loom_config_env, other)]",
        "#[parallel(loom_config_env)]",
        "#[serial_test::parallel(loom_config_env)]",
    ] {
        let helper = scan_source(&format!("{attr}\nfn helper() {{}}\n"));
        assert_eq!(helper.violations.len(), 1, "{attr} on a helper must be flagged");
        let test = scan_source(&format!("#[test]\n{attr}\nfn t() {{}}\n"));
        assert!(test.violations.is_empty(), "{attr} on a test must pass");
        assert_eq!(test.attributes, 1);
    }
}

#[test]
fn other_keys_and_the_default_group_are_not_flagged() {
    let src = "#[serial]\nfn a() {}\n#[serial(other_env)]\nfn b() {}\n#[serial(codesign_identity_env)]\nfn c() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 0);
    assert!(scan.violations.is_empty());
}

#[test]
fn an_attribute_split_across_lines_is_still_seen() {
    let src = "#[test]\n#[serial(\n    loom_config_env\n)]\nfn ok() {}\n\n#[serial(\n    loom_config_env,\n)]\nfn helper() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert_eq!(scan.violations.len(), 1);
    assert_eq!(scan.violations[0].line, 7);
}

#[test]
fn tokio_test_counts_as_a_test() {
    for marker in [
        "#[tokio::test]",
        "#[tokio::test(flavor = \"multi_thread\")]",
    ] {
        let src = format!("{marker}\n#[serial(loom_config_env)]\nasync fn t() {{}}\n");
        let scan = scan_source(&src);
        assert_eq!(scan.attributes, 1);
        assert!(scan.violations.is_empty(), "{marker}");
    }
}

#[test]
fn a_macro_generated_test_with_literal_attributes_passes() {
    // The shape of `env_test!` in host_identity_tests.rs.
    let src = "$(#[$m])*\n#[test]\n#[serial(loom_config_env)]\nfn $name() {\n    #[serial]\n    fn inner() $body\n    inner();\n}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}
