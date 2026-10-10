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
    ("intake_reconcile/singleton.rs", "loom:"),
    ("label_registry/mod.rs", "loom:"),
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
    ("check-promotion-landed.sh", "loom:promotion-author-gate"),
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

/// Remove only the bodies of inline `#[cfg(test)] (#[..])* [pub..] mod name { .. }`
/// blocks. Everything else stays in the scan: `#[cfg(test)] mod name;`
/// declarations (their file is filtered by name in `rs_files`), test-only
/// `use` items and helper fns, and all production code after them. The block
/// end is the first line that is exactly `}` at the `#[cfg(test)]` line's
/// indentation (rustfmt layout, which CI enforces), so braces inside comments,
/// raw strings or char literals cannot move it. Both failure modes are loud or
/// strict, never blind: an unterminated block panics, and a stray `}` line
/// inside a multi-line raw string would only end the strip early, scanning
/// MORE test code, not less production code.
fn strip_inline_test_mods(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mod_re = Regex::new(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{\s*$")
        .expect("regex");
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == "#[cfg(test)]" {
            let indent = &line[..line.len() - line.trim_start().len()];
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim_start().starts_with("#[") {
                j += 1;
            }
            if j < lines.len() && mod_re.is_match(lines[j]) {
                let close = format!("{indent}}}");
                let end = (j + 1..lines.len())
                    .find(|&k| lines[k].trim_end() == close)
                    .unwrap_or_else(|| panic!("unterminated #[cfg(test)] mod at line {}", i + 1));
                i = end + 1;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    out
}

#[test]
fn strip_inline_test_mods_keeps_production_code_around_test_items() {
    // Unbalanced braces in comments, raw strings, char literals and strings
    // inside the test module must not move its end: the end is found by
    // layout (the column-0 `}`), never by counting braces (#10735 review).
    let src = r#"
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
#[cfg(test)]
use crate::x::y;
const A: &str = "loom:production-a";
#[cfg(test)]
pub(crate) mod inline_tests {
    // example payload: {
    /* unbalanced { in a block comment */
    const T: &str = "loom:test-only";
    const R: &str = r"{ raw string brace";
    const C: char = '{';
    fn f() {
        let _ = "loom:test-only-nested }";
    }
}
const B: &str = "loom:production-b";
"#;
    let kept = strip_inline_test_mods(src);
    assert!(kept.contains("loom:production-a"), "{kept}");
    assert!(kept.contains("loom:production-b"), "{kept}");
    assert!(!kept.contains("loom:test-only"), "{kept}");
}

#[test]
#[should_panic(expected = "unterminated #[cfg(test)] mod")]
fn strip_inline_test_mods_refuses_an_unterminated_test_module() {
    strip_inline_test_mods("#[cfg(test)]\nmod tests {\n    fn f() {}\n  }\n");
}

#[test]
fn guard_scans_production_code_past_early_cfg_test_items() {
    // work_finder.rs has a `#[cfg(test)] use` at line ~133 and its test
    // modules out-of-line; the scan must still cover nearly the whole file.
    let text = std::fs::read_to_string(repo_root().join("loom-daemon/src/work_finder.rs"))
        .expect("read work_finder.rs");
    let kept = strip_inline_test_mods(&text).lines().count();
    let total = text.lines().count();
    assert!(
        kept * 10 >= total * 9,
        "guard scans only {kept} of {total} lines of work_finder.rs"
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

/// Pull the `loom:` names off the first non-comment line of `script` that
/// matches `anchor` (a `for ... in ...; do` loop that hand-lists a label set).
fn loop_label_set(script: &str, anchor: &Regex) -> BTreeSet<String> {
    let text = std::fs::read_to_string(repo_root().join("defaults/scripts").join(script))
        .unwrap_or_else(|e| panic!("{script}: {e}"));
    let line = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find(|l| anchor.is_match(l))
        .unwrap_or_else(|| panic!("{script}: no line matching {anchor}"));
    let re = Regex::new(r"loom:[a-z][a-z0-9-]*").expect("regex");
    re.find_iter(line).map(|m| m.as_str().to_string()).collect()
}

fn names_with(prop: &str) -> BTreeSet<String> {
    Registry::embedded()
        .with_property(prop)
        .unwrap_or_else(|| panic!("unknown property {prop}"))
        .into_iter()
        .map(String::from)
        .collect()
}

/// `verdict-staleness-guard.sh` hand-lists the PR explicit-hold labels. That
/// set is not one registry property, so pin it to its two registry bounds:
/// every parking label is in it, and every member is a `hold` label. A new
/// park label or a de-held member fails here until the script is updated.
#[test]
fn verdict_staleness_guard_hold_set_is_bounded_by_the_registry() {
    let anchor = Regex::new(r"for held in ").expect("regex");
    let set = loop_label_set("verdict-staleness-guard.sh", &anchor);
    assert!(!set.is_empty());
    let park = names_with("park");
    let hold = names_with("hold");
    let missing_park: Vec<_> = park.difference(&set).collect();
    assert!(
        missing_park.is_empty(),
        "park label(s) {missing_park:?} missing from verdict-staleness-guard.sh hold_label()"
    );
    let not_hold: Vec<_> = set.difference(&hold).collect();
    assert!(
        not_hold.is_empty(),
        "verdict-staleness-guard.sh hold_label() lists non-hold label(s) {not_hold:?}"
    );
}

/// `warn-operator-gated.sh` flags a dependency carrying one of two labels; both
/// must stay operator-gate labels in the registry.
#[test]
fn warn_operator_gated_dep_labels_are_operator_gate_labels() {
    let anchor = Regex::new(r"for dep_label in ").expect("regex");
    let set = loop_label_set("warn-operator-gated.sh", &anchor);
    assert!(!set.is_empty());
    let gate = names_with("operator_gate");
    let outside: Vec<_> = set.difference(&gate).collect();
    assert!(
        outside.is_empty(),
        "warn-operator-gated.sh dep labels {outside:?} are not operator_gate in the registry"
    );
}

/// `check-promotion-landed.sh` treats `loom:building`/`loom:blocked` as
/// "promotion landed and progressed"; both must stay `skip` labels (work in
/// flight or parked) in the registry, or that rule needs revisiting.
#[test]
fn check_promotion_landed_progress_labels_are_registered_skip_labels() {
    let anchor = Regex::new(r#"select\(\.name=="loom:building""#).expect("regex");
    let set = loop_label_set("check-promotion-landed.sh", &anchor);
    let skip = names_with("skip");
    let outside: Vec<_> = set.difference(&skip).collect();
    assert!(outside.is_empty(), "later-lifecycle labels {outside:?} are not skip labels");
}
