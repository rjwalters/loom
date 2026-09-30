//! Tests for per-repo input declarations (#9589).

use super::super::inputs::{file_set, BaseMove, FileSet, ScopedEvidence};
use super::super::workflow_scope::CiScope;
use super::super::{assess_scoped, CheckRun, Verdict};
use super::*;
use chrono::{DateTime, Utc};

const LINT: &str = "Lint (ruff)";

/// A klayout-tools-shaped declaration: ruff reads its config and each `.py`
/// file on its own.
const LINT_DECL: &str = r#"{
  "version": 1,
  "checks": {
    "Lint (ruff)": {
      "global": ["pyproject.toml", ".github/workflows/ci.yml"],
      "scanned": ["**/*.py"]
    }
  }
}"#;

fn fset(paths: &[&str]) -> FileSet {
    file_set(paths.iter().map(|p| (*p, false)))
}

fn green(name: &str) -> CheckRun {
    CheckRun {
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some("success".to_string()),
        // Re-dated after the tip, so the time rule alone would pass it: every
        // refusal below is the input-scoped predicate's.
        started_at: Some("2026-09-30T12:00:00Z".parse().unwrap()),
        actions_run_id: None,
        actions_job_id: None,
    }
}

fn tip() -> DateTime<Utc> {
    "2026-09-30T11:00:00Z".parse().unwrap()
}

/// Evidence where every `contexts` entry tested the same base and saw `d`.
fn evidence(contexts: &[&str], d: &[&str], p: &[&str], repo: RepoSpecs) -> ScopedEvidence {
    ScopedEvidence {
        pr_delta: fset(p),
        pr_ci_scope: CiScope::Unscoped,
        base_moves: contexts
            .iter()
            .map(|c| {
                (
                    (*c).to_string(),
                    BaseMove {
                        tested_base: "b4se".to_string(),
                        files: fset(d),
                        ci_scope: CiScope::Unscoped,
                    },
                )
            })
            .collect(),
        fallbacks: BTreeMap::new(),
        repo_specs: repo,
    }
}

fn verdict(contexts: &[&str], ev: &ScopedEvidence) -> (Verdict, Vec<String>) {
    let required: Vec<String> = contexts.iter().map(|c| (*c).to_string()).collect();
    let runs: Vec<CheckRun> = contexts.iter().map(|c| green(c)).collect();
    assess_scoped(tip(), &required, &runs, Some(ev))
}

fn stale_check(v: &Verdict) -> Option<&str> {
    match v {
        Verdict::StaleInputs { check, .. } => Some(check),
        _ => None,
    }
}

// --- Absent: today's behaviour, unchanged ------------------------------------

#[test]
fn absent_declaration_keeps_any_base_move_stale_without_a_warning() {
    let ev = evidence(&[LINT], &["README.md"], &["src/a.py"], RepoSpecs::Absent);
    let (v, warnings) = verdict(&[LINT], &ev);
    match &v {
        Verdict::StaleInputs { reason, .. } => {
            assert!(reason.clause.contains("no entry in the input-scope table"), "{reason:?}");
            assert!(reason.clause.contains(DECLARATION_PATH), "{reason:?}");
        }
        other => panic!("an undeclared context must stay stale, got {other:?}"),
    }
    assert!(warnings.is_empty(), "absence is silent: {warnings:?}");
}

#[test]
fn a_404_is_absence_and_any_other_read_error_is_a_rejection() {
    assert_eq!(
        RepoSpecs::from_fetch(Err("gh: Not Found (HTTP 404)".to_string())),
        RepoSpecs::Absent
    );
    for e in [
        "gh: Forbidden (HTTP 403)",
        "could not exec gh api: No such file",
        "HTTP 502",
    ] {
        assert!(
            matches!(RepoSpecs::from_fetch(Err(e.to_string())), RepoSpecs::Rejected(_)),
            "{e} could be hiding a declaration and must reject"
        );
    }
}

// --- A declaration narrows only what it lists --------------------------------

#[test]
fn a_declared_context_is_fresh_across_an_unrelated_base_move() {
    let ev =
        evidence(&[LINT], &["README.md", "docs/x.md"], &["src/a.py"], RepoSpecs::parse(LINT_DECL));
    let (v, warnings) = verdict(&[LINT], &ev);
    assert_eq!(v, Verdict::Fresh);
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn a_declared_context_is_stale_when_the_base_moves_a_global_input() {
    let ev = evidence(&[LINT], &["pyproject.toml"], &["src/a.py"], RepoSpecs::parse(LINT_DECL));
    assert_eq!(stale_check(&verdict(&[LINT], &ev).0), Some(LINT));
}

#[test]
fn per_file_semantics_hold_for_a_declared_scanned_set() {
    // Different .py files on each side: each file's verdict is its own.
    let ev = evidence(&[LINT], &["src/b.py"], &["src/a.py"], RepoSpecs::parse(LINT_DECL));
    assert_eq!(verdict(&[LINT], &ev).0, Verdict::Fresh);
    // The same .py file on both sides: the merged content was never linted.
    let ev = evidence(&[LINT], &["src/a.py"], &["src/a.py"], RepoSpecs::parse(LINT_DECL));
    assert_eq!(stale_check(&verdict(&[LINT], &ev).0), Some(LINT));
}

#[test]
fn an_undeclared_sibling_context_still_refuses_on_any_move() {
    let ev = evidence(&[LINT, "Tests"], &["README.md"], &["src/a.py"], RepoSpecs::parse(LINT_DECL));
    assert_eq!(stale_check(&verdict(&[LINT, "Tests"], &ev).0), Some("Tests"));
}

#[test]
fn built_in_specs_win_over_a_declaration_for_the_same_context() {
    // Declares File Size Ratchet as reading nothing the base move touched.
    let decl = r#"{"version": 1, "checks": {"File Size Ratchet": {"global": ["nothing.txt"]}}}"#;
    let ctx = "File Size Ratchet";
    let ev = evidence(
        &[ctx],
        &["scripts/file-size-baseline.txt"],
        &["loom-daemon/src/main_health_gate.rs"],
        RepoSpecs::parse(decl),
    );
    assert_eq!(stale_check(&verdict(&[ctx], &ev).0), Some(ctx));
}

#[test]
fn a_declared_ci_yml_input_is_judged_as_a_whole_file() {
    // A base-side ci.yml attribution that names none of loom's components must
    // not narrow a consumer's check: its job is not in loom's table.
    let mut ev = evidence(
        &[LINT],
        &[".github/workflows/ci.yml"],
        &["src/a.py"],
        RepoSpecs::parse(LINT_DECL),
    );
    for mv in ev.base_moves.values_mut() {
        mv.ci_scope = CiScope::Scoped(std::collections::BTreeSet::new());
    }
    assert_eq!(stale_check(&verdict(&[LINT], &ev).0), Some(LINT));
}

// --- Self-protection ---------------------------------------------------------

#[test]
fn a_declaration_change_on_either_side_interacts_with_every_declared_check() {
    let repo = RepoSpecs::parse(LINT_DECL);
    // Base side changed the declaration; the PR touches a linted file.
    let ev = evidence(&[LINT], &[DECLARATION_PATH], &["src/a.py"], repo.clone());
    assert_eq!(stale_check(&verdict(&[LINT], &ev).0), Some(LINT));
    // The PR changes the declaration; the base touched a linted file.
    let ev = evidence(&[LINT], &["src/b.py"], &[DECLARATION_PATH], repo);
    assert_eq!(stale_check(&verdict(&[LINT], &ev).0), Some(LINT));
}

// --- Malformed: fail closed, loudly ------------------------------------------

#[test]
fn every_malformed_declaration_is_rejected_whole() {
    let cases = [
        ("not json", "{"),
        ("wrong version", r#"{"version": 2, "checks": {"L": {"global": ["a"]}}}"#),
        ("missing version", r#"{"checks": {"L": {"global": ["a"]}}}"#),
        ("unknown top field", r#"{"version": 1, "checks": {}, "extra": 1}"#),
        (
            "unknown spec field",
            r#"{"version": 1, "checks": {"L": {"global": ["a"], "scaned": ["b"]}}}"#,
        ),
        (
            "empty global",
            r#"{"version": 1, "checks": {"L": {"global": [], "scanned": ["**"]}}}"#,
        ),
        ("missing global", r#"{"version": 1, "checks": {"L": {"scanned": ["**"]}}}"#),
        ("empty context", r#"{"version": 1, "checks": {" ": {"global": ["a"]}}}"#),
        (
            "duplicate context",
            r#"{"version": 1, "checks": {"L": {"global": ["a"]}, "L": {"global": ["b"]}}}"#,
        ),
        ("absolute pattern", r#"{"version": 1, "checks": {"L": {"global": ["/etc/x"]}}}"#),
        ("dotdot pattern", r#"{"version": 1, "checks": {"L": {"global": ["../x"]}}}"#),
        ("empty pattern", r#"{"version": 1, "checks": {"L": {"global": [""]}}}"#),
        (
            "brace glob",
            r#"{"version": 1, "checks": {"L": {"global": ["a"], "scanned": ["**/*.{py,pyi}"]}}}"#,
        ),
        (
            "char class",
            r#"{"version": 1, "checks": {"L": {"global": ["a"], "coupled": ["[ab].txt"]}}}"#,
        ),
        (
            "inner doublestar",
            r#"{"version": 1, "checks": {"L": {"global": ["src**/x"]}}}"#,
        ),
        ("backslash", r#"{"version": 1, "checks": {"L": {"global": ["a\\b"]}}}"#),
    ];
    for (name, text) in cases {
        assert!(
            matches!(RepoSpecs::parse(text), RepoSpecs::Rejected(_)),
            "{name}: must be rejected, got {:?}",
            RepoSpecs::parse(text)
        );
    }
}

#[test]
fn a_rejected_declaration_keeps_contexts_stale_and_warns_naming_the_file() {
    let repo = RepoSpecs::parse(r#"{"version": 1, "checks": {"Lint (ruff)": {"global": []}}}"#);
    let ev = evidence(&[LINT], &["README.md"], &["src/a.py"], repo);
    let (v, warnings) = verdict(&[LINT], &ev);
    assert_eq!(stale_check(&v), Some(LINT), "a rejected declaration narrows nothing");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(DECLARATION_PATH), "{warnings:?}");
    assert!(warnings[0].contains("empty `global`"), "{warnings:?}");
}

#[test]
fn a_rejection_warns_once_however_many_contexts_it_affects() {
    let repo = RepoSpecs::Rejected("it could not be read".to_string());
    let ev = evidence(&["A", "B", "C"], &["x"], &["y"], repo);
    assert_eq!(verdict(&["A", "B", "C"], &ev).1.len(), 1);
}

#[test]
fn a_valid_declaration_parses_with_defaults() {
    let RepoSpecs::Declared(map) = RepoSpecs::parse(LINT_DECL) else {
        panic!("valid declaration rejected");
    };
    let spec = &map[LINT];
    assert_eq!(spec.global, vec!["pyproject.toml", ".github/workflows/ci.yml"]);
    assert_eq!(spec.scanned, vec!["**/*.py"]);
    assert!(spec.coupled.is_empty());
    assert!(!spec.removal_sensitive);
}
