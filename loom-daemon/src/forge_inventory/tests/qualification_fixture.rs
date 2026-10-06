//! The Gitea qualification CI fixture is real work, not a green stub (#9790).
//!
//! #9790's first test-plan item: genuinely compile and test the selected
//! fixture, introduce a failing test, and assert the result differs from the
//! passing run. A fixture that "passes" whatever its code does would let a
//! gitea-1 run report a green merge gate that proves nothing, so this test is
//! the local half of that guarantee. The live half — the same fixture on
//! gitea-1 — is recorded in docs/research/gitea-1-ci-delivery-qualification.md.
//!
//! The fixture is copied into a tempdir first, so neither its `Cargo.lock`
//! nor its `target/` ever lands in this checkout.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::workflow_deps::{scan, Plane};

const FIXTURE: &str = "defaults/forge/qualification/ci-fixture";
const WORKFLOW: &str = ".gitea/workflows/qual-ci.yml";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(FIXTURE)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if name == "target" || name == "Cargo.lock" {
            continue;
        }
        let dst = to.join(&name);
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// `cargo test` in `dir`, isolated from this build's target dir.
fn cargo_test(dir: &Path) -> (bool, String) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["test", "--offline", "--quiet", "-j", "1"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn a_failing_test_changes_the_fixture_result() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("fixture");
    copy_tree(&fixture_dir(), &work);

    let (ok, log) = cargo_test(&work);
    assert!(ok, "the unmodified fixture must pass:\n{log}");
    assert!(log.contains("numeric_not_lexical_ordering") || log.contains("test result: ok"));

    // Break the behaviour the tests protect: order versions lexically.
    let lib = work.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    let broken = src.replace(
        "Some(Version::parse(a)?.cmp(&Version::parse(b)?))",
        "Version::parse(a)?;\n    Version::parse(b)?;\n    Some(a.cmp(b))",
    );
    assert_ne!(src, broken, "the mutation must apply");
    std::fs::write(&lib, broken).unwrap();

    let (ok, log) = cargo_test(&work);
    assert!(!ok, "a lexical-ordering regression must fail the fixture:\n{log}");
    assert!(
        log.contains("numeric_not_lexical_ordering"),
        "the failure must name the broken test:\n{log}"
    );
}

/// Static guards on the fixture workflow: it must exercise the action
/// revisions production actually pins, route only to the isolated runner,
/// never call the forge CLI, and never cancel a started commit's run.
#[test]
fn the_fixture_workflow_is_pinned_isolated_and_non_cancelling() {
    let root = fixture_dir();
    let text = std::fs::read_to_string(root.join(WORKFLOW)).unwrap();
    let deps = scan(WORKFLOW, &text);

    let actions: Vec<_> = deps
        .iter()
        .filter(|d| d.plane == Plane::ActionSource)
        .collect();
    assert!(!actions.is_empty());
    assert!(
        actions.iter().all(|d| d.sha_pinned == Some(true)),
        "every fixture action must be SHA-pinned: {actions:#?}"
    );
    assert!(
        !deps.iter().any(|d| d.plane == Plane::ForgeApi),
        "the fixture must not call the forge API: {deps:#?}"
    );

    // Same revisions as the production workflows, so a Gitea result speaks to
    // the actions Loom depends on rather than to a hand-picked older one.
    let prod_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut prod = String::new();
    for e in std::fs::read_dir(prod_dir.join(".github/workflows")).unwrap() {
        prod.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap_or_default());
    }
    for a in &actions {
        assert!(
            prod.contains(&format!("uses: {}", a.reference)),
            "{} is not a revision the production workflows pin",
            a.reference
        );
    }

    let runs_on: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("runs-on:"))
        .map(str::trim)
        .collect();
    assert!(!runs_on.is_empty());
    assert!(
        runs_on.iter().all(|r| *r == "loom-qual"),
        "every job must target the isolated qualification runner label: {runs_on:?}"
    );
    assert!(text.contains("cancel-in-progress: false"));
    assert!(!text.contains("cancel-in-progress: true"));
}
