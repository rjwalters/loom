use super::*;

#[test]
fn a_standard_dependabot_title_yields_one_bump() {
    let bumps = extract_bumps("Bump serde from 1.0.1 to 1.0.2");
    assert_eq!(bumps.len(), 1);
    assert_eq!(bumps[0].from, "1.0.1");
    assert_eq!(bumps[0].to, "1.0.2");
    assert_eq!(bumps[0].level, Some(BumpLevel::Patch));
}

#[test]
fn the_conventional_commit_and_directory_title_forms_parse_too() {
    for title in [
        "chore(deps): bump serde from 1.0.1 to 1.1.0",
        "Bump serde from 1.0.1 to 1.1.0 in /mcp-loom",
        "Bump serde from v1.0.1 to v1.1.0",
    ] {
        let bumps = extract_bumps(title);
        assert_eq!(bumps.len(), 1, "{title}");
        assert_eq!(bumps[0].level, Some(BumpLevel::Minor), "{title}");
    }
}

#[test]
fn a_body_sentence_does_not_swallow_its_trailing_period() {
    let bumps = extract_bumps("Bumps [serde](https://example.test) from 1.0.1 to 1.0.2.");
    assert_eq!(bumps[0].to, "1.0.2");
    assert_eq!(bumps[0].level, Some(BumpLevel::Patch));
}

#[test]
fn a_grouped_pr_takes_its_bumps_from_the_body() {
    // The grouped title carries no versions at all — the body does.
    let title = "Bump the cargo group across 1 directory with 3 updates";
    let body = "\
Bumps the cargo group with 3 updates:

Updates `serde` from 1.0.1 to 1.0.2
Updates `regex` from 1.11.0 to 1.12.0
Updates `libc` from 0.2.170 to 0.2.171
";
    assert!(extract_bumps(title).is_empty());
    assert_eq!(extract_bumps(body).len(), 3);
    // AC: a grouped PR qualifies only if EVERY bump in it does.
    assert_eq!(evaluate(MaxSemver::Minor, title, body), SemverVerdict::Within(BumpLevel::Minor));
}

#[test]
fn a_grouped_pr_is_rejected_when_any_single_member_exceeds_the_ceiling() {
    let title = "Bump the npm group with 2 updates";
    let body = "Updates `a` from 1.0.0 to 1.0.1\nUpdates `b` from 1.0.0 to 2.0.0\n";
    assert_eq!(
        evaluate(MaxSemver::Minor, title, body),
        SemverVerdict::Exceeded(BumpLevel::Major)
    );
}

#[test]
fn zero_major_versions_use_caret_compatibility_semantics() {
    // Cargo/npm both treat 0.4 -> 0.5 as breaking. Reading it as "minor" would
    // let a `minor` ceiling admit exactly what the ceiling exists to exclude.
    assert_eq!(classify_pair("0.4.0", "0.5.0"), Some(BumpLevel::Major));
    assert_eq!(classify_pair("0.2.170", "0.2.171"), Some(BumpLevel::Minor));
    assert_eq!(classify_pair("0.0.1", "0.0.2"), Some(BumpLevel::Minor));
}

#[test]
fn normal_versions_classify_by_the_first_differing_component() {
    assert_eq!(classify_pair("1.2.3", "2.0.0"), Some(BumpLevel::Major));
    assert_eq!(classify_pair("1.2.3", "1.3.0"), Some(BumpLevel::Minor));
    assert_eq!(classify_pair("1.2.3", "1.2.4"), Some(BumpLevel::Patch));
    assert_eq!(classify_pair("1.2.3", "1.2.3"), Some(BumpLevel::Patch));
    assert_eq!(classify_pair("1.2", "1.3"), Some(BumpLevel::Minor));
    assert_eq!(classify_pair("1.2.3-rc1", "1.2.3"), Some(BumpLevel::Patch));
}

#[test]
fn a_non_numeric_version_is_unclassifiable_rather_than_assumed_safe() {
    assert_eq!(classify_pair("latest", "1.0.0"), None);
    assert_eq!(classify_pair("1.0.0", "main"), None);
}

#[test]
fn the_all_ceiling_evaluates_nothing() {
    assert_eq!(
        evaluate(MaxSemver::All, "Bump x from 1.0.0 to 9.0.0", ""),
        SemverVerdict::NotEvaluated
    );
}

#[test]
fn a_set_ceiling_with_no_parseable_pair_fails_closed() {
    assert_eq!(
        evaluate(MaxSemver::Patch, "Update the vendored toolchain", ""),
        SemverVerdict::Unparseable
    );
    assert_eq!(
        evaluate(MaxSemver::Patch, "Bump x from latest to 1.0.0", ""),
        SemverVerdict::Unparseable,
        "an unclassifiable pair must never read as within the ceiling"
    );
}

#[test]
fn the_patch_ceiling_admits_only_patch_bumps() {
    assert_eq!(
        evaluate(MaxSemver::Patch, "Bump x from 1.0.0 to 1.0.1", ""),
        SemverVerdict::Within(BumpLevel::Patch)
    );
    assert_eq!(
        evaluate(MaxSemver::Patch, "Bump x from 1.0.0 to 1.1.0", ""),
        SemverVerdict::Exceeded(BumpLevel::Minor)
    );
}

#[test]
fn admitted_by_orders_the_three_ceilings() {
    assert!(BumpLevel::Major.admitted_by(MaxSemver::All));
    assert!(BumpLevel::Minor.admitted_by(MaxSemver::Minor));
    assert!(!BumpLevel::Major.admitted_by(MaxSemver::Minor));
    assert!(BumpLevel::Patch.admitted_by(MaxSemver::Patch));
    assert!(!BumpLevel::Minor.admitted_by(MaxSemver::Patch));
}
