//! Registry literal guards (#10013 slice 3, AC 4 and AC 5).
//!
//! 1. Non-test daemon source may not contain a `"loom:..."` string literal
//!    that is missing from `defaults/labels.json`, except the shrinking
//!    allowlist below (a ratchet: an allowlisted literal that no longer
//!    occurs fails the test, so the list can only get shorter).
//! 2. The label-naming shell scripts may only name registered `loom:` labels
//!    (plus a short, ratcheted list of non-label markers).
//!
//! A new label must be added to the registry first; typos and unregistered
//! labels fail here instead of silently joining or missing a label set.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use loom_daemon::label_registry::Registry;
use regex::Regex;

/// `(file relative to loom-daemon/src, literal)` pairs that are not registry
/// labels: prefixes, comment/marker/lock names, and the bare `loom:` prefix.
/// Only ever remove entries.
const SOURCE_ALLOWLIST: &[(&str, &str)] = &[
    ("epic_supervisor.rs", "loom:epic:"),
    ("guards_status.rs", "loom:forge-egress"),
    ("host_affinity.rs", "loom:host:"),
    ("intake_reconcile.rs", "loom:"),
    ("label_registry/mod.rs", "loom:"),
    ("intake_reconcile/singleton.rs", "loom:"),
    ("merge_pr/chain_lock.rs", "loom:chain-head-lock"),
    ("merge_pr/consolidate.rs", "loom:consolidation"),
    ("merge_pr/consolidate.rs", "loom:consolidation-abort"),
    ("merge_pr/sequence.rs", "loom:sequence"),
    ("merge_pr/stale_checks/local_eval.rs", "loom:merge-tree-reverify"),
    ("observability/captain_gauges/facts.rs", "loom:"),
    ("observability/ops/stage_dwell.rs", "loom:"),
    ("observability/pick_journal.rs", "loom:"),
    ("pr_planning.rs", "loom:"),
    ("premise_check/cli.rs", "loom:premise-check"),
    ("premise_check/record.rs", "loom:premise-check"),
    ("star_liveness/levels.rs", "loom:priority-inherited"),
];

/// `(script, token)` pairs in the shell scripts that are comment markers, not
/// labels. Only ever remove entries.
const SCRIPT_ALLOWLIST: &[(&str, &str)] = &[
    ("extract-capability-markers.sh", "loom:capability"),
    ("verdict-staleness-guard.sh", "loom:verdict-sha"),
    ("verdict-staleness-guard.sh", "loom:verdict-stale"),
];

/// The scripts named by #10013 plus the hard-exclusion accessor.
const SCRIPTS: &[&str] = &[
    "check-labels-drift.sh",
    "check-promotion-landed.sh",
    "check-stale-blocked.sh",
    "classify-dependency-block.sh",
    "extract-capability-markers.sh",
    "merge-pr.sh",
    "premise-check.sh",
    "record-noop-release.sh",
    "resync-installed.sh",
    "verdict-staleness-guard.sh",
    "warn-operator-gated.sh",
    "warn-out-of-set-deps.sh",
    "hard-exclusion-labels.sh",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("read_dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if n == "tests.rs" || n.ends_with("_tests.rs") || n.starts_with("test_") {
                continue;
            }
            out.push(p);
        }
    }
}

/// Remove the brace-matched body of every inline `#[cfg(test)] mod name { .. }`
/// and nothing else. Other `#[cfg(test)]` items (a `mod tests;` declaration, a
/// `use`, a helper fn) are left in place, so the production code after them is
/// still scanned.
fn strip_inline_test_mods(text: &str) -> String {
    let attr = Regex::new(
        r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{",
    )
    .expect("regex");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(m) = attr.find(rest) {
        out.push_str(&rest[..m.start()]);
        let after = &rest[m.end()..];
        let mut depth = 1usize;
        let mut in_str = false;
        let mut esc = false;
        let mut end = after.len();
        for (i, c) in after.char_indices() {
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn strip_keeps_production_code_after_non_inline_cfg_test_items() {
    let src = "#[cfg(test)]\nmod tests;\nfn prod() { let _ = \"loom:prod\"; }\n\
               #[cfg(test)]\nmod t { fn x() { let _ = \"loom:test}\"; } }\nfn after() { let _ = \"loom:after\"; }\n";
    let body = strip_inline_test_mods(src);
    assert!(body.contains("loom:prod"), "code after `mod tests;` must stay scanned");
    assert!(body.contains("loom:after"), "code after an inline test mod must stay scanned");
    assert!(!body.contains("loom:test"), "inline test mod body must be stripped");
}

#[test]
fn guard_scans_label_heavy_files_past_their_first_cfg_test() {
    let src = repo_root().join("loom-daemon/src");
    let text = std::fs::read_to_string(src.join("work_finder.rs")).expect("read");
    assert!(
        strip_inline_test_mods(&text).lines().count() > 1000,
        "work_finder.rs must be scanned well past its first #[cfg(test)]"
    );
}

#[test]
fn no_unregistered_loom_literal_in_non_test_daemon_source() {
    let reg = Registry::embedded();
    let re = Regex::new(r#""(loom:[A-Za-z0-9:_*.-]*)""#).expect("regex");
    let src = repo_root().join("loom-daemon/src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);

    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    for f in files {
        let text = std::fs::read_to_string(&f).expect("read");
        let body = strip_inline_test_mods(&text);
        let rel = f
            .strip_prefix(&src)
            .expect("under src")
            .to_string_lossy()
            .replace('\\', "/");
        for m in re.captures_iter(&body) {
            if reg.get(&m[1]).is_none() {
                found.insert((rel.clone(), m[1].to_string()));
            }
        }
    }
    let allowed: BTreeSet<(String, String)> = SOURCE_ALLOWLIST
        .iter()
        .map(|(a, b)| ((*a).into(), (*b).into()))
        .collect();

    let unregistered: Vec<_> = found.difference(&allowed).collect();
    assert!(
        unregistered.is_empty(),
        "loom: literal(s) not in defaults/labels.json: {unregistered:?}. Add the label to \
         the registry (then `loom-daemon labels generate --write`), do not extend the allowlist."
    );
    let stale: Vec<_> = allowed.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "SOURCE_ALLOWLIST entries no longer needed (remove them, the list only shrinks): {stale:?}"
    );
}

#[test]
fn label_shell_scripts_name_only_registered_labels() {
    let reg = Registry::embedded();
    let re = Regex::new(r"loom:[a-z][a-z0-9-]*").expect("regex");
    let dir = repo_root().join("defaults/scripts");
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    for s in SCRIPTS {
        let text = std::fs::read_to_string(dir.join(s)).unwrap_or_else(|e| panic!("{s}: {e}"));
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            for m in re.find_iter(line) {
                if reg.get(m.as_str()).is_none() {
                    found.insert(((*s).into(), m.as_str().into()));
                }
            }
        }
    }
    let allowed: BTreeSet<(String, String)> = SCRIPT_ALLOWLIST
        .iter()
        .map(|(a, b)| ((*a).into(), (*b).into()))
        .collect();
    let unregistered: Vec<_> = found.difference(&allowed).collect();
    assert!(
        unregistered.is_empty(),
        "script label(s) not in defaults/labels.json: {unregistered:?}"
    );
    let stale: Vec<_> = allowed.difference(&found).collect();
    assert!(stale.is_empty(), "SCRIPT_ALLOWLIST entries no longer needed: {stale:?}");
}
