//! Unit tests for the input-scoped staleness predicate and its table (#8919).
//!
//! The pin test at the bottom is the load-bearing one: the table is
//! hand-maintained, so the only thing keeping it honest as `ci.yml` changes is
//! a test that reads `ci.yml` and fails when a required job grows a step the
//! table does not know about.

use super::*;

fn set(paths: &[&str]) -> FileSet {
    file_set(paths.iter().map(|p| (*p, false)))
}

fn set_with_removal(paths: &[&str], removed: &[&str]) -> FileSet {
    let mut s = set(paths);
    for r in removed {
        s.paths.insert((*r).to_string());
        s.removed.insert((*r).to_string());
    }
    s
}

fn spec(ctx: &str) -> &'static CheckSpec<'static> {
    spec_for(ctx).unwrap_or_else(|| panic!("{ctx} must have a spec"))
}

// --- The #8078/#8204 incident, under the new predicate -----------------------

#[test]
fn the_8078_8204_timeline_is_refused_on_inputs_not_on_timing() {
    // B = main at 09-17 with baseline 1845; D = #8204 tightening the baseline
    // (a G input); P = the PR's own main_health_gate.rs (an S path).
    let d = set(&["scripts/file-size-baseline.txt"]);
    let p = set(&["loom-daemon/src/main_health_gate.rs"]);
    let reason = stale_reason(spec("File Size Ratchet"), &d, &p)
        .expect("a tightened baseline under an in-flight source change is stale");
    assert!(reason.clause.contains("global input"), "{reason:?}");
    assert_eq!(reason.base_path.as_deref(), Some("scripts/file-size-baseline.txt"));
    assert_eq!(reason.pr_path.as_deref(), Some("loom-daemon/src/main_health_gate.rs"));
}

#[test]
fn a_baseline_tightening_with_no_measured_file_in_the_pr_is_fresh() {
    // The narrowing that makes #8919 worth doing: main tightened the ratchet
    // baseline, but this PR touches nothing the ratchet measures, so the merged
    // tree's ratchet verdict is main's own — not this PR's business.
    let d = set(&["scripts/file-size-baseline.txt"]);
    let p = set(&["docs/adr/0020-something.md"]);
    assert_eq!(stale_reason(spec("File Size Ratchet"), &d, &p), None);
}

// --- Per-file (S) checks -----------------------------------------------------

#[test]
fn disjoint_per_file_moves_are_fresh_and_an_overlap_is_stale() {
    let s = spec("File Size Ratchet");
    // Disjoint: main grew a.rs, the PR grew b.rs. Each file's verdict depends
    // only on its own content, so neither side can change the other's.
    let d = set(&["loom-daemon/src/a.rs"]);
    let p = set(&["loom-daemon/src/b.rs"]);
    assert_eq!(stale_reason(s, &d, &p), None);

    // Overlap: both sides touched the SAME measured file, so the merged
    // content is neither side's tested content.
    let d = set(&["loom-daemon/src/a.rs", "loom-daemon/src/c.rs"]);
    let p = set(&["loom-daemon/src/b.rs", "loom-daemon/src/c.rs"]);
    let reason = stale_reason(s, &d, &p).expect("an overlapping measured file is stale");
    assert_eq!(reason.base_path.as_deref(), Some("loom-daemon/src/c.rs"));
    assert!(reason.clause.contains("same scanned file"), "{reason:?}");
}

#[test]
fn a_base_move_outside_every_input_of_the_check_is_fresh() {
    // `Conflict Marker Check` scans every tracked file, so nothing is outside
    // its S — but `CLAUDE.md Line Budget` reads exactly one file.
    let d = set(&["README.md", "loom-daemon/src/x.rs"]);
    let p = set(&["loom-daemon/src/y.rs"]);
    assert_eq!(stale_reason(spec("CLAUDE.md Line Budget"), &d, &p), None);
    // …and the same move IS stale for the everything-scanner only when the two
    // sides share a file.
    let reason = stale_reason(spec("Conflict Marker Check"), &d, &set(&["README.md"]));
    assert!(reason.is_some(), "a shared scanned file is stale");
}

// --- Coupled (C) checks ------------------------------------------------------

#[test]
fn a_coupled_aggregate_check_is_stale_when_both_sides_touch_the_set() {
    // Role Prompt Prefix Ratchet sums a role's WHOLE file set, so two disjoint
    // per-file edits still move the aggregate.
    let s = spec("Role Prompt Prefix Ratchet");
    let d = set(&["defaults/.claude/commands/loom/builder.md"]);
    let p = set(&["defaults/.claude/commands/loom/judge.md"]);
    let reason = stale_reason(s, &d, &p).expect("disjoint files still move one SUM");
    assert!(reason.clause.contains("coupled"), "{reason:?}");

    // One side outside the coupled set entirely is still fresh.
    assert_eq!(stale_reason(s, &d, &set(&["loom-daemon/src/x.rs"])), None);
}

#[test]
fn the_generated_pair_check_couples_only_its_own_pair() {
    let s = spec("AGENTS.md Sync Check");
    assert!(stale_reason(s, &set(&["CLAUDE.md"]), &set(&["AGENTS.md"])).is_some());
    assert_eq!(stale_reason(s, &set(&["CLAUDE.md"]), &set(&["defaults/docs/x.md"])), None);
}

// --- The removal clause ------------------------------------------------------

#[test]
fn the_removal_clause_fires_for_the_link_check_in_both_directions() {
    let s = spec("Dangling Link Check");
    // main DELETED a script (not itself a link source) while this PR edits a
    // doc: the PR's doc may now name a path that no longer exists.
    let d = set_with_removal(&[], &["scripts/retired-helper.sh"]);
    let p = set(&["defaults/docs/troubleshooting.md"]);
    let reason = stale_reason(s, &d, &p).expect("a deleted link target is stale");
    assert!(reason.clause.contains("deleted or renamed"), "{reason:?}");

    // And with the roles swapped: this PR deletes a target while main edits docs.
    let d = set(&["defaults/docs/troubleshooting.md"]);
    let p = set_with_removal(&[], &["scripts/retired-helper.sh"]);
    assert!(stale_reason(s, &d, &p).is_some());

    // A deletion with NOTHING in the coupled set on the other side is fresh.
    let d = set_with_removal(&[], &["scripts/retired-helper.sh"]);
    assert_eq!(stale_reason(s, &d, &set(&["loom-daemon/src/x.rs"])), None);
}

#[test]
fn a_check_that_is_not_removal_sensitive_ignores_a_bare_deletion() {
    let s = spec("CLAUDE.md Line Budget");
    assert!(!s.removal_sensitive);
    let d = set_with_removal(&[], &["some/file.txt"]);
    assert_eq!(stale_reason(s, &d, &set(&["CLAUDE.md"])), None);
}

// --- Fail-closed cases -------------------------------------------------------

#[test]
fn an_unknown_required_check_is_stale_whenever_the_base_moved() {
    assert!(
        spec_for("Some Future Required Job").is_none(),
        "the fixture must not accidentally exist"
    );
    let reason = unknown_check_reason(&set(&["anything.txt"]))
        .expect("an unmapped check with a moved base is an unknown, and unknowns refuse");
    assert!(reason.clause.contains("no entry in the input-scope table"), "{reason:?}");
    // With an EMPTY base move there is nothing to be unknown about.
    assert_eq!(unknown_check_reason(&FileSet::default()), None);
}

#[test]
fn an_empty_base_move_is_fresh_for_every_required_check() {
    // The end state a validated version restamp produces: D empties out, and
    // the tree the check tested IS the tree it will merge onto.
    let p = set(&[
        "CLAUDE.md",
        "loom-daemon/src/x.rs",
        "defaults/roles/builder.md",
        "scripts/file-size-baseline.txt",
        ".gitignore",
        "AGENTS.md",
    ]);
    for ctx in REQUIRED_CONTEXTS {
        let specs = specs_for(ctx).unwrap_or_else(|| panic!("{ctx} must resolve"));
        assert_eq!(
            composite_stale_reason(&specs, &FileSet::default(), &p, &CiScopes::unscoped()),
            None,
            "{ctx} must be fresh when the base did not move"
        );
    }
}

#[test]
fn a_global_input_touched_by_the_pr_alone_is_fresh() {
    // Clause 2 needs BOTH sides: the PR editing a ratchet script while main
    // moved somewhere irrelevant does not invalidate the tested verdict.
    let s = spec("File Size Ratchet");
    let p = set(&["scripts/check-file-size-budget.sh"]);
    assert_eq!(stale_reason(s, &set(&["README.md"]), &p), None);
    // …but main touching anything the check reads makes it stale.
    assert!(stale_reason(s, &set(&["loom-daemon/src/x.rs"]), &p).is_some());
}

#[test]
fn the_version_bump_check_reads_only_its_own_script() {
    // Its verdict is `merge-base(base, head)..head` on the PR's own commits, so
    // a version-bearing file changing on `main` is NOT an input to it.
    let s = spec("PRs Must Not Hand-Edit Version-Bearing Files");
    let d = set(&["VERSION", "Cargo.lock", "mcp-loom/package.json"]);
    let p = set(&["loom-daemon/src/x.rs", "VERSION"]);
    assert_eq!(stale_reason(s, &d, &p), None);
    // Both sides touching its script is the only interaction it has.
    let script = set(&["defaults/scripts/check-defaults-version-bump.sh"]);
    assert!(stale_reason(s, &script, &script).is_some());
}

#[test]
fn version_is_a_global_input_to_the_shell_syntax_legs() {
    // check-daemon-subcommand-versions.sh compares `requires-daemon >= X`
    // against VERSION. A VALIDATED increasing restamp is stripped from D before
    // this ever runs; anything else that edits VERSION lands here.
    for leg in [
        "Shell Syntax (ubuntu-latest)",
        "Shell Syntax (macos-latest)",
    ] {
        let s = spec(leg);
        assert!(s.global.contains(&"VERSION"), "{leg} must treat VERSION as global");
        let reason = stale_reason(s, &set(&["VERSION"]), &set(&["scripts/new-helper.sh"]));
        assert!(reason.is_some(), "{leg}: an unvalidated VERSION move is stale");
    }
}

// --- Glob matching -----------------------------------------------------------

#[test]
fn glob_patterns_match_the_shapes_the_table_uses() {
    for (pat, path, want) in [
        ("VERSION", "VERSION", true),
        ("VERSION", "VERSION.txt", false),
        ("**/*.sh", "scripts/a.sh", true),
        ("**/*.sh", "a.sh", true),
        ("**/*.sh", "scripts/a/b/c.sh", true),
        ("**/*.sh", "scripts/a.rs", false),
        ("defaults/docs/*.md", "defaults/docs/x.md", true),
        ("defaults/docs/*.md", "defaults/docs/sub/x.md", false),
        ("loom-daemon/**", "loom-daemon/src/a/b.rs", true),
        ("loom-daemon/**", "loom-daemon", true),
        ("loom-daemon/**", "loom-api/src/a.rs", false),
        ("docs/adr/0016-*.md", "docs/adr/0016-write-target.md", true),
        ("docs/adr/0016-*.md", "docs/adr/0017-other.md", false),
        (
            "tests/hooks/test-guard-destructive*.sh",
            "tests/hooks/test-guard-destructive-generic.sh",
            true,
        ),
        ("tests/hooks/test-guard-destructive*.sh", "tests/hooks/test-other.sh", false),
        ("**", "anything/at/all.txt", true),
    ] {
        assert_eq!(glob_match(pat, path), want, "glob_match({pat:?}, {path:?})");
    }
}

// --- The table <-> ci.yml pin ------------------------------------------------

/// `ci.yml` as it is on this commit. Compiled in so the pin cannot drift from
/// the workflow it pins — a test that read the file at runtime would pass on a
/// host where the path resolved to something else.
const CI_YML: &str = include_str!("../../../../../.github/workflows/ci.yml");

/// Every `scripts/…​.sh` / `defaults/scripts/…​.sh` a job's non-comment lines
/// name. Paths under another prefix (`.loom/scripts/…`) are the installed
/// copies, not this repo's sources, and are deliberately not collected.
fn script_refs(lines: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in lines {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let bytes = line.as_bytes();
        let mut from = 0;
        while let Some(rel) = line[from..].find("scripts/") {
            let anchor = from + rel;
            let mut start = anchor;
            while start > 0 && is_path_byte(bytes[start - 1]) {
                start -= 1;
            }
            let mut end = anchor + "scripts/".len();
            while end < bytes.len() && is_path_byte(bytes[end]) {
                end += 1;
            }
            let tok = &line[start..end];
            if tok.ends_with(".sh")
                && (tok.starts_with("scripts/") || tok.starts_with("defaults/scripts/"))
            {
                out.insert(tok.to_string());
            }
            from = end.max(anchor + 1);
        }
    }
    out
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/')
}

#[test]
fn the_table_covers_exactly_mains_required_contexts() {
    let listed: BTreeSet<&str> = REQUIRED_CONTEXTS.iter().copied().collect();
    let checks: BTreeSet<&str> = REQUIRED_CHECKS.iter().map(|r| r.context).collect();
    assert_eq!(listed.len(), REQUIRED_CONTEXTS.len(), "duplicate in REQUIRED_CONTEXTS");
    assert_eq!(
        listed, checks,
        "REQUIRED_CONTEXTS and REQUIRED_CHECKS must name the same contexts"
    );
    // Every component has a spec, and every spec is a component of exactly one
    // required check: an orphan spec is dead weight, and a gate in two
    // contexts would be judged twice from two different logs.
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for req in REQUIRED_CHECKS {
        assert!(!req.components.is_empty(), "{}: no components", req.context);
        for c in req.components {
            assert!(spec_for(c).is_some(), "{}: component {c:?} has no CheckSpec", req.context);
            if let Some(prev) = owner.insert(c, req.context) {
                panic!("component {c:?} is in both {prev:?} and {:?}", req.context);
            }
        }
    }
    for sp in SPECS {
        assert!(
            owner.contains_key(sp.context),
            "{:?} has a spec but no required check runs it — remove it or add it to REQUIRED_CHECKS",
            sp.context
        );
    }
    let unique: BTreeSet<&str> = SPECS.iter().map(|s| s.context).collect();
    assert_eq!(unique.len(), SPECS.len(), "duplicate component in SPECS");
}

#[test]
fn every_required_context_names_a_real_ci_yml_job() {
    let wf = super::super::workflow_scope::parse(CI_YML);
    let names: BTreeSet<&str> = wf
        .jobs
        .iter()
        .flat_map(|j| j.names.iter().map(String::as_str))
        .collect();
    assert!(names.len() > 10, "the ci.yml parser found only {names:?}");
    for ctx in REQUIRED_CONTEXTS {
        assert!(
            names.contains(ctx),
            "required context {ctx:?} matches no job name in ci.yml — either the ruleset or this \
table is out of date"
        );
    }
}

#[test]
fn every_script_a_required_job_runs_is_a_global_input() {
    // The pin that makes the table maintainable: adding a step to a required
    // job fails HERE, at PR time, instead of silently making the guard trust a
    // check whose new input it cannot see. Scripts are pinned to the COMPONENT
    // whose marker they sit under, not merely to the composite context — and
    // to the SAME parse of `ci.yml` the freshness guard itself attributes a
    // base move's hunks with (#9065), so the pin and the guard can never read
    // the workflow differently.
    let wf = super::super::workflow_scope::parse(CI_YML);
    let mut pinned = 0;
    for req in REQUIRED_CHECKS {
        let job = wf
            .job_named(req.context)
            .unwrap_or_else(|| panic!("{}: no ci.yml job", req.context));
        // A single-gate job carries no marker: its whole body is that gate's.
        let groups: Vec<(&str, Vec<&str>)> = if job.components.is_empty() {
            assert_eq!(
                req.components.len(),
                1,
                "{}: a job with no `# component:` markers must run exactly one gate",
                req.context
            );
            vec![(req.components[0], job.lines.iter().map(String::as_str).collect())]
        } else {
            job.components
                .iter()
                .map(|c| (c.name.as_str(), job.component_lines(&c.name)))
                .collect()
        };
        let marked: BTreeSet<&str> = groups.iter().map(|(n, _)| *n).collect();
        let expected: BTreeSet<&str> = req.components.iter().copied().collect();
        assert_eq!(
            marked, expected,
            "{}: the job's `# component:` markers must name exactly its REQUIRED_CHECKS components",
            req.context
        );
        // In a COMPOSITE job, nothing before the first marker may run a gate
        // script: such a step belongs to no component, so no spec would cover
        // its input. A single-gate job has no markers and no setup region —
        // its whole body is the gate's, and was pinned as such above.
        if !job.components.is_empty() {
            let setup = script_refs(&job.setup_lines());
            assert!(
                setup.is_empty(),
                "{}: {setup:?} run before the first `# component:` marker, so no spec covers them",
                req.context
            );
        }
        for (component, lines) in &groups {
            let refs = script_refs(lines);
            let spec = spec(component);
            pinned += 1;
            // These three run only the built daemon, no script; their G set
            // carries the subcommand's Rust surface instead, pinned by
            // `daemon_surface_tests.rs`.
            assert!(
                !refs.is_empty()
                    || matches!(
                        *component,
                        "Shell Budget Ratchet" | "Secret Scan" | "MCP Guard Wiring Contract"
                    ),
                "{component}: no script references found — the parser probably broke"
            );
            for script in refs {
                assert!(
                    spec.global.contains(&script.as_str()),
                    "{component} (in {}) runs {script} but it is not in that component's G set \
(inputs.rs). A step added to a required job must be reflected in the table, or the freshness \
guard will not notice when `main` changes that script.",
                    req.context
                );
            }
        }
    }
    assert_eq!(pinned, SPECS.len(), "every component must be pinned against ci.yml");
}

// --- Composite contexts (#9065) ----------------------------------------------

#[test]
fn a_composite_context_is_stale_when_any_component_is() {
    let specs = specs_for("Structural Checks").expect("composite resolves");
    assert_eq!(specs.len(), 16);
    // main tightens the file-size baseline; the PR edits a Rust source file.
    let d = set(&["scripts/file-size-baseline.txt"]);
    let p = set(&["loom-daemon/src/lib.rs"]);
    let (component, _) =
        composite_stale_reason(&specs, &d, &p, &CiScopes::unscoped()).expect("stale");
    assert_eq!(component, "File Size Ratchet");
}

#[test]
fn a_composite_context_is_not_stale_on_cross_component_terms() {
    // main changes the CLAUDE.md budget script (a global input of CLAUDE.md
    // Line Budget only); the PR edits a role prompt, which CLAUDE.md Line
    // Budget never reads. The UNION of all sixteen specs would call this stale
    // (main moved a global input; the PR touches some input). Per-component OR
    // does not: no single gate's verdict could have changed.
    let specs = specs_for("Structural Checks").expect("composite resolves");
    let d = set(&["scripts/check-claude-md-budget.sh"]);
    let p = set(&["defaults/roles/curator.md"]);
    let union_global: Vec<&str> = specs
        .iter()
        .flat_map(|s| s.global.iter().copied())
        .collect();
    assert!(
        d.first_match(&union_global).is_some(),
        "precondition: the union WOULD see a global-input move"
    );
    assert!(
        spec("CLAUDE.md Line Budget")
            .scanned
            .iter()
            .all(|pat| !glob_match(pat, "defaults/roles/curator.md")),
        "precondition: the moved gate does not read the PR's file"
    );
    let stale = composite_stale_reason(&specs, &d, &p, &CiScopes::unscoped());
    assert!(stale.is_none(), "cross-component terms must not refuse: {stale:?}");
}

#[test]
fn a_component_name_still_resolves_to_its_own_spec() {
    // Historical evidence (and the #8078 incident fixtures) name the gate, not
    // the composite; both must resolve.
    let one = specs_for("File Size Ratchet").expect("component resolves");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].context, "File Size Ratchet");
    assert!(specs_for("No Such Check").is_none());
}

#[test]
fn every_spec_lists_the_ci_workflow_as_a_global_input() {
    // The job definition itself is an input to every check it defines.
    for s in SPECS {
        assert!(s.global.contains(&CI_WORKFLOW), "{}: ci.yml must be a global input", s.context);
    }
}

// --- The `P`-side ci.yml narrowing (#9065) -----------------------------------

/// `CiScopes` with only the named components on the `P` side and the base side
/// left at the whole-file meaning.
fn pr_scope(components: &[&str]) -> CiScopes {
    CiScopes {
        base: CiScope::Unscoped,
        pr: CiScope::Scoped(components.iter().map(|c| (*c).to_string()).collect()),
    }
}

#[test]
fn a_pr_editing_an_unrelated_ci_yml_job_is_not_a_global_input_change() {
    // The #9065 churn on the PRs that this very issue produces: `ci.yml` is in
    // EVERY component's `G`, and `Conflict Marker Check` scans `**`, so a PR
    // that so much as reformats the `backend-tests` block was stale against any
    // base move at all — permanently, since `main` moves every ~11 min.
    let d = set(&["loom-daemon/src/unrelated.rs"]);
    let p = set(&[CI_WORKFLOW]);
    let s = spec("Conflict Marker Check");

    // Unscoped — the pre-narrowing meaning — refuses.
    assert!(stale_reason(s, &d, &p).is_some());
    // Attributed to a job no required context runs, it does not.
    assert!(stale_reason_scoped(s, &d, &p, &pr_scope(&[])).is_none());
}

#[test]
fn a_pr_editing_this_gates_own_ci_yml_block_is_still_stale() {
    // The half that must NOT relax. The PR changed the rule this gate runs
    // under, and `main` brought new subjects for that rule to judge — exactly
    // clause 2, and the merged tree has never been judged that way.
    let d = set(&["loom-daemon/src/unrelated.rs"]);
    let p = set(&[CI_WORKFLOW]);
    let s = spec("Conflict Marker Check");
    assert!(stale_reason_scoped(s, &d, &p, &pr_scope(&["Conflict Marker Check"])).is_some());
    // …and a scope naming some OTHER gate does not accidentally cover this one.
    assert!(stale_reason_scoped(s, &d, &p, &pr_scope(&["File Size Ratchet"])).is_none());
}

#[test]
fn the_pr_side_narrowing_does_not_rescue_a_real_global_input_change() {
    // The narrowing removes ONE path from the `P` match. A PR that edits
    // `ci.yml` in an unrelated block AND tightens the gate's baseline is still
    // stale on the baseline.
    let d = set(&["loom-daemon/src/big.rs"]);
    let p = set(&[CI_WORKFLOW, "scripts/file-size-baseline.txt"]);
    let reason = stale_reason_scoped(spec("File Size Ratchet"), &d, &p, &pr_scope(&[]))
        .expect("the baseline edit is a global-input change regardless of ci.yml");
    assert_eq!(reason.pr_path.as_deref(), Some("scripts/file-size-baseline.txt"));
}

#[test]
fn the_two_sides_narrow_independently() {
    // `D` and `P` are attributed against different trees, so one side's answer
    // must never stand in for the other's.
    let s = spec("Conflict Marker Check");
    let d = set(&[CI_WORKFLOW]);
    let p = set(&["loom-daemon/src/lib.rs"]);
    let none = CiScope::Scoped(BTreeSet::new());

    // Base narrowed, `P` untouched by ci.yml: clause 1 no longer fires.
    assert!(stale_reason_scoped(
        s,
        &d,
        &p,
        &CiScopes {
            base: none.clone(),
            pr: CiScope::Unscoped
        }
    )
    .is_none());
    // The SAME base move with the base side unscoped still refuses — a narrow
    // `P` scope does not cover for it.
    assert!(stale_reason_scoped(
        s,
        &d,
        &p,
        &CiScopes {
            base: CiScope::Unscoped,
            pr: none
        }
    )
    .is_some());
}

#[test]
fn the_pr_side_narrowing_is_confined_to_clauses_1_and_2() {
    // Clauses 3-5 match `P` against `scanned`/`coupled`/`removed` and are left
    // un-narrowed on purpose: over-refusing is the safe direction, so a scope
    // that excludes every component must not turn those clauses off.
    let none = pr_scope(&[]);

    // Clause 3: both sides touch ci.yml, which `Conflict Marker Check` scans
    // via `**`. Only the BASE scope may silence it.
    let both = set(&[CI_WORKFLOW]);
    assert!(stale_reason_scoped(spec("Conflict Marker Check"), &both, &both, &none).is_some());

    // Clause 5: the PR deletes a path while the base move touches the link
    // graph. `P`'s removal set is never filtered.
    let d = set(&["docs/some-page.md"]);
    let p = set_with_removal(&[], &["scripts/gone.sh"]);
    assert!(stale_reason_scoped(spec("Dangling Link Check"), &d, &p, &none).is_some());
}

// --- Role Prompt Prefix Ratchet: dedicated read surface (#9748) ---------------
//
// `scripts/check-role-prompt-budget.sh` reads exactly: the two shared prefixes,
// `defaults/roles/*.json` (role discovery), and the command-directory markdown
// (entry points + transitively linked bare siblings). It never reads
// `defaults/docs`, installed `.loom/docs`, or the installed mirrors.

const ROLE_PREFIX: &str = "Role Prompt Prefix Ratchet";

/// Both orientations of an ordinary-modification pair: no `StaleReason`.
fn assert_role_prefix_fresh_both_ways(a: &[&str], b: &[&str]) {
    let s = spec(ROLE_PREFIX);
    assert_eq!(stale_reason(s, &set(a), &set(b)), None, "base={a:?} pr={b:?}");
    assert_eq!(stale_reason(s, &set(b), &set(a)), None, "base={b:?} pr={a:?}");
}

/// Both orientations of an interacting pair: a `StaleReason` each way.
fn assert_role_prefix_stale_both_ways(a: &[&str], b: &[&str]) {
    let s = spec(ROLE_PREFIX);
    assert!(stale_reason(s, &set(a), &set(b)).is_some(), "base={a:?} pr={b:?}");
    assert!(stale_reason(s, &set(b), &set(a)).is_some(), "base={b:?} pr={a:?}");
}

const JUDGE_CMD: &str = "defaults/.claude/commands/loom/judge.md";
const BUILDER_CMD: &str = "defaults/.claude/commands/loom/builder.md";

#[test]
fn role_prefix_ignores_documentation_edits_on_both_sides() {
    // The measured #10423/#10497 tuple.
    assert_role_prefix_fresh_both_ways(
        &["defaults/docs/daemon-reference.md"],
        &["defaults/docs/eta.md"],
    );
    // Same doc on both sides is still not this check's input.
    assert_role_prefix_fresh_both_ways(&["defaults/docs/eta.md"], &["defaults/docs/eta.md"]);
}

#[test]
fn role_prefix_ignores_eta_docs_against_the_judge_prompt() {
    // PR #10486's recorded refusal: base ETA docs vs the PR's Judge prompt.
    assert_role_prefix_fresh_both_ways(&["defaults/docs/eta.md"], &[JUDGE_CMD]);
}

#[test]
fn role_prefix_ignores_docs_against_the_shared_claude_md() {
    assert_role_prefix_fresh_both_ways(&["defaults/docs/eta.md"], &["CLAUDE.md"]);
    assert_role_prefix_fresh_both_ways(&["defaults/docs/eta.md"], &["defaults/.loom/CLAUDE.md"]);
}

#[test]
fn role_prefix_ignores_installed_mirrors_and_unread_prompt_files() {
    // Installed `.loom/docs`, `.loom/roles`, `.claude/commands` mirrors, the
    // role markdown, AGENTS.md and the installed CLAUDE.md are not read.
    for unread in [
        ".loom/docs/eta.md",
        ".loom/docs/daemon-reference.md",
        ".loom/roles/judge.md",
        ".claude/commands/loom/judge.md",
        ".loom/CLAUDE.md",
        "AGENTS.md",
        "defaults/.loom/AGENTS.md",
        "defaults/roles/judge.md",
        "defaults/roles/README.md",
        "defaults/docs/ci-principles.md",
    ] {
        assert_role_prefix_fresh_both_ways(&[unread], &[JUDGE_CMD]);
        assert_role_prefix_fresh_both_ways(&[unread], &["CLAUDE.md"]);
        assert_role_prefix_fresh_both_ways(&[".loom/docs/eta.md"], &[unread]);
    }
}

#[test]
fn role_prefix_still_couples_every_declared_read() {
    // Positive controls: two interacting reads on opposite sides stay stale.
    assert_role_prefix_stale_both_ways(&[JUDGE_CMD], &[BUILDER_CMD]);
    assert_role_prefix_stale_both_ways(&["CLAUDE.md"], &[JUDGE_CMD]);
    assert_role_prefix_stale_both_ways(&["CLAUDE.md"], &["defaults/.loom/CLAUDE.md"]);
    // A transitively reached sibling (not a role entry point) is a command-dir
    // markdown file and is covered.
    assert_role_prefix_stale_both_ways(
        &["defaults/.claude/commands/loom/probe-protocol.md"],
        &["CLAUDE.md"],
    );
    // Role discovery (`defaults/roles/*.json`) against a command input.
    assert_role_prefix_stale_both_ways(&["defaults/roles/judge.json"], &[JUDGE_CMD]);
    assert_role_prefix_stale_both_ways(&["defaults/roles/newrole.json"], &["CLAUDE.md"]);
}

#[test]
fn role_prefix_keeps_its_global_inputs() {
    let s = spec(ROLE_PREFIX);
    for global in [
        "scripts/check-role-prompt-budget.sh",
        "scripts/role-prompt-budget.txt",
        CI_WORKFLOW,
    ] {
        assert!(s.global.contains(&global), "{global} must stay global");
        // Budget/checker/workflow change against a measured input, both ways.
        assert_role_prefix_stale_both_ways(&[global], &[JUDGE_CMD]);
        assert_role_prefix_stale_both_ways(&[global], &["CLAUDE.md"]);
        assert_role_prefix_stale_both_ways(&[global], &["defaults/roles/judge.json"]);
    }
    // A global move against a PR that touches only docs has nothing to re-judge.
    let d = set(&["scripts/role-prompt-budget.txt"]);
    assert_eq!(stale_reason(s, &d, &set(&["defaults/docs/eta.md"])), None);
}

#[test]
fn role_prefix_keeps_conservative_removal_handling() {
    let s = spec(ROLE_PREFIX);
    assert!(s.removal_sensitive);
    // Deleting/renaming a relevant input while the other side touches a read.
    let d = set_with_removal(&[], &[JUDGE_CMD]);
    assert!(stale_reason(s, &d, &set(&["CLAUDE.md"])).is_some());
    assert!(stale_reason(s, &set(&["CLAUDE.md"]), &d).is_some());
    let d = set_with_removal(&[], &["defaults/roles/judge.json"]);
    assert!(stale_reason(s, &d, &set(&[BUILDER_CMD])).is_some());
    assert!(stale_reason(s, &set(&[BUILDER_CMD]), &d).is_some());
    // The removal clause does not couple to unread docs on the other side.
    let d = set_with_removal(&[], &[JUDGE_CMD]);
    assert_eq!(stale_reason(s, &d, &set(&["defaults/docs/eta.md"])), None);
}

#[test]
fn only_role_prefix_got_a_dedicated_surface() {
    // Neighbouring prompt ratchets keep the broad shared set.
    let md = spec("Markdown Token Ratchet");
    assert_eq!(md.scanned, PROMPT_SURFACE);
    assert!(md.scanned.contains(&"defaults/docs/**"));
    assert!(!spec(ROLE_PREFIX).coupled.contains(&"defaults/docs/**"));
    assert!(PROMPT_SURFACE.contains(&"defaults/docs/**"));
    assert!(PROMPT_SURFACE.contains(&".loom/docs/**"));
}

/// Source contract (#9748): the declared coupled set must cover everything the
/// REAL checker's `--files` resolution reads, and nothing it names may fall
/// outside the declared inputs. A fixture that grows the checker's read
/// surface (a new shared prefix, a second command dir) fails here.
#[test]
fn role_prefix_inputs_cover_the_real_checkers_read_surface() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = std::process::Command::new("bash")
        .arg(root.join("scripts/check-role-prompt-budget.sh"))
        .arg("--files")
        .current_dir(&root)
        .output()
        .expect("the role-prompt checker must be runnable");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let s = spec(ROLE_PREFIX);
    let mut read: BTreeSet<String> = BTreeSet::new();
    for line in stdout.lines() {
        // Per-file rows are "<tokens>  <path>"; role headers start with "==".
        let mut cols = line.split_whitespace();
        if let (Some(n), Some(path), None) = (cols.next(), cols.next(), cols.next()) {
            if n.chars().all(|c| c.is_ascii_digit()) {
                read.insert(path.to_string());
            }
        }
    }
    assert!(
        read.contains("CLAUDE.md") && read.contains("defaults/.loom/CLAUDE.md"),
        "{read:?}"
    );
    assert!(
        read.iter().any(|p| p.ends_with("/probe-protocol.md")),
        "the fixture must include a transitively reached sibling: {read:?}"
    );
    for path in &read {
        assert!(
            s.coupled.iter().any(|pat| glob_match(pat, path)),
            "the checker reads `{path}` but the Role Prompt Prefix spec does not couple it"
        );
    }
    // Role discovery: every tracked role JSON is declared, and the checker
    // reads no documentation tree.
    assert!(s
        .coupled
        .iter()
        .any(|pat| glob_match(pat, "defaults/roles/judge.json")));
    assert!(read.iter().all(|p| !p.contains("/docs/")), "{read:?}");
    // And the declared command-dir surface is not wider than one directory
    // level of markdown (the checker only follows bare sibling links). Role
    // discovery is the one recursive entry: see the nested-JSON test below.
    for pat in s.coupled {
        assert!(
            !pat.contains("**") || *pat == ROLE_DISCOVERY_GLOB,
            "unexpectedly broad coupled glob `{pat}`"
        );
    }
}

/// The checker's role-discovery pathspec, as the spec must declare it.
const ROLE_DISCOVERY_GLOB: &str = "defaults/roles/**/*.json";

/// The real checker, verbatim — the authority every assertion below is
/// grounded in, not a copy of the declared list.
const ROLE_PREFIX_CHECKER: &str =
    include_str!("../../../../../scripts/check-role-prompt-budget.sh");

/// Role discovery is `git ls-files -- 'defaults/roles/*.json'`. A git
/// pathspec without `:(glob)` magic matches `*` ACROSS `/`, so a JSON file in a
/// subdirectory of `defaults/roles/` is a discovered role too (its basename is
/// the role name). A one-segment `defaults/roles/*.json` glob silently misses
/// that read — the fail-open narrowing ci-principles rule 9 forbids. This runs
/// the REAL checker on a hermetic fixture to prove the discovery, then asserts
/// the spec couples it.
#[test]
fn role_prefix_couples_a_nested_role_json_the_checker_discovers() {
    // Grounding: the discovery line is still the bare (non-:(glob)) pathspec.
    assert!(
        ROLE_PREFIX_CHECKER.contains("git ls-files -- 'defaults/roles/*.json'"),
        "role discovery changed shape — re-derive the Role Prompt Prefix inputs from it"
    );

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let tmp = tempfile::tempdir().expect("tempdir");
    let t = tmp.path();
    let cmd_dir = t.join("defaults/.claude/commands/loom");
    for dir in [
        t.join("scripts"),
        cmd_dir.clone(),
        t.join("defaults/roles/extra"),
        t.join("defaults/.loom"),
    ] {
        std::fs::create_dir_all(dir).expect("mkdir");
    }
    std::fs::copy(
        root.join("scripts/check-role-prompt-budget.sh"),
        t.join("scripts/check-role-prompt-budget.sh"),
    )
    .expect("copy checker");
    std::fs::write(t.join("CLAUDE.md"), "x").unwrap();
    std::fs::write(t.join("defaults/.loom/CLAUDE.md"), "x").unwrap();
    std::fs::write(cmd_dir.join("nested.md"), "nested role prompt\n").unwrap();
    std::fs::write(t.join("defaults/roles/extra/nested.json"), "{}\n").unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(t)
            .output()
            .expect("git must be runnable");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    let out = std::process::Command::new("bash")
        .arg(t.join("scripts/check-role-prompt-budget.sh"))
        .arg("--files")
        .current_dir(t)
        .output()
        .expect("the role-prompt checker must be runnable");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l.trim() == "== nested"),
        "precondition: the real checker discovers a role from a nested JSON:\n{stdout}"
    );

    let s = spec(ROLE_PREFIX);
    assert!(s.coupled.contains(&ROLE_DISCOVERY_GLOB), "{:?}", s.coupled);
    let nested = "defaults/roles/extra/nested.json";
    assert!(
        s.coupled.iter().any(|pat| glob_match(pat, nested)),
        "the checker discovers `{nested}` but the spec does not couple it"
    );
    // Behaviour, not just the list: a new nested role on one side against a
    // shared-prefix or command edit on the other is stale both ways.
    assert_role_prefix_stale_both_ways(&[nested], &["CLAUDE.md"]);
    assert_role_prefix_stale_both_ways(&[nested], &["defaults/.claude/commands/loom/nested.md"]);
    // And the recursion admits only JSON: role markdown symlinks stay unread.
    assert_role_prefix_fresh_both_ways(&["defaults/roles/extra/notes.md"], &["CLAUDE.md"]);
}

/// Every repo path the checker names outside its throwaway fixtures, with the
/// `$ROOT/` prefix stripped. Lines that only run inside the self-test's
/// sandbox (`cd "$tmp"`, `git -C "$tmp"`) and `echo` lines (operator-facing
/// prose and the budget file's generated header) are not reads of this repo.
fn checker_repo_paths(src: &str) -> BTreeSet<String> {
    const ROOTS: &[&str] = &[
        "scripts/",
        "defaults/",
        "docs/",
        ".loom/",
        ".claude/",
        ".github/",
        "CLAUDE.md",
        "AGENTS.md",
    ];
    let mut out = BTreeSet::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#')
            || trimmed.starts_with("echo ")
            || line.contains("cd \"$tmp\"")
            || line.contains("git -C \"$tmp\"")
        {
            continue;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let Some(root) = ROOTS.iter().find(|r| bytes[i..].starts_with(r.as_bytes())) else {
                i += 1;
                continue;
            };
            // Byte-wise: the checker's prose carries multi-byte characters.
            let before = &bytes[..i];
            let anchored_at_root = before.ends_with(b"$ROOT/") || before.ends_with(b"\"$ROOT\"/");
            let free_standing = i == 0 || !(is_path_byte(bytes[i - 1]) || bytes[i - 1] == b'$');
            if !(anchored_at_root || free_standing) {
                i += root.len();
                continue;
            }
            let mut end = i + root.len();
            while end < bytes.len() && (is_path_byte(bytes[end]) || bytes[end] == b'*') {
                end += 1;
            }
            out.insert(line[i..end].trim_end_matches('.').to_string());
            i = end;
        }
    }
    out
}

/// Source contract (#9748): every repo path the checker names — in its check
/// path AND in the `--self-test` CI runs first (`ci.yml`'s "Self-test the
/// ratchet" step) — is a declared input of the spec. The self-test executes
/// `scripts/check-markdown-token-budget.sh --list` on the REAL tree and fails
/// the step when the two estimators disagree, so that script is a global input
/// even though the main check never runs it. A new path literal in the checker
/// fails here until the spec grows to cover it.
#[test]
fn role_prefix_inputs_cover_every_repo_path_the_checker_names() {
    let s = spec(ROLE_PREFIX);
    let named = checker_repo_paths(ROLE_PREFIX_CHECKER);
    for must in [
        "CLAUDE.md",
        "defaults/.loom/CLAUDE.md",
        "defaults/.claude/commands/loom",
        "defaults/roles/*.json",
        "scripts/role-prompt-budget.txt",
        "scripts/check-markdown-token-budget.sh",
    ] {
        assert!(named.contains(must), "the scan lost `{must}` — the parser broke: {named:?}");
    }
    // A bare directory (`CMD_DIR`) is covered when a markdown entry point in it is.
    let covered = |path: &str| {
        let is_dir = !path.rsplit('/').next().unwrap_or(path).contains('.');
        s.global.iter().chain(s.coupled.iter()).any(|pat| {
            glob_match(pat, path) || (is_dir && glob_match(pat, &format!("{path}/entry.md")))
        })
    };
    for path in &named {
        assert!(
            covered(path),
            "scripts/check-role-prompt-budget.sh names `{path}` but the Role Prompt Prefix spec \
             declares no input covering it (inputs.rs)"
        );
    }
    // The self-test's estimator peer is a GLOBAL input: a change to it can flip
    // the parity assertion against any measured file.
    assert!(s.global.contains(&"scripts/check-markdown-token-budget.sh"));
    assert_role_prefix_stale_both_ways(
        &["scripts/check-markdown-token-budget.sh"],
        &["defaults/.claude/commands/loom/probe-protocol.md"],
    );
    // …and it does not couple unread documentation.
    assert_eq!(
        stale_reason(
            s,
            &set(&["scripts/check-markdown-token-budget.sh"]),
            &set(&["defaults/docs/eta.md"])
        ),
        None
    );
}
