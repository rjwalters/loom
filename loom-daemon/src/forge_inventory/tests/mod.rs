//! Tests for the forge operation inventory (Issue #9777, epic #9769 phase 1).
//!
//! Two kinds, and the distinction matters:
//!
//! 1. **Gates over the real tree** — [`the_embedded_manifest_is_publishable`]
//!    and [`the_repository_has_no_unclassified_direct_forge_call`] run the
//!    shipped validator and the shipped change gate against *this* checkout.
//!    They are the mechanism #9777 AC2 asks for: adding a required operation
//!    without a test, or a new unclassified direct forge call, fails here on the
//!    PR that introduces it. Neither restates an implementation — each one reads
//!    the repository and can fail on a change that never touches this file.
//! 2. **Rule tests over synthetic manifests** — each validator rule is driven
//!    from a deliberately broken manifest written to a tempdir and loaded
//!    through [`super::load_from_dir`], the same parse the shipped one uses.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::gate::{self, Baseline, Bypass, Verdict};
use super::model::{is_stale_version, version_key, Coverage, Profile, Support};
use super::validate::{self, Rule};
use super::{load_embedded, load_embedded_baseline, load_from_dir, probe, report, Inventory};

/// The repository root: `loom-daemon/`'s parent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ has a parent")
        .to_path_buf()
}

fn rules(findings: &[validate::Finding]) -> BTreeSet<Rule> {
    findings.iter().map(|f| f.rule).collect()
}

// ============================================================================
// Gates over the real tree
// ============================================================================

#[test]
fn the_embedded_manifest_parses_and_covers_every_group() {
    let inv = load_embedded().expect("the embedded manifest must parse");
    assert!(
        inv.operations.len() >= 40,
        "only {} operation(s) — the inventory lost rows",
        inv.operations.len()
    );
    let rep = report::build(&inv);
    assert!(
        rep.uncovered_groups.is_empty(),
        "#9769 inventory group(s) with no mapped operation: {:?}",
        rep.uncovered_groups
    );
    assert!(
        rep.unseeded_high_risk_cases.is_empty(),
        "high-risk case(s) with no seeded operation: {:?}",
        rep.unseeded_high_risk_cases
    );
}

/// The coverage validator, run against the shipped manifest and this checkout.
///
/// This is the gate: a new required operation with no `test_id`, a duplicate ID,
/// a stale `version_support` pin, an optional exclusion with no stated reason, a
/// quietly waived non-waivable requirement, or a `test_status = "implemented"`
/// row whose `test_path` no longer holds the named test all fail HERE.
#[test]
fn the_embedded_manifest_is_publishable() {
    let inv = load_embedded().unwrap();
    let mut findings = validate::validate(&inv);
    findings.extend(validate::validate_test_evidence(&inv, &repo_root()));
    assert!(
        findings.is_empty(),
        "the shipped forge operation inventory does not validate:\n{}",
        findings
            .iter()
            .map(|f| format!("  {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The change gate, run against every tracked file in this checkout.
///
/// A file that makes a direct `gh` forge call must be a declared caller of some
/// inventoried operation, or be listed in the bypass baseline. A new one is an
/// `Unclassified` failure; one that grows beyond its baselined count is a
/// `BaselineGrew` failure. Both fail here, on the PR that introduces them.
#[test]
fn the_repository_has_no_unclassified_direct_forge_call() {
    let root = repo_root();
    if !root.join(".git").exists() {
        // A source tarball has no git index to enumerate, and this gate measures
        // tracked files only. Say so rather than passing silently on an
        // unmeasured tree.
        eprintln!("skipping: {} is not a git checkout", root.display());
        return;
    }
    let Some(files) = tracked_files(&root) else {
        eprintln!("skipping: `git ls-files` unavailable");
        return;
    };
    assert!(files.len() > 100, "`git ls-files` returned only {} path(s)", files.len());

    let inv = load_embedded().unwrap();
    let baseline = load_embedded_baseline().expect("the embedded baseline must parse");
    let results = gate::evaluate(&root, &files, &inv.declared_caller_paths(), &baseline);

    let problems = gate::validate_baseline(&baseline, &inv.header.owners);
    assert!(
        problems.is_empty(),
        "the bypass baseline is not well-formed (every entry needs an owner from \
         manifest.toml's [owners] and a removal issue):\n  {}",
        problems.join("\n  ")
    );

    let failures: Vec<String> = results
        .iter()
        .filter(|r| r.is_failure())
        .map(|r| match r.verdict {
            Verdict::Unclassified => format!("UNCLASSIFIED {} ({} call site(s))", r.path, r.calls),
            Verdict::BaselineGrew { recorded } => {
                format!("GREW {} ({recorded} -> {})", r.path, r.calls)
            }
            _ => format!("{} (unexpected verdict)", r.path),
        })
        .collect();
    assert!(
        failures.is_empty(),
        "a direct forge call must be CLASSIFIED (declare the file as a caller in \
         defaults/forge/operations/*.toml) or BASELINED \
         (`loom-daemon forge-inventory gate --update --removal-issue <N>`):\n  {}",
        failures.join("\n  ")
    );
    // The ledger is a ratchet, so it has to actually be measuring something.
    assert!(
        !results.is_empty(),
        "the gate found no direct forge call anywhere — the scanner is broken, not the tree"
    );
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

/// Every baseline entry names a file that still exists, so the ledger cannot
/// accumulate entries for paths that were deleted or renamed away.
#[test]
fn every_baseline_entry_names_an_existing_file() {
    let root = repo_root();
    let baseline = load_embedded_baseline().unwrap();
    let missing: Vec<&str> = baseline
        .bypasses
        .iter()
        .map(|b| b.path.as_str())
        .filter(|p| !root.join(p).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "baseline entries for paths that no longer exist: {missing:?} \
         (regenerate with `forge-inventory gate --update`)"
    );
}

/// The qualification gate asks a *different* question from well-formedness, and
/// on phase 1's tree its honest answer is "not yet". Asserting that it reports
/// unresolved rows is what keeps the two from being quietly merged into one gate
/// later: wiring `validate_qualification` into CI would force the manifest to
/// start asserting evidence no probe has produced.
#[test]
fn the_qualification_gate_still_reports_unresolved_evidence() {
    let inv = load_embedded().unwrap();
    assert!(
        validate::validate(&inv).is_empty(),
        "well-formedness and qualification must be separable"
    );
    let q = validate::validate_qualification(&inv);
    assert!(
        !q.is_empty(),
        "no hosted probe has run yet, so the qualification gate cannot be clean"
    );
    let fired = rules(&q);
    assert!(fired.contains(&Rule::UnresolvedRequiredSupport), "{fired:?}");
    assert!(fired.contains(&Rule::UnprovenRequiredTest), "{fired:?}");
    assert!(fired.contains(&Rule::UnintegratedRequiredCaller), "{fired:?}");
}

/// Every declared caller path exists in the tree. A `callers` entry pointing at
/// a moved file is how an inventory silently stops classifying what it names —
/// and, because the change gate treats declared paths as classified, how a
/// migrated file silently falls back to being an unclassified bypass.
#[test]
fn every_declared_caller_path_exists() {
    let root = repo_root();
    let inv = load_embedded().unwrap();
    let missing: Vec<String> = inv
        .operations
        .iter()
        .flat_map(|o| {
            let id = o.id.clone();
            o.callers.iter().map(move |c| (id.clone(), c.path.clone()))
        })
        .filter(|(_, path)| !root.join(path).exists())
        .map(|(id, path)| format!("{id}: {path}"))
        .collect();
    assert!(
        missing.is_empty(),
        "declared caller path(s) that do not exist:\n  {}",
        missing.join("\n  ")
    );
}

// ============================================================================
// Synthetic manifests: one per rule
// ============================================================================

/// A minimal manifest header naming the providers and owners the fixtures use.
const FIXTURE_HEADER: &str = r#"
schema_version = 1
inventory_source = "fixture"
base_sha = "0000000000000000000000000000000000000000"
[providers.github]
floor = "0"
[providers.gitea]
floor = "1.24"
[owners]
daemon-forge = "fixture owner"
"#;

/// One well-formed required row the fixtures mutate. `extra` is appended so each
/// test can add exactly the field it is exercising.
fn fixture_row(id: &str, group: &str, extra: &str) -> String {
    // Operation IDs are dotted; test IDs are `::`-separated and forbid `.`.
    let test = id.replace('.', "-");
    format!(
        r#"
[[operation]]
id = "{id}"
group = "{group}"
summary = "fixture"
profile = "required-coordination"
disposition = "required"
class = "read"
actor = "daemon"
pagination = "not-paginated"
consistency = "eventual"
idempotency = "idempotent"
retry = "backoff-safe"
test_id = "fixture::{test}"
owner = "daemon-forge"
platform_support = "documented"
{extra}
"#
    )
}

/// Write `<dir>/manifest.toml` + `<dir>/operations/ops.toml` and load it through
/// the same parser the shipped manifest uses.
fn load_fixture(rows: &str) -> (tempfile::TempDir, Inventory) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("manifest.toml"), FIXTURE_HEADER).unwrap();
    std::fs::create_dir_all(dir.path().join("operations")).unwrap();
    std::fs::write(dir.path().join("operations/ops.toml"), rows).unwrap();
    let inv = load_from_dir(dir.path()).expect("fixture must parse");
    (dir, inv)
}

/// One filler row per #9769 inventory group, so a fixture testing one rule does
/// not also trip `uncovered-group` for the other twenty.
fn all_groups_rows() -> String {
    super::model::INVENTORY_GROUPS
        .iter()
        .enumerate()
        .map(|(i, g)| {
            fixture_row(&format!("filler.op-{i}"), g, "risk = \"normal\"\nhigh_risk_cases = []")
        })
        .collect()
}

/// …plus one high-risk row per seeded case, so `unseeded-high-risk-case` is also
/// satisfied. The result validates clean, and every single-rule test below
/// differs from it by exactly one mutation.
fn clean_fixture_rows() -> String {
    let mut rows = all_groups_rows();
    for (i, case) in super::model::SEEDED_HIGH_RISK_CASES.iter().enumerate() {
        rows.push_str(&fixture_row(
            &format!("seed.case-{i}"),
            "quota-caching-errors",
            &format!("risk = \"high\"\nhigh_risk_cases = [\"{case}\"]"),
        ));
    }
    rows
}

#[test]
fn a_clean_fixture_validates_clean() {
    // The control: a finding reported by any test below is attributable to that
    // test's single mutation and nothing else.
    let (_d, inv) = load_fixture(&clean_fixture_rows());
    assert_eq!(validate::validate(&inv), vec![]);
}

#[test]
fn duplicate_operation_and_test_ids_are_rejected() {
    let mut rows = clean_fixture_rows();
    // Same id AND same test_id as filler.op-0.
    rows.push_str(&fixture_row(
        "filler.op-0",
        super::model::INVENTORY_GROUPS[0],
        "risk = \"normal\"",
    ));
    let (_d, inv) = load_fixture(&rows);
    let fired = rules(&validate::validate(&inv));
    assert!(fired.contains(&Rule::DuplicateId), "{fired:?}");
    assert!(fired.contains(&Rule::DuplicateTestId), "{fired:?}");
}

#[test]
fn a_malformed_operation_or_test_id_is_rejected() {
    let mut rows = clean_fixture_rows();
    rows.push_str(
        &fixture_row("Issue_Create", super::model::INVENTORY_GROUPS[0], "")
            .replace("test_id = \"fixture::Issue_Create\"", "test_id = \"solo\""),
    );
    let (_d, inv) = load_fixture(&rows);
    let fired = rules(&validate::validate(&inv));
    assert!(fired.contains(&Rule::MalformedId), "{fired:?}");
    assert!(fired.contains(&Rule::MalformedTestId), "{fired:?}");
}

#[test]
fn a_required_operation_without_a_test_is_rejected() {
    // AC2: "adding a ... required operation without a test causes a meaningful
    // validation failure".
    let rows = clean_fixture_rows().replace("test_id = \"fixture::filler-op-0\"\n", "");
    let (_d, inv) = load_fixture(&rows);
    let findings = validate::validate(&inv);
    assert!(rules(&findings).contains(&Rule::MissingTest), "{findings:?}");
    assert!(findings.iter().any(|f| f.subject == "filler.op-0"));
}

#[test]
fn an_uncovered_inventory_group_is_rejected() {
    let rows = clean_fixture_rows().replace(
        &format!("group = \"{}\"", super::model::INVENTORY_GROUPS[0]),
        "group = \"quota-caching-errors\"",
    );
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::UncoveredGroup));
}

#[test]
fn a_group_outside_the_9769_enumeration_is_rejected() {
    let mut rows = clean_fixture_rows();
    rows.push_str(&fixture_row("extra.op", "something-invented", ""));
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::UnknownGroup));
}

#[test]
fn an_unacknowledged_unknown_is_rejected_but_an_acknowledged_one_is_not() {
    let base = clean_fixture_rows();
    // Unfilled: platform support unknown with nothing said about it.
    let bad = base.replace(
        "platform_support = \"documented\"\nrisk = \"normal\"\nhigh_risk_cases = []\n",
        "platform_support = \"unknown\"\nrisk = \"normal\"\nhigh_risk_cases = []\n",
    );
    let (_d, inv) = load_fixture(&bad);
    assert!(
        rules(&validate::validate(&inv)).contains(&Rule::UnacknowledgedUnknownSupport),
        "an unknown with no `unknowns` entry is an unfilled field, not an open question"
    );
    // Acknowledged: the same unknown with the open question recorded. This is
    // phase 1's correct answer and must NOT fail well-formedness…
    let good = bad.replace(
        "platform_support = \"unknown\"\n",
        "platform_support = \"unknown\"\nunknowns = [\"a hosted probe has not run\"]\n",
    );
    let (_d2, inv2) = load_fixture(&good);
    assert_eq!(validate::validate(&inv2), vec![]);
    // …while the qualification gate still refuses to count it as a pass.
    assert!(
        rules(&validate::validate_qualification(&inv2)).contains(&Rule::UnresolvedRequiredSupport)
    );
}

#[test]
fn a_version_pin_below_the_provider_floor_is_stale() {
    let rows = clean_fixture_rows().replacen(
        "owner = \"daemon-forge\"\nplatform_support",
        "version_support = { gitea = \"1.20\" }\nowner = \"daemon-forge\"\nplatform_support",
        1,
    );
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::StaleVersion));
}

#[test]
fn a_version_pin_naming_an_undeclared_provider_is_rejected() {
    let rows = clean_fixture_rows().replacen(
        "owner = \"daemon-forge\"\nplatform_support",
        "version_support = { bitbucket = \"9\" }\nowner = \"daemon-forge\"\nplatform_support",
        1,
    );
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::UnknownProvider));
}

#[test]
fn an_owner_outside_the_manifests_table_is_rejected() {
    let rows = clean_fixture_rows().replacen("owner = \"daemon-forge\"", "owner = \"somebody\"", 1);
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::UnknownOwnerArea));
}

#[test]
fn an_optional_exclusion_needs_a_reason_and_a_preflight() {
    let rows = clean_fixture_rows().replacen(
        "disposition = \"required\"\nclass = \"read\"",
        "disposition = \"optional\"\nclass = \"read\"",
        1,
    );
    let (_d, inv) = load_fixture(&rows);
    let fired = rules(&validate::validate(&inv));
    assert!(fired.contains(&Rule::MissingExclusionReason), "{fired:?}");
    assert!(fired.contains(&Rule::MissingPreflight), "{fired:?}");
}

/// The non-waivability rule in all four of its shapes. #9777: profile reduction
/// may not *quietly* waive a claims / identity / review / CI / protection /
/// guarded-merge requirement — but declining one convenience operation inside
/// such a group is legitimate when a required row still carries the requirement.
#[test]
fn waiving_a_non_waivable_requirement_must_name_a_required_carrier() {
    let nw = super::model::NON_WAIVABLE_GROUPS[0];
    let optional_row = |extra: &str| {
        fixture_row("waived.op", nw, extra)
            .replace("disposition = \"required\"", "disposition = \"optional\"")
            + "exclusion_reason = \"a convenience\"\npreflight = \"warn-and-disable\"\n"
    };

    // 1. Names nothing: a quiet waiver.
    let (_d, inv) = load_fixture(&format!("{}{}", clean_fixture_rows(), optional_row("")));
    assert!(
        rules(&validate::validate(&inv)).contains(&Rule::QuietlyWaivedRequirement),
        "declining a row in a non-waivable group with no named carrier must fail"
    );

    // 2. Names a row that does not exist.
    let (_d, inv) = load_fixture(&format!(
        "{}{}",
        clean_fixture_rows(),
        optional_row("requirement_carried_by = [\"nope.missing\"]")
    ));
    assert!(rules(&validate::validate(&inv)).contains(&Rule::UnknownRequirementCarrier));

    // 3. Names a row that is itself not required.
    let mut rows = clean_fixture_rows();
    rows.push_str(
        &(fixture_row("other.optional", nw, "")
            .replace("disposition = \"required\"", "disposition = \"optional\"")
            + "exclusion_reason = \"x\"\npreflight = \"warn-and-disable\"\n\
               requirement_carried_by = [\"filler.op-0\"]\n"),
    );
    rows.push_str(&optional_row("requirement_carried_by = [\"other.optional\"]"));
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::UnknownRequirementCarrier));

    // 4. Names a real required row: allowed, and nothing else fires.
    let (_d, inv) = load_fixture(&format!(
        "{}{}",
        clean_fixture_rows(),
        optional_row("requirement_carried_by = [\"filler.op-0\"]")
    ));
    assert_eq!(validate::validate(&inv), vec![]);
}

#[test]
fn a_non_waivable_group_left_with_no_required_row_is_rejected() {
    // Carriers pointing at each other must not be able to empty a group out.
    let nw = super::model::NON_WAIVABLE_GROUPS[1];
    let idx = super::model::INVENTORY_GROUPS
        .iter()
        .position(|g| *g == nw)
        .expect("every non-waivable group is an inventory group");
    let rows = all_groups_rows()
        .replace(
            &format!(
                "id = \"filler.op-{idx}\"\ngroup = \"{nw}\"\nsummary = \"fixture\"\n\
                 profile = \"required-coordination\"\ndisposition = \"required\""
            ),
            &format!(
                "id = \"filler.op-{idx}\"\ngroup = \"{nw}\"\nsummary = \"fixture\"\n\
                 profile = \"required-coordination\"\ndisposition = \"optional\""
            ),
        )
        // Give the now-optional row a reason/preflight/carrier so only the
        // group-level rule can fire on it.
        .replace(
            &format!("test_id = \"fixture::filler-op-{idx}\""),
            &format!(
                "test_id = \"fixture::filler-op-{idx}\"\nexclusion_reason = \"x\"\n\
                 preflight = \"warn-and-disable\"\nrequirement_carried_by = [\"filler.op-0\"]"
            ),
        );
    let (_d, inv) = load_fixture(&rows);
    let fired = rules(&validate::validate(&inv));
    assert!(fired.contains(&Rule::GroupFullyWaived), "{fired:?}");
    assert!(!fired.contains(&Rule::QuietlyWaivedRequirement), "{fired:?}");
}

#[test]
fn a_required_row_may_not_sit_in_the_optional_profile() {
    let rows = clean_fixture_rows().replacen(
        "profile = \"required-coordination\"",
        "profile = \"optional\"",
        1,
    );
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::RequiredInOptionalProfile));
}

#[test]
fn claiming_caller_integration_without_a_caller_is_rejected() {
    let rows = clean_fixture_rows().replacen(
        "platform_support = \"documented\"",
        "platform_support = \"documented\"\ncaller_integration = \"complete\"",
        1,
    );
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::IntegrationWithoutCaller));
}

#[test]
fn an_unseeded_high_risk_case_and_a_caseless_high_risk_row_are_rejected() {
    // Drop the high-risk seeds entirely.
    let (_d, inv) = load_fixture(&all_groups_rows());
    assert!(rules(&validate::validate(&inv)).contains(&Rule::UnseededHighRiskCase));

    // …and a row claiming `risk = high` with no case named.
    let mut rows = clean_fixture_rows();
    rows.push_str(&fixture_row("lonely.highrisk", "quota-caching-errors", "risk = \"high\""));
    let (_d2, inv2) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv2)).contains(&Rule::HighRiskWithoutCase));
}

#[test]
fn an_implemented_test_claim_is_checked_against_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let rows = clean_fixture_rows().replacen(
        "test_id = \"fixture::filler-op-0\"",
        "test_id = \"fixture::proven-by-file\"\ntest_status = \"implemented\"\n\
         test_path = \"some/test_file.rs\"",
        1,
    );
    let (_fixture, inv) = load_fixture(&rows);

    // No such file: the stronger claim cannot be made for free.
    assert_eq!(
        rules(&validate::validate_test_evidence(&inv, dir.path())),
        BTreeSet::from([Rule::TestEvidenceMissing])
    );

    // File exists but does not contain the named test: still rejected.
    std::fs::create_dir_all(dir.path().join("some")).unwrap();
    std::fs::write(dir.path().join("some/test_file.rs"), "fn unrelated() {}").unwrap();
    assert_eq!(
        rules(&validate::validate_test_evidence(&inv, dir.path())),
        BTreeSet::from([Rule::TestEvidenceMissing])
    );

    // File contains it (with `-` normalized to `_`): accepted.
    std::fs::write(dir.path().join("some/test_file.rs"), "#[test]\nfn proven_by_file() {}")
        .unwrap();
    assert_eq!(validate::validate_test_evidence(&inv, dir.path()), vec![]);
}

#[test]
fn an_implemented_claim_without_a_test_path_is_rejected() {
    let rows = clean_fixture_rows().replacen(
        "test_id = \"fixture::filler-op-0\"",
        "test_id = \"fixture::filler-op-0\"\ntest_status = \"implemented\"",
        1,
    );
    let (_d, inv) = load_fixture(&rows);
    assert!(rules(&validate::validate(&inv)).contains(&Rule::TestEvidenceMissing));
}

// ============================================================================
// Version comparison
// ============================================================================

#[test]
fn version_comparison_handles_prereleases_and_short_pins() {
    // A non-numeric trailer contributes 0 rather than being parsed or dropped,
    // so `1.24.0-rc1` sorts just below `1.24.0.1` and not above `1.24.0`.
    assert_eq!(version_key("1.24.0-rc1"), vec![1, 24, 0, 0]);
    assert_eq!(version_key("1.24"), vec![1, 24]);
    assert!(is_stale_version("1.20", "1.24"));
    assert!(is_stale_version("1.23.9", "1.24"));
    assert!(!is_stale_version("1.24", "1.24"));
    assert!(!is_stale_version("1.24.1", "1.24"));
    assert!(!is_stale_version("2.0", "1.24"));
    // A floor of "0" accepts anything (GitHub has no version axis).
    assert!(!is_stale_version("0", "0"));
    // A non-numeric trailer compares by its numeric prefix, not lexically.
    assert!(!is_stale_version("11.0-forgejo", "11.0"));
}

// ============================================================================
// The change gate's scanner
// ============================================================================

#[test]
fn the_scanner_counts_real_calls_and_not_lookalikes() {
    let text = "\
gh issue list --label x
  gh api repos/o/r/issues
echo second; gh pr view 1 && gh label list
\"$GH_BIN\" issue edit 1 || gh issue edit 1
nothing here
tough api call
--gh api
loom-gh api
gh help
gh config get editor
";
    let (count, samples) = gate::scan_text(text, 3);
    assert_eq!(
        count, 5,
        "lines 1+2 one each, line 3 two, line 4 one (the `\"$GH_BIN\"` half is not \
         a literal `gh` and is not counted); samples={samples:?}"
    );
    assert_eq!(samples.len(), 3, "sample cap honoured");
    assert!(samples[0].starts_with("1:"), "samples carry line numbers");

    // A `gh` subcommand that is not a forge call is not counted.
    assert_eq!(gate::scan_text("gh help\ngh config get x\n", 3).0, 0);
    // `gh` as part of a longer token is not a call.
    assert_eq!(gate::scan_text("tough api\nmygh api\nx-gh api\n", 3).0, 0);
    // Dynamic construction IS counted (#9777 asks for it explicitly).
    assert_eq!(gate::scan_text("cmd=(gh api \"$route\")\n", 3).0, 1);
}

#[test]
fn the_scanner_scope_excludes_mirrors_fixtures_and_prohibitions() {
    assert!(gate::is_scannable("defaults/scripts/create-issue.sh"));
    assert!(gate::is_scannable("loom-daemon/src/forge_cmd.rs"));
    assert!(gate::is_scannable(".github/workflows/ci.yml"));
    // A label catalogue mentioning `gh api` in a description is prose.
    assert!(!gate::is_scannable(".github/labels.yml"));
    assert!(!gate::is_scannable("README.md"));

    // Installed mirrors are measured at their source, like the size budget —
    // including the quickstart templates' own nested `.loom/` trees, which the
    // root-prefix-only check missed (three copies of `worktree.sh` showed up in
    // the first generated baseline).
    assert!(gate::is_excluded(".loom/scripts/create-issue.sh"));
    assert!(gate::is_excluded("quickstarts/api/.loom/scripts/worktree.sh"));
    // Fixtures are classified, not counted.
    assert!(gate::is_excluded("defaults/hooks/tests/test-guard-mcp-tools.sh"));
    assert!(gate::is_excluded("loom-daemon/src/forge_cmd_tests.rs"));
    assert!(gate::is_excluded("loom-daemon/src/merge_pr/tests.rs"));
    // A guard that recognises `gh pr merge` in order to DENY it is not a caller.
    assert!(gate::is_excluded("defaults/hooks/guard-destructive-gh.sh"));
    assert!(!gate::is_excluded("defaults/scripts/merge-pr.sh"));
}

#[test]
fn evaluate_classifies_baselines_and_catches_growth() {
    let dir = tempfile::tempdir().unwrap();
    let w = |rel: &str, body: &str| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    w("declared.sh", "gh issue list\n");
    w("baselined.sh", "gh api x\n");
    w("grown.sh", "gh api x\ngh api y\n");
    w("brand-new.sh", "gh pr view 1\n");
    w("clean.sh", "echo hi\n");
    w(".loom/mirror.sh", "gh api x\n");

    let declared = BTreeSet::from(["declared.sh".to_string()]);
    let bypass = |path: &str, calls: usize| Bypass {
        path: path.to_string(),
        calls,
        owner: "daemon-forge".to_string(),
        removal_issue: 9799,
    };
    let baseline = Baseline {
        removal_epic: Some(9769),
        bypasses: vec![
            bypass("baselined.sh", 1),
            bypass("grown.sh", 1),
            bypass("gone.sh", 3),
        ],
    };
    let paths: Vec<String> = [
        "declared.sh",
        "baselined.sh",
        "grown.sh",
        "brand-new.sh",
        "clean.sh",
        ".loom/mirror.sh",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();

    let results = gate::evaluate(dir.path(), &paths, &declared, &baseline);
    let by_path: BTreeMap<&str, &gate::FileResult> =
        results.iter().map(|r| (r.path.as_str(), r)).collect();
    assert_eq!(by_path["declared.sh"].verdict, Verdict::Classified);
    assert_eq!(by_path["baselined.sh"].verdict, Verdict::Baselined);
    assert_eq!(by_path["grown.sh"].verdict, Verdict::BaselineGrew { recorded: 1 });
    assert_eq!(by_path["brand-new.sh"].verdict, Verdict::Unclassified);
    // No call sites, or out of scope: absent entirely.
    assert!(!by_path.contains_key("clean.sh"));
    assert!(!by_path.contains_key(".loom/mirror.sh"));

    assert!(by_path["brand-new.sh"].is_failure());
    assert!(by_path["grown.sh"].is_failure());
    assert!(!by_path["baselined.sh"].is_failure());
    // Shrinking is always fine, and surfaces as a droppable entry.
    assert_eq!(gate::resolved_entries(&results, &baseline), vec!["gone.sh"]);
}

#[test]
fn a_baseline_entry_needs_an_owner_and_a_removal_issue() {
    let owners = BTreeMap::from([("daemon-forge".to_string(), "x".to_string())]);
    let entry = |path: &str, owner: &str, removal_issue: u64| Bypass {
        path: path.to_string(),
        calls: 1,
        owner: owner.to_string(),
        removal_issue,
    };
    let baseline = Baseline {
        removal_epic: Some(9769),
        bypasses: vec![
            entry("a.sh", "", 9799),
            entry("b.sh", "nobody", 9799),
            entry("c.sh", "daemon-forge", 0),
            entry("c.sh", "daemon-forge", 1),
        ],
    };
    let problems = gate::validate_baseline(&baseline, &owners);
    assert_eq!(problems.len(), 4, "{problems:?}");
    assert!(problems.iter().any(|p| p.contains("no owner recorded")));
    assert!(problems
        .iter()
        .any(|p| p.contains("not declared in manifest.toml")));
    assert!(problems.iter().any(|p| p.contains("no removal_issue")));
    assert!(problems.iter().any(|p| p.contains("listed more than once")));
}

#[test]
fn the_rendered_baseline_round_trips_and_sorts() {
    let baseline = Baseline {
        removal_epic: Some(9769),
        bypasses: vec![
            Bypass {
                path: "z.sh".into(),
                calls: 2,
                owner: "daemon-forge".into(),
                removal_issue: 9799,
            },
            Bypass {
                path: "a.sh".into(),
                calls: 1,
                owner: "role-scripts".into(),
                removal_issue: 9799,
            },
        ],
    };
    let rendered = gate::render_baseline(&baseline);
    let back: Baseline = toml::from_str(&rendered).expect("rendered baseline must parse");
    assert_eq!(back.removal_epic, Some(9769));
    // Sorted by path, so regeneration never produces a reordering diff.
    assert_eq!(
        back.bypasses
            .iter()
            .map(|b| b.path.as_str())
            .collect::<Vec<_>>(),
        vec!["a.sh", "z.sh"]
    );
    assert_eq!(gate::render_baseline(&back), rendered, "rendering is a fixpoint");
}

#[test]
fn owner_attribution_is_derived_from_the_path() {
    for (path, owner) in [
        ("loom-daemon/src/merge_pr/mod.rs", "daemon-landing"),
        ("loom-daemon/src/forge_cmd.rs", "daemon-forge"),
        ("loom-daemon/src/cli/misc_cmds.rs", "daemon-cli"),
        ("loom-daemon/src/work_finder.rs", "daemon-sweep"),
        ("loom-daemon/src/ci_telemetry/mod.rs", "daemon-ci-telemetry"),
        ("loom-daemon/src/something_else.rs", "daemon-other"),
        ("defaults/scripts/create-issue.sh", "role-scripts"),
        ("scripts/install/setup-mcp.sh", "installer"),
        ("scripts/version.sh", "repo-tooling"),
        (".github/workflows/ci.yml", "ci-workflows"),
        ("install.sh", "installer"),
        ("uninstall.sh", "installer"),
        ("unmapped/place.sh", "unassigned"),
    ] {
        assert_eq!(gate::owner_for_path(path), owner, "for {path}");
    }
}

// ============================================================================
// Report and probe manifest
// ============================================================================

#[test]
fn the_report_keeps_the_four_coverage_axes_apart() {
    let inv = load_embedded().unwrap();
    let rep = report::build(&inv);
    assert_eq!(
        rep.rows.len(),
        inv.operations.iter().filter(|o| o.is_active()).count(),
        "prohibitions and test fixtures are classified, never counted as active"
    );
    // Platform support and caller integration are independently sourced, so a
    // row can be `documented` with no integration — the distinction #9792 needs.
    let probe_only = rep.rows.iter().filter(|r| r.probe_only_evidence).count();
    assert!(probe_only > 0, "phase 1 has provider-side evidence with no caller integration");
    for row in &rep.rows {
        assert_eq!(
            row.probe_only_evidence,
            row.platform_support != "unknown" && row.caller_integration == "none",
            "{}: probe-only evidence is derived from two axes, never one",
            row.id
        );
    }
    let text = report::render_text(&rep);
    for header in [
        "PLATFORM",
        "ADAPTER",
        "CALLER-INT",
        "EVIDENCE CAUTION",
        "OMISSIONS",
        "REMAINING UNKNOWNS",
    ] {
        assert!(text.contains(header), "report text is missing {header}");
    }
}

#[test]
fn the_probe_manifest_puts_high_risk_first_and_states_what_a_pass_proves() {
    let inv = load_embedded().unwrap();
    let all = probe::build(&inv, &[]);
    assert!(!all.entries.is_empty());
    // Every required profile, and no optional rows.
    assert!(!all.profiles.contains(&"optional"));
    assert!(all.entries.iter().all(|e| e.profile != "optional"));
    // High-risk entries sort ahead of normal ones (#9777: seed those first).
    let first_normal = all.entries.iter().position(|e| e.risk == "normal");
    let last_high = all.entries.iter().rposition(|e| e.risk == "high");
    if let (Some(fi), Some(la)) = (first_normal, last_high) {
        assert!(la < fi, "a high-risk entry sorted after a normal one");
    }
    // A platform-only entry must say, in data, what a pass does NOT establish.
    let platform_only = all
        .entries
        .iter()
        .find(|e| e.establishes == probe::EvidenceClass::Platform)
        .expect("phase 1 has platform-only entries");
    assert!(platform_only
        .does_not_establish
        .iter()
        .any(|s| s.contains("production Loom caller")));

    // Profile selection is honoured, which is what lets the hosted probes run
    // independently of any post-GO fleet migration (AC4).
    let coord = probe::build(&inv, &[Profile::RequiredCoordination]);
    assert_eq!(coord.profiles, vec!["required-coordination"]);
    assert!(coord.entries.len() < all.entries.len());
    assert!(coord
        .entries
        .iter()
        .all(|e| e.profile == "required-coordination"));
    // Serializable: the harness consumes this as JSON.
    assert!(serde_json::to_string(&coord)
        .unwrap()
        .contains("does_not_establish"));
}

#[test]
fn the_inventory_honours_the_evidence_constraints_it_records() {
    let inv = load_embedded().unwrap();
    // AC5's constraint applies to the inventory itself, not only to runtime
    // traces: no field may carry a credential-shaped or unprintable value.
    for op in &inv.operations {
        for field in op
            .inputs
            .iter()
            .chain(op.outputs.iter())
            .chain(op.required_fields.iter())
        {
            assert!(
                crate::forge_call_stats::sanitize(field).is_some(),
                "{}: field {field:?} is credential-shaped or unprintable",
                op.id
            );
        }
    }
    // The identity row must normalize an origin: that is what distinguishes two
    // accounts carrying the same slug on two different forges.
    let identity = inv.get("identity.viewer").expect("identity.viewer row");
    assert!(
        identity.outputs.iter().any(|o| o.contains("origin"))
            || identity
                .required_fields
                .iter()
                .any(|o| o.contains("origin")),
        "identity.viewer must normalize an origin: {:?}",
        identity.outputs
    );
    // Coverage defaults are pessimistic, so an unfilled row cannot read as a pass.
    assert_eq!(Support::default(), Support::Unknown);
    assert_eq!(Coverage::default(), Coverage::None);
}
