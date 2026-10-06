//! W9: which `(repo, number)` a write argv pins (`own_writes`).

use super::*;

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

fn numbers(parts: &[&str]) -> Vec<u32> {
    written_targets(&argv(parts), None)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

fn at(repo: &str, n: u32) -> Key {
    (Some(repo.to_string()), n)
}

#[test]
fn api_paths_name_the_issue_or_pr_they_write() {
    for (args, want) in [
        (&["api", "-X", "POST", "repos/acme/app/issues/12/labels"][..], vec![12]),
        (
            &[
                "api",
                "--method",
                "DELETE",
                "/repos/acme/app/issues/7/labels/x",
            ][..],
            vec![7],
        ),
        (&["api", "-X", "PATCH", "repos/acme/app/issues/9"][..], vec![9]),
        (
            &[
                "api",
                "-X",
                "POST",
                "repos/acme/app/issues/3/comments",
                "-f",
                "body=b",
            ][..],
            vec![3],
        ),
        (&["api", "-X", "PATCH", "repos/acme/app/pulls/40"][..], vec![40]),
        // A comment edited by id names no issue number.
        (&["api", "-X", "PATCH", "repos/acme/app/issues/comments/9911"][..], vec![]),
        (&["api", "graphql", "-f", "query=mutation{}"][..], vec![]),
    ] {
        assert_eq!(numbers(args), want, "{args:?}");
    }
}

#[test]
fn issue_and_pr_verbs_name_their_selector() {
    for (args, want) in [
        (&["issue", "edit", "12", "--add-label", "loom:building"][..], vec![12]),
        (&["issue", "edit", "--repo", "acme/app", "12"][..], vec![12]),
        (
            &[
                "issue",
                "comment",
                "https://github.com/acme/app/issues/5",
                "-b",
                "x",
            ][..],
            vec![5],
        ),
        (&["pr", "edit", "#33", "--remove-label", "loom:pr"][..], vec![33]),
        (&["pr", "merge", "--squash", "--delete-branch", "34"][..], vec![34]),
        (&["issue", "create", "--title", "t"][..], vec![]),
        (&["label", "create", "x"][..], vec![]),
    ] {
        assert_eq!(numbers(args), want, "{args:?}");
    }
}

/// A mis-parse may add a pin, never drop one: verb-dependent short flags and
/// unknown flags cannot swallow the selector, and a numeric flag value may be
/// pinned alongside it.
#[test]
fn every_numeric_argument_is_pinned_unless_a_number_free_flag_owns_it() {
    assert_eq!(numbers(&["issue", "close", "-r", "completed", "12"]), vec![12]);
    assert_eq!(numbers(&["issue", "close", "12", "-r", "completed"]), vec![12]);
    let n = numbers(&["issue", "edit", "--milestone", "3", "12"]);
    assert!(n.contains(&12), "{n:?}");
    assert_eq!(numbers(&["issue", "edit", "--milestone=3", "12"]), vec![12]);
    let n = numbers(&["issue", "edit", "--milestone", "3", "--unknownflag", "12"]);
    assert!(n.contains(&12), "{n:?}");
    assert_eq!(numbers(&["pr", "review", "-r", "-b", "7", "15"]), vec![15]);
    assert_eq!(numbers(&["pr", "comment", "-b", "7", "--", "15"]), vec![15]);
    assert_eq!(numbers(&["issue", "edit", "12", "--add-label", "404"]), vec![12]);
}

#[test]
fn the_repo_comes_from_the_path_the_flag_the_url_or_the_target() {
    let t = |parts: &[&str], target| written_targets(&argv(parts), target);
    assert_eq!(
        t(&["api", "-X", "PATCH", "repos/Acme/App/issues/9"], None),
        vec![at("acme/app", 9)]
    );
    assert_eq!(
        t(&["issue", "edit", "4", "--repo", "acme/other"], Some("acme/app")),
        vec![at("acme/other", 4)],
        "the argv's repo wins over the target"
    );
    assert_eq!(t(&["issue", "edit", "-R", "acme/app", "4"], None), vec![at("acme/app", 4)]);
    assert_eq!(
        t(&["issue", "comment", "https://github.com/acme/app/issues/5"], None),
        vec![at("acme/app", 5)]
    );
    assert_eq!(t(&["issue", "edit", "6"], Some("acme/app")), vec![at("acme/app", 6)]);
    assert_eq!(t(&["issue", "edit", "6"], None), vec![(None, 6)]);
}

/// `{owner}`/`{repo}` (or `:owner`) is filled in by `gh` from the working
/// directory: the literal is never a slug, so it takes the target or becomes
/// the wildcard.
#[test]
fn a_placeholder_api_path_uses_the_target_or_the_wildcard() {
    let t = |parts: &[&str], target| written_targets(&argv(parts), target);
    let path = "repos/{owner}/{repo}/issues/9/labels";
    assert_eq!(t(&["api", "-X", "POST", path], None), vec![(None, 9)]);
    assert_eq!(t(&["api", "-X", "POST", path], Some("Acme/App")), vec![at("acme/app", 9)]);
    assert_eq!(t(&["api", "-X", "PATCH", "repos/:owner/:repo/issues/9"], None), vec![(None, 9)]);
    assert_eq!(t(&["api", "-X", "PATCH", "repos/acme/{repo}/issues/9"], None), vec![(None, 9)]);
}

#[test]
fn the_repo_flag_is_normalised_to_owner_slash_repo() {
    let t = |parts: &[&str]| written_targets(&argv(parts), Some("acme/target"));
    assert_eq!(t(&["issue", "edit", "-Racme/app", "4"]), vec![at("acme/app", 4)]);
    assert_eq!(t(&["issue", "edit", "-R=acme/app", "4"]), vec![at("acme/app", 4)]);
    assert_eq!(
        t(&[
            "issue",
            "edit",
            "--repo",
            "github.example.com/Acme/App",
            "4"
        ]),
        vec![at("acme/app", 4)]
    );
    assert_eq!(
        t(&[
            "issue",
            "edit",
            "--repo=https://github.com/Acme/App.git",
            "4"
        ]),
        vec![at("acme/app", 4)]
    );
    assert_eq!(
        t(&["pr", "edit", "-R", "https://github.com/acme/app/", "4"]),
        vec![at("acme/app", 4)]
    );
}

#[test]
fn a_noted_write_pins_only_its_own_number_for_the_window() {
    note(
        &argv(&["issue", "edit", "4711", "--add-label", "loom:building"]),
        Some("acme/app"),
    );
    assert!(written_within("acme/app", 4711, Duration::from_secs(600)));
    assert!(!written_within("acme/app", 4712, Duration::from_secs(600)));
    assert!(
        !written_within("acme/app", 4711, Duration::ZERO),
        "outside the window it is not pinned"
    );
}

/// A write to #N in one repo does not pin #N in another; a write whose repo
/// is unknown pins #N everywhere (the safe direction).
#[test]
fn a_pin_is_scoped_to_its_repo_unless_the_repo_is_unknown() {
    let window = Duration::from_secs(600);
    note(&argv(&["api", "-X", "POST", "repos/acme/app/issues/4720/labels"]), None);
    assert!(written_within("acme/app", 4720, window));
    assert!(written_within("ACME/App", 4720, window), "slugs compare case-insensitively");
    assert!(!written_within("acme/other", 4720, window));

    note(&argv(&["issue", "edit", "4721"]), None);
    assert!(written_within("acme/app", 4721, window));
    assert!(written_within("acme/other", 4721, window));
}
