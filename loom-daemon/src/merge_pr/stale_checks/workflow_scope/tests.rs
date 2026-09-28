//! Unit tests for the `ci.yml` block attribution (#9065).
//!
//! Every test runs against the **real** `ci.yml` compiled into the binary, for
//! the same reason the input-table pin does: an attribution derived from a
//! hand-written fixture would keep passing after the workflow was restructured
//! underneath it, which is precisely the drift this exists to survive.

use super::*;
use crate::merge_pr::stale_checks::evidence::ChangedFile;
use crate::merge_pr::stale_checks::inputs::{spec_for, REQUIRED_CHECKS};

const CI_YML: &str = include_str!("../../../../../.github/workflows/ci.yml");

fn workflow() -> Workflow {
    parse(CI_YML)
}

/// The 1-based line number of the single line equal to `needle`, panicking
/// when it is absent or ambiguous — a test that silently matched the wrong
/// line would assert nothing.
fn line_of(needle: &str) -> usize {
    let hits: Vec<usize> = CI_YML
        .lines()
        .enumerate()
        .filter(|(_, l)| *l == needle)
        .map(|(i, _)| i + 1)
        .collect();
    assert_eq!(hits.len(), 1, "{needle:?} matched {} lines", hits.len());
    hits[0]
}

/// A patch that adds one line at `line` and touches nothing else.
fn add_at(line: usize) -> String {
    format!("@@ -{line},0 +{line},1 @@\n+      # added by this base move\n")
}

fn ci_file(status: &str, patch: Option<&str>) -> ChangedFile {
    ChangedFile {
        path: CI_WORKFLOW.to_string(),
        status: status.to_string(),
        previous_filename: None,
        patch: patch.map(String::from),
    }
}

fn scoped(patch: &str) -> CiScope {
    scope_for_patch(&workflow(), patch)
}

fn affected(patch: &str) -> BTreeSet<String> {
    match scoped(patch) {
        CiScope::Scoped(set) => set,
        CiScope::Unscoped => panic!("expected a scoped answer, got Unscoped"),
    }
}

// --- The parse ---------------------------------------------------------------

#[test]
fn the_three_required_contexts_resolve_to_jobs_with_their_components() {
    let wf = workflow();
    for req in REQUIRED_CHECKS {
        let job = wf
            .job_named(req.context)
            .unwrap_or_else(|| panic!("{}: no job", req.context));
        if job.components.is_empty() {
            // A single-gate job (the macOS Shell Syntax leg) carries no marker.
            assert_eq!(req.components.len(), 1, "{}", req.context);
            continue;
        }
        let names: BTreeSet<&str> = job.components.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            req.components.iter().copied().collect::<BTreeSet<&str>>(),
            "{}: marker set",
            req.context
        );
    }
}

#[test]
fn component_ranges_are_disjoint_and_inside_their_job() {
    for job in &workflow().jobs {
        let mut prev_end = job.start;
        for c in &job.components {
            assert!(c.start > prev_end, "{}/{}: overlapping block", job.key, c.name);
            assert!(c.end >= c.start && c.end <= job.end, "{}/{}: range", job.key, c.name);
            prev_end = c.end;
        }
    }
}

#[test]
fn needs_are_read_from_the_key_indented_line_only() {
    let wf = workflow();
    let daemon = wf.job_named("Daemon Checks").expect("Daemon Checks");
    assert_eq!(daemon.needs, vec!["build-daemon".to_string(), "changes".to_string()]);
    // `shell-syntax`'s prose says "it needs a macOS runner"; that is not a
    // `needs:` key and must not be read as one.
    let macos = wf
        .job_named("Shell Syntax (macos-latest)")
        .expect("macOS leg");
    assert!(macos.needs.is_empty(), "{:?}", macos.needs);
}

#[test]
fn every_needs_names_a_job_this_workflow_defines() {
    // The closure walk fails closed on a dangling `needs:`; this asserts the
    // real workflow never puts it in that state (which would silently disable
    // the narrowing).
    let wf = workflow();
    for job in &wf.jobs {
        assert!(
            wf.needs_closure(&job.key).is_some(),
            "{}: needs {:?} names a job ci.yml does not define",
            job.key,
            job.needs
        );
    }
}

// --- The narrowing this issue exists for -------------------------------------

#[test]
fn a_base_move_editing_an_unrequired_job_affects_no_component() {
    // THE #9065 FINDING. `backend-tests` is not a required context and no
    // required job needs it, so editing its block cannot change what any
    // required gate would say — yet before this narrowing it made every open
    // PR that touched anything stale, because `ci.yml` was one path in `G`.
    let line = line_of("  backend-tests:") + 3;
    assert!(affected(&add_at(line)).is_empty());
}

#[test]
fn a_base_move_inside_one_component_block_affects_only_that_component() {
    let wf = workflow();
    let job = wf
        .job_named("Structural Checks")
        .expect("Structural Checks");
    let target = job
        .components
        .iter()
        .find(|c| c.name == "Dangling Link Check")
        .expect("marker");
    // A line strictly inside the block, so the deletion-boundary widening of
    // `changed_new_lines` is not what is being measured here.
    let hit = affected(&add_at(target.start + 2));
    assert_eq!(
        hit,
        ["Dangling Link Check".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
}

#[test]
fn a_base_move_in_a_composite_jobs_setup_affects_all_its_components() {
    let wf = workflow();
    let job = wf
        .job_named("Structural Checks")
        .expect("Structural Checks");
    let first = job.components.first().expect("markers").start;
    let hit = affected(&add_at(first - 1));
    let expected: BTreeSet<String> = REQUIRED_CHECKS
        .iter()
        .find(|r| r.context == "Structural Checks")
        .expect("entry")
        .components
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    assert_eq!(hit, expected, "a shared checkout step feeds every gate in the job");
}

#[test]
fn a_base_move_in_a_needed_job_affects_the_components_that_need_it() {
    // `daemon-checks` needs `build-daemon`; `structural-checks` does not.
    let line = line_of("  build-daemon:") + 2;
    let hit = affected(&add_at(line));
    let daemon: BTreeSet<String> = REQUIRED_CHECKS
        .iter()
        .find(|r| r.context == "Daemon Checks")
        .expect("entry")
        .components
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    assert_eq!(hit, daemon, "the shared daemon binary is an input to the gates that run it");
}

#[test]
fn a_component_name_in_the_scope_always_names_a_real_spec() {
    // The scope keys on `CheckSpec::context`; a typo would silently make the
    // narrowing drop a real global-input move.
    let wf = workflow();
    let line = wf
        .job_named("Structural Checks")
        .expect("job")
        .components
        .first()
        .expect("markers")
        .start
        - 1;
    for name in affected(&add_at(line)) {
        assert!(spec_for(&name).is_some(), "{name:?} has no CheckSpec");
    }
}

// --- Fail-closed -------------------------------------------------------------

#[test]
fn a_preamble_edit_is_unscoped() {
    // `env:` / `on:` / `concurrency:` apply to every job in the file.
    for anchor in ["env:", "concurrency:", "jobs:"] {
        assert_eq!(
            scoped(&add_at(line_of(anchor))),
            CiScope::Unscoped,
            "editing {anchor} must keep the whole-file meaning"
        );
    }
}

#[test]
fn a_structural_deletion_is_unscoped() {
    for removed in [
        "-  backend-tests:",
        "-env:",
        "-      # component: Dangling Link Check",
    ] {
        let patch = format!("@@ -100,1 +100,0 @@\n{removed}\n");
        assert_eq!(scoped(&patch), CiScope::Unscoped, "{removed}");
    }
}

#[test]
fn an_ordinary_deletion_is_attributed_not_refused() {
    // Only a STRUCTURAL deletion is unattributable; deleting a step's body
    // line is charged to the block it sat in.
    let wf = workflow();
    let target = wf
        .job_named("Structural Checks")
        .expect("job")
        .components
        .iter()
        .find(|c| c.name == "Dangling Link Check")
        .expect("marker")
        .start
        + 3;
    let patch = format!("@@ -{target},1 +{target},0 @@\n-        echo removed\n");
    assert_eq!(
        scoped(&patch),
        CiScope::Scoped(
            ["Dangling Link Check".to_string()]
                .into_iter()
                .collect::<BTreeSet<_>>()
        )
    );
}

#[test]
fn a_hunk_past_the_end_of_the_tip_file_is_unscoped() {
    // The patch and the fetched tip disagree about the file — attribute
    // nothing.
    let beyond = CI_YML.lines().count() + 50;
    assert_eq!(scoped(&add_at(beyond)), CiScope::Unscoped);
}

#[test]
fn an_unparseable_or_empty_patch_is_unscoped() {
    for patch in [
        "",                                   // nothing at all
        "@@ nonsense @@\n+x\n",               // no `+start`
        "@@ -1,1 +1,1 @@\n?  weird marker\n", // an unrecognised body marker
        "@@ -0,0 +0,0 @@\n+x\n",              // a start of 0 owns no line
    ] {
        assert_eq!(scoped(patch), CiScope::Unscoped, "{patch:?}");
    }
}

#[test]
fn only_a_modified_ci_yml_with_a_patch_is_narrowed() {
    let wf = workflow();
    let ok = add_at(line_of("  backend-tests:") + 3);
    // The narrowing applies.
    assert!(matches!(
        scope_for_files(&wf, &[ci_file("modified", Some(&ok))]),
        CiScope::Scoped(_)
    ));
    // …and every other shape keeps the whole-file meaning.
    for f in [
        ci_file("modified", None), // GitHub suppressed the patch
        ci_file("added", Some(&ok)),
        ci_file("removed", Some(&ok)),
        ci_file("renamed", Some(&ok)),
    ] {
        assert_eq!(
            scope_for_files(&wf, std::slice::from_ref(&f)),
            CiScope::Unscoped,
            "{:?}",
            f.status
        );
    }
    // A base move that did not touch ci.yml at all has nothing to narrow.
    assert_eq!(
        scope_for_files(
            &wf,
            &[ChangedFile {
                path: "README.md".to_string(),
                status: "modified".to_string(),
                previous_filename: None,
                patch: Some("@@ -1,1 +1,1 @@\n-a\n+b\n".to_string()),
            }]
        ),
        CiScope::Unscoped
    );
}

#[test]
fn unscoped_affects_everything_and_scoped_affects_only_its_members() {
    assert!(CiScope::Unscoped.affects("anything at all"));
    assert_eq!(CiScope::default(), CiScope::Unscoped);
    let scope = CiScope::Scoped(["File Size Ratchet".to_string()].into_iter().collect());
    assert!(scope.affects("File Size Ratchet"));
    assert!(!scope.affects("Dangling Link Check"));
    assert!(!CiScope::Scoped(BTreeSet::new()).affects("File Size Ratchet"));
}

#[test]
fn a_required_job_whose_markers_disagree_with_the_table_is_unscoped() {
    // The version-skew hole this invariant closes: `main` adds a gate to a
    // required job AFTER this binary was built, so REQUIRED_CHECKS does not
    // list it. Attributing per component would place that gate's `ci.yml` edit
    // with nobody and narrow it away — while a NEW required gate had in fact
    // never run against the PR's tree. An unrecognised marker set therefore
    // restores the whole-file meaning instead.
    let wf = workflow();
    let job = wf.job_named("Structural Checks").expect("job");
    // Splice the new marker in immediately after an existing one, so the file
    // still parses as a workflow and only the marker SET changes.
    let anchor = job.components.first().expect("markers").start;
    let mut lines: Vec<String> = CI_YML.lines().map(String::from).collect();
    lines.insert(anchor, "      # component: Brand New Gate".to_string());
    let spliced = parse(&lines.join("\n"));
    assert!(
        spliced
            .job_named("Structural Checks")
            .expect("job")
            .components
            .iter()
            .any(|c| c.name == "Brand New Gate"),
        "precondition: the splice added a marker the table does not list"
    );
    // Even an edit deep inside an unrelated, unrequired job is now unscoped.
    let line = line_of("  backend-tests:") + 3;
    assert_eq!(scope_for_patch(&spliced, &add_at(line + 1)), CiScope::Unscoped);
}

#[test]
fn a_single_gate_job_that_grows_markers_is_unscoped() {
    // The mirror case: the macOS Shell Syntax leg runs ONE gate and carries no
    // marker, so the table lists exactly its own context. If the tip splits it
    // into components, per-component attribution is no longer derivable from
    // this binary's table.
    let wf = workflow();
    let job = wf.job_named("Shell Syntax (macos-latest)").expect("job");
    let mut lines: Vec<String> = CI_YML.lines().map(String::from).collect();
    lines.insert(job.start + 2, "      # component: Some New Split".to_string());
    let spliced = parse(&lines.join("\n"));
    let line = line_of("  backend-tests:") + 3;
    assert_eq!(scope_for_patch(&spliced, &add_at(line)), CiScope::Unscoped);
}

#[test]
fn a_workflow_missing_a_required_job_is_unscoped() {
    // The guard reads the TIP's ci.yml; if a required context's job is not in
    // it, the component cannot be located and the old meaning must return.
    let wf = parse(
        "name: CI\njobs:\n  other:\n    name: Something Else\n    steps:\n      - run: true\n",
    );
    assert_eq!(scope_for_patch(&wf, "@@ -6,0 +6,1 @@\n+      - run: more\n"), CiScope::Unscoped);
}
