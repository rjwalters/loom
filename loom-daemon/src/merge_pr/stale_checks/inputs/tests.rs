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

fn spec(ctx: &str) -> &'static CheckSpec {
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
    let d = set(&["defaults/roles/builder.md"]);
    let p = set(&["defaults/roles/judge.md"]);
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
        assert_eq!(
            stale_reason(spec(ctx), &FileSet::default(), &p),
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

/// One `ci.yml` job: its key, its expanded display name(s), and its own lines.
struct Job {
    names: Vec<String>,
    lines: Vec<String>,
}

/// A deliberately small `ci.yml` reader: job keys at indent 2 under `jobs:`,
/// the `name:` at indent 4, and `${{ matrix.os }}` expanded from the job's own
/// `os:` list. Enough to pin the table, with nothing to go wrong in a YAML
/// dependency.
fn parse_jobs(yaml: &str) -> Vec<Job> {
    let mut jobs: Vec<Job> = Vec::new();
    let mut in_jobs = false;
    let mut current: Option<Vec<String>> = None;
    for line in yaml.lines() {
        if !in_jobs {
            in_jobs = line == "jobs:";
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            break; // left the `jobs:` mapping
        }
        let is_key = line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim_start().starts_with('#');
        if is_key {
            if let Some(lines) = current.take() {
                jobs.push(finish(lines));
            }
            current = Some(Vec::new());
        } else if let Some(lines) = current.as_mut() {
            lines.push(line.to_string());
        }
    }
    if let Some(lines) = current.take() {
        jobs.push(finish(lines));
    }
    jobs
}

fn finish(lines: Vec<String>) -> Job {
    let raw = lines
        .iter()
        .find_map(|l| l.strip_prefix("    name: "))
        .unwrap_or("")
        .trim()
        .to_string();
    let names = if raw.contains("${{ matrix.os }}") {
        matrix_os(&lines)
            .into_iter()
            .map(|os| raw.replace("${{ matrix.os }}", &os))
            .collect()
    } else {
        vec![raw]
    };
    Job { names, lines }
}

/// The job's `os:` matrix values, from either the flow (`os: [a, b]`) or block
/// (`os:` then `- a`) form.
fn matrix_os(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_os = false;
    for line in lines {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("os:") {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                return inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            in_os = rest.is_empty();
            continue;
        }
        if in_os {
            match t.strip_prefix("- ") {
                Some(v) => out.push(v.trim().trim_matches('"').to_string()),
                None => in_os = false,
            }
        }
    }
    out
}

/// Every `scripts/…​.sh` / `defaults/scripts/…​.sh` a job's non-comment lines
/// name. Paths under another prefix (`.loom/scripts/…`) are the installed
/// copies, not this repo's sources, and are deliberately not collected.
fn script_refs(lines: &[String]) -> BTreeSet<String> {
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
    assert_eq!(
        REQUIRED_CONTEXTS.len(),
        19,
        "main requires 19 contexts (verified 2026-09-26); update the list AND the specs together"
    );
    for ctx in REQUIRED_CONTEXTS {
        assert!(spec_for(ctx).is_some(), "no CheckSpec for required context {ctx:?}");
    }
    for s in SPECS {
        assert!(
            REQUIRED_CONTEXTS.contains(&s.context),
            "{:?} has a spec but is not a required context — remove it or add it to \
REQUIRED_CONTEXTS",
            s.context
        );
    }
    let unique: BTreeSet<&str> = SPECS.iter().map(|s| s.context).collect();
    assert_eq!(unique.len(), SPECS.len(), "duplicate context in SPECS");
    assert_eq!(unique.len(), REQUIRED_CONTEXTS.len());
}

#[test]
fn every_required_context_names_a_real_ci_yml_job() {
    let jobs = parse_jobs(CI_YML);
    let names: BTreeSet<&str> = jobs
        .iter()
        .flat_map(|j| j.names.iter().map(String::as_str))
        .collect();
    assert!(names.len() > 20, "the ci.yml parser found only {names:?}");
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
    // check whose new input it cannot see.
    let jobs = parse_jobs(CI_YML);
    let mut checked = 0;
    for job in &jobs {
        for name in &job.names {
            let Some(spec) = spec_for(name) else { continue };
            checked += 1;
            let refs = script_refs(&job.lines);
            assert!(
                !refs.is_empty() || name == "Shell Budget Ratchet",
                "{name}: no script references found — the parser probably broke"
            );
            for script in refs {
                assert!(
                    spec.global.contains(&script.as_str()),
                    "{name} runs {script} but it is not in that spec's G set (inputs.rs). A step \
added to a required job must be reflected in the table, or the freshness guard will not notice \
when `main` changes that script."
                );
            }
        }
    }
    assert_eq!(checked, 19, "expected all 19 required contexts to be pinned");
}

#[test]
fn every_spec_lists_the_ci_workflow_as_a_global_input() {
    // The job definition itself is an input to every check it defines.
    for s in SPECS {
        assert!(s.global.contains(&CI_WORKFLOW), "{}: ci.yml must be a global input", s.context);
    }
}
