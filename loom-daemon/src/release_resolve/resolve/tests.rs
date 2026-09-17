//! Tests for resolution's decision order and its never-fabricate rule
//! (epic #7810, PR 5).
//!
//! The `gh` calls themselves are covered end to end by
//! `test-loom-daemon-update.sh`'s 27 `--resolve-json` assertions, which were
//! written against the shell. What is here is what those cannot reach cheaply:
//! the gates that must refuse *before* any forge call, and the shape of a
//! refusal.

use super::*;

fn inputs(root: &Path) -> Inputs<'_> {
    Inputs {
        repo_root: root,
        target_override: None,
        repo_override: None,
        installed_bin: None,
        fetch_disabled: false,
    }
}

fn reason(r: &Resolution) -> String {
    match r {
        Resolution::Unresolved(s) => s.clone(),
        Resolution::Resolved(_) => panic!("expected Unresolved"),
    }
}

#[test]
fn fetch_disabled_is_reported_before_anything_is_asked_of_the_forge() {
    // Order matters: an operator who turned the artifact path off fleet-wide
    // must get THAT as the reason. Reporting a forge problem instead would
    // send them debugging an API that was never consulted — and this gate is
    // also what keeps the tick falling back to source on such a host.
    let root = std::env::temp_dir();
    let mut i = inputs(&root);
    i.fetch_disabled = true;
    // A bogus repo override proves no call was made: if one were, the reason
    // would name the forge failure instead.
    i.repo_override = Some("no-such-owner/no-such-repo".to_string());
    i.target_override = Some("aarch64-apple-darwin".to_string());
    assert!(
        reason(&resolve(&i)).contains("artifact-fetch is disabled"),
        "{}",
        reason(&resolve(&i))
    );
}

#[test]
fn an_unmapped_platform_refuses_without_a_forge_call() {
    // No target triple means no artifact can exist for this host. Asking the
    // forge anyway would spend a call to learn nothing.
    let root = std::env::temp_dir();
    let mut i = inputs(&root);
    i.target_override = Some(String::new()); // empty = "not overridden"
                                             // Cannot force the host mapping to fail portably, so assert the shape of
                                             // whichever branch this host takes: either a triple resolved and the next
                                             // gate (repo slug) refused, or the platform gate refused. Both refuse
                                             // before any release query.
    let r = resolve(&i);
    let why = reason(&r);
    assert!(
        why.contains("unrecognized host platform") || why.contains("could not resolve owner/repo"),
        "{why}"
    );
}

#[test]
fn an_unresolvable_repo_slug_refuses_and_names_the_override() {
    // The reason has to be actionable: the operator's fix is an env var, so
    // the message says which one.
    let root = std::env::temp_dir(); // no git remote here
    let mut i = inputs(&root);
    i.target_override = Some("aarch64-apple-darwin".to_string());
    let why = reason(&resolve(&i));
    assert!(why.contains("could not resolve owner/repo"), "{why}");
    assert!(why.contains("LOOM_DAEMON_UPDATE_GH_REPO"), "{why}");
}

#[test]
fn an_empty_override_falls_through_to_detection_rather_than_being_used() {
    // `${LOOM_DAEMON_UPDATE_TARGET:-$(detect)}` — an empty env var means unset,
    // not "the empty triple". Using it literally would ask the forge for
    // `loom-daemon-` assets.
    let root = std::env::temp_dir();
    let mut i = inputs(&root);
    i.target_override = Some(String::new());
    i.repo_override = Some(String::new());
    let why = reason(&resolve(&i));
    assert!(
        !why.contains("checked for loom-daemon- "),
        "an empty override must not become a literal target: {why}"
    );
}

#[test]
fn an_absent_installed_binary_reports_no_identity_rather_than_a_guess() {
    // (None, None), not ("", "") — an empty version would compare as older
    // than every release and roll a host that simply had no binary to ask.
    let (v, c) = installed_identity(None);
    assert_eq!(v, None);
    assert_eq!(c, None);
}

#[test]
fn a_binary_that_will_not_answer_reports_no_identity() {
    let (v, c) = installed_identity(Some(Path::new("/nonexistent/loom-daemon")));
    assert_eq!(v, None);
    assert_eq!(c, None);
}

#[test]
fn a_missing_version_file_yields_none_not_an_empty_string() {
    assert_eq!(read_source_version(Path::new("/nonexistent")), None);
}

#[test]
fn a_version_file_is_read_with_whitespace_stripped() {
    let dir = std::env::temp_dir().join(format!("loom-ver-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("VERSION"), "  0.19.24\n").expect("write");
    let got = read_source_version(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(got.as_deref(), Some("0.19.24"));
}

#[test]
fn an_empty_version_file_yields_none() {
    let dir = std::env::temp_dir().join(format!("loom-ver-empty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("VERSION"), "\n \n").expect("write");
    let got = read_source_version(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(got, None);
}

#[test]
fn the_asset_scratch_directory_does_not_outlive_the_call() {
    // Resolution runs on every auto-update tick; a leaked directory per tick
    // is a real leak. The call fails (no such release) but must still clean up.
    let before = count_scratch_dirs();
    let _ = fetch_asset_sha256(
        "v0.0.0-nonexistent",
        "no-such-owner/no-such-repo",
        "loom-daemon-x.sha256",
        &std::env::temp_dir(),
    );
    assert_eq!(count_scratch_dirs(), before, "scratch directory leaked");
}

fn count_scratch_dirs() -> usize {
    std::fs::read_dir(std::env::temp_dir())
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("loom-daemon-resolve-")
                })
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn every_refusal_names_what_it_actually_tried() {
    // Carried forward from #7818/#7999, which added "name the exact script and
    // fold in a stderr tail" to the shell-out this PR deletes. There is no
    // child process to quote any more, so the equivalent guarantee is that each
    // reason is constructed at its own failure point and names the concrete
    // thing that failed — never a bare "no release artifact resolved" that
    // leaves an operator reconstructing which step gave up.
    let root = std::env::temp_dir();

    let mut disabled = inputs(&root);
    disabled.fetch_disabled = true;
    let why = reason(&resolve(&disabled));
    assert!(
        why.contains("--no-fetch") && why.contains("LOOM_DAEMON_UPDATE_FETCH"),
        "must name the switch an operator would flip back: {why}"
    );

    let mut no_repo = inputs(&root);
    no_repo.target_override = Some("aarch64-apple-darwin".to_string());
    let why = reason(&resolve(&no_repo));
    assert!(
        why.contains("origin") && why.contains("LOOM_DAEMON_UPDATE_GH_REPO"),
        "must name both what was consulted and the override: {why}"
    );

    for r in [
        Resolution::Unresolved("x".into()),
        resolve(&disabled),
        resolve(&no_repo),
    ] {
        let why = reason(&r);
        assert!(
            !why.eq_ignore_ascii_case("no release artifact resolved"),
            "a bare generic reason tells an operator nothing: {why}"
        );
    }
}
