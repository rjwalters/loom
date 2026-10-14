//! The `CI Result` aggregate's input policy (#10444), and the pin that keeps
//! every configured required context mapped.
//!
//! Before this policy, `CI Result` was a required context with no entry in the
//! table, so `assess_scoped` sent it to [`unknown_check_reason`]: ANY base move
//! — a `README.md` edit included — refused every PR once the ruleset applied it.

use super::*;
use crate::merge_pr::stale_checks::workflow_scope::{self, CiScopes};

/// `.loom/config.json` as it is on this commit — the file
/// `scripts/install/setup-branch-protection.sh` applies to the live ruleset.
const CONFIG_JSON: &str = include_str!("../../../../../.loom/config.json");

const CI_YML: &str = include_str!("../../../../../.github/workflows/ci.yml");

fn set(paths: &[&str]) -> FileSet {
    file_set(paths.iter().map(|p| (*p, false)))
}

fn ci_result() -> Vec<&'static CheckSpec<'static>> {
    specs_for("CI Result").expect("CI Result must resolve to specs")
}

fn configured_required_contexts() -> BTreeSet<String> {
    let v: serde_json::Value = serde_json::from_str(CONFIG_JSON).expect("config.json parses");
    v["branchProtection"]["requiredStatusChecks"]
        .as_array()
        .expect("branchProtection.requiredStatusChecks is an array")
        .iter()
        .map(|c| c.as_str().expect("context is a string").to_string())
        .collect()
}

#[test]
fn every_configured_required_context_is_mapped_in_the_table() {
    // The pin the #10444 review asked for: a context added to the configured
    // ruleset without an input policy here would fall through to
    // `unknown_check_reason` and refuse on every base move. Fail at PR time.
    let configured = configured_required_contexts();
    assert!(!configured.is_empty(), "the parse found no required contexts");
    let listed: BTreeSet<String> = REQUIRED_CONTEXTS.iter().map(|c| (*c).to_string()).collect();
    assert_eq!(
        configured, listed,
        ".loom/config.json requiredStatusChecks and REQUIRED_CONTEXTS (inputs.rs) must name the \
same contexts — give a new required context an input policy before configuring it"
    );
    for ctx in &configured {
        assert!(specs_for(ctx).is_some(), "{ctx}: configured as required but has no specs");
    }
}

#[test]
fn the_aggregate_folds_in_every_component_of_the_contexts_it_aggregates() {
    let req = required_check("CI Result").expect("entry");
    let mut expected: Vec<&str> = req.components.to_vec();
    for agg in req.aggregates {
        let inner = required_check(agg).unwrap_or_else(|| panic!("{agg}: not a required check"));
        assert!(inner.aggregates.is_empty(), "{agg}: aggregates must not nest");
        expected.extend(inner.components.iter().copied());
    }
    let got: Vec<&str> = ci_result().iter().map(|s| s.context).collect();
    assert_eq!(got, expected);
    // Its own component, plus Structural (16), macOS Shell Syntax (1) and
    // Daemon Checks (4).
    assert_eq!(got.len(), 22);
}

#[test]
fn the_aggregate_needs_the_job_of_every_context_it_aggregates() {
    // `aggregates` is only honest if the `ci-result` job really `needs:` those
    // jobs; otherwise their verdicts are not part of what it reports.
    let wf = workflow_scope::parse(CI_YML);
    let job = wf.job_named("CI Result").expect("ci-result job");
    for agg in required_check("CI Result").expect("entry").aggregates {
        let key = &wf
            .job_named(agg)
            .unwrap_or_else(|| panic!("{agg}: no job"))
            .key;
        assert!(job.needs.contains(key), "ci-result must `needs:` {key} ({agg})");
    }
    // …and every other job in the workflow, so its own component's G (the
    // jobs no granular context runs) covers something real.
    for other in &wf.jobs {
        if other.key != job.key {
            assert!(job.needs.contains(&other.key), "ci-result must `needs:` {}", other.key);
        }
    }
}

#[test]
fn an_unrelated_readme_move_does_not_refuse_the_aggregate() {
    // The #10444 review's reproduction: green `CI Result`, PR touches Rust,
    // `main` moved only `README.md`. Unmapped, this refused; mapped, it holds.
    let d = set(&["README.md"]);
    let p = set(&["loom-daemon/src/example.rs"]);
    assert_eq!(composite_stale_reason(&ci_result(), &d, &p, &CiScopes::unscoped()), None);
}

#[test]
fn a_work_plan_move_refuses_the_aggregate_when_a_suite_reading_it_changes() {
    // `test-guide-operator-attention-fold.sh` (Test 3) and
    // `test-docs-worktree.sh` (Test 6) read the committed root `WORK_PLAN.md`
    // and run only under `shell-suite-tests`, which only `CI Result` reports.
    let d = set(&["WORK_PLAN.md"]);
    for pr in [
        "defaults/scripts/tests/test-guide-operator-attention-fold.sh",
        "defaults/scripts/tests/test-docs-worktree.sh",
        "defaults/.claude/commands/loom/guide.md",
    ] {
        let (component, reason) =
            composite_stale_reason(&ci_result(), &d, &set(&[pr]), &CiScopes::unscoped())
                .unwrap_or_else(|| panic!("WORK_PLAN.md move under {pr} must refuse CI Result"));
        assert_eq!(component, "CI Result", "{pr}: {reason:?}");
        assert_eq!(reason.base_path.as_deref(), Some("WORK_PLAN.md"), "{pr}: {reason:?}");
    }
    // The other direction: a Guide PR touching `WORK_PLAN.md` under a code
    // move on `main` refuses too.
    let (component, _) = composite_stale_reason(
        &ci_result(),
        &set(&["defaults/scripts/lib/common.sh"]),
        &set(&["WORK_PLAN.md"]),
        &CiScopes::unscoped(),
    )
    .expect("a WORK_PLAN.md PR under a global move on main must refuse");
    assert_eq!(component, "CI Result");
    // It is coupled, not global: a Guide docs refresh on `main` does not
    // refuse a PR that touches neither `WORK_PLAN.md` nor a global input.
    let own = ci_result()
        .into_iter()
        .find(|s| s.context == "CI Result")
        .expect("own component");
    assert_eq!(
        stale_reason(
            own,
            &set(&["WORK_PLAN.md", "WORK_LOG.md", "README.md"]),
            &set(&["docs/guides/unrelated.md"]),
        ),
        None,
        "a WORK_PLAN.md move must not refuse CI Result's own component under an unrelated PR"
    );
}

#[test]
fn a_rust_move_under_a_rust_pr_refuses_the_aggregate_on_its_own_component() {
    // The inputs the granular contexts do NOT model: backend-tests compiles
    // the whole workspace, so two disjoint Rust edits can still conflict.
    let d = set(&["loom-daemon/src/a.rs"]);
    let p = set(&["loom-daemon/src/b.rs"]);
    let (component, reason) =
        composite_stale_reason(&ci_result(), &d, &p, &CiScopes::unscoped()).expect("stale");
    assert_eq!(component, "CI Result");
    assert!(reason.clause.contains("global input"), "{reason:?}");
}

#[test]
fn shell_suite_and_node_inputs_refuse_the_aggregate() {
    for (moved, pr) in [
        ("defaults/scripts/lib/common.sh", "defaults/scripts/tests/test-foo.sh"),
        ("mcp-loom/src/index.ts", "mcp-loom/package.json"),
        ("docker/worker/Dockerfile", "defaults/scripts/run-job.sh"),
        ("scripts/ci-result-gate.sh", "loom-daemon/src/x.rs"),
        ("CLAUDE.md", "defaults/scripts/tests/test-champion-premise-false-close.sh"),
    ] {
        let stale = composite_stale_reason(
            &ci_result(),
            &set(&[moved]),
            &set(&[pr]),
            &CiScopes::unscoped(),
        );
        assert!(stale.is_some(), "{moved} under {pr} must refuse CI Result");
    }
}

#[test]
fn a_granular_components_refusal_carries_into_the_aggregate() {
    // The #8078 shape: main tightens the file-size baseline under a Rust PR.
    // `File Size Ratchet` refuses; so must the aggregate that folds it in.
    let d = set(&["scripts/file-size-baseline.txt"]);
    let p = set(&["docs/adr/0020-something.md"]);
    // A doc-only PR gives neither the own component nor the ratchet anything
    // to re-judge…
    assert_eq!(composite_stale_reason(&ci_result(), &d, &p, &CiScopes::unscoped()), None);
    // …but with a measured file in the PR the folded-in ratchet refuses, and
    // does so through the aggregate even with the own component removed.
    let folded: Vec<&'static CheckSpec<'static>> = ci_result()
        .into_iter()
        .filter(|s| s.context != "CI Result")
        .collect();
    let p2 = set(&["loom-daemon/src/main_health_gate.rs"]);
    let (component, _) =
        composite_stale_reason(&folded, &d, &p2, &CiScopes::unscoped()).expect("stale");
    assert_eq!(component, "File Size Ratchet");
    let p = set(&[
        "docs/adr/0020-something.md",
        "loom-daemon/src/main_health_gate.rs",
    ]);
    let stale = composite_stale_reason(&ci_result(), &d, &p, &CiScopes::unscoped());
    assert!(stale.is_some(), "the ratchet's refusal must surface through CI Result");
    // And a doc-only base move under a doc-only PR still reaches the link
    // check through the aggregate.
    let (component, _) = composite_stale_reason(
        &ci_result(),
        &set(&["docs/guides/testing.md"]),
        &set(&["docs/guides/development.md"]),
        &CiScopes::unscoped(),
    )
    .expect("both sides touch the link graph");
    assert_ne!(component, "CI Result", "the markdown coupling is a Structural component's");
}
