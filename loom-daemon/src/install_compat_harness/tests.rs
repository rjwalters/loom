//! Direction B's old-daemon selection (#10716): the oldest PUBLISHED release
//! at or above `REQUIRES_DAEMON`, not the exact tag, because releases skip
//! versions. No network: the release listing is a closure, and the end-to-end
//! cases use a scratch git repo with no installed shell, so no forge call and
//! no subcommand probe runs.

use super::*;
use std::process::Command;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn tags(list: &[&str]) -> String {
    list.join("\n")
}

#[test]
fn tags_at_or_above_filters_and_sorts_oldest_first() {
    let got = tags_at_or_above(
        &tags(&[
            "v0.19.900",
            "v0.19.10",
            "nightly",
            "v0.19.899",
            "v0.19.9",
            "v0.20.0",
        ]),
        v("0.19.10"),
    );
    let names: Vec<&str> = got.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(names, ["v0.19.10", "v0.19.899", "v0.19.900", "v0.20.0"]);
}

#[test]
fn exact_tag_missing_uses_the_oldest_newer_release() {
    // req 0.19.893 was skipped; 0.19.895 and 0.19.897 are published.
    let candidates = tags_at_or_above(
        &tags(&["v0.19.890", "v0.19.891", "v0.19.895", "v0.19.897"]),
        v("0.19.893"),
    );
    let mut asked = Vec::new();
    let got = pick_oldest_published(&candidates, |tag| {
        asked.push(tag.to_string());
        Some(true)
    })
    .unwrap();
    assert_eq!(got, OldRelease::Published(v("0.19.895"), "v0.19.895".into()));
    assert_eq!(asked, ["v0.19.895"], "one lookup when the oldest answers");
}

#[test]
fn no_release_at_or_above_req_is_unpublished_without_a_lookup() {
    // After the post-merge bump VERSION == req, but no release is cut yet.
    let candidates = tags_at_or_above(&tags(&["v0.19.890", "v0.19.891"]), v("0.19.893"));
    let got = pick_oldest_published(&candidates, |_| panic!("no lookup expected")).unwrap();
    assert_eq!(got, OldRelease::Unpublished);
}

#[test]
fn a_release_still_uploading_is_skipped() {
    let candidates = tags_at_or_above(&tags(&["v0.19.893", "v0.19.895"]), v("0.19.893"));
    let got = pick_oldest_published(&candidates, |tag| Some(tag != "v0.19.893")).unwrap();
    assert_eq!(got, OldRelease::Published(v("0.19.895"), "v0.19.895".into()));

    let got = pick_oldest_published(&candidates, |_| Some(false)).unwrap();
    assert_eq!(got, OldRelease::Unpublished, "nothing uploaded yet");
}

#[test]
fn an_unanswerable_lookup_is_an_error_not_a_skip() {
    let candidates = tags_at_or_above(&tags(&["v0.19.893", "v0.19.895"]), v("0.19.893"));
    let err = pick_oldest_published(&candidates, |_| None).unwrap_err();
    assert!(err.to_string().contains("v0.19.893"), "{err}");
}

#[test]
fn lookups_are_capped() {
    let list: Vec<String> = (900..920).map(|n| format!("v0.19.{n}")).collect();
    let candidates = tags_at_or_above(&list.join("\n"), v("0.19.900"));
    let mut calls = 0;
    let err = pick_oldest_published(&candidates, |_| {
        calls += 1;
        Some(false)
    })
    .unwrap_err();
    assert_eq!(calls, MAX_RELEASE_LOOKUPS);
    assert!(err.to_string().contains("--old-daemon"), "{err}");
}

// ---- end to end through `run`, no forge, no probe ----

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A checkout at `version` with release tags `release_tags` and a `defaults/`
/// holding no shell, so neither direction runs the detector or a daemon.
fn checkout(version: &str, release_tags: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join("defaults/docs")).unwrap();
    std::fs::write(root.join("defaults/docs/x.md"), "x\n").unwrap();
    std::fs::write(root.join("VERSION"), format!("{version}\n")).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    for t in release_tags {
        git(root, &["tag", t]);
    }
    dir
}

fn opts(root: &Path, req: &str, old_daemon: OldDaemon) -> CheckOptions {
    CheckOptions {
        repo_root: root.to_path_buf(),
        new_daemon: PathBuf::from("/nonexistent/new-daemon"),
        old_daemon,
        prev_ref: None,
        supports_installed: v("0.19.0"),
        requires_daemon: v(req),
        invoked_files: Vec::new(),
    }
}

/// A stand-in old daemon that reports `version`.
fn fake_daemon(dir: &Path, version: &str) -> PathBuf {
    let bin = dir.join(format!("fake-{version}"));
    std::fs::write(&bin, format!("#!/bin/sh\necho 'loom-daemon {version}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn main_after_the_bump_with_no_release_yet_passes_as_unreleased() {
    // The blocking case: REQUIRES_DAEMON was VERSION + 1, the post-merge bump
    // made VERSION == req, and no release is cut. Fetch and Absent both pass.
    let repo = checkout("0.19.893", &["v0.19.890", "v0.19.891"]);
    let fetch = OldDaemon::Fetch {
        repo: "example/none".into(),
        asset: "loom-daemon-test".into(),
    };
    for old in [fetch, OldDaemon::Absent] {
        let report = run(&opts(repo.path(), "0.19.893", old)).unwrap();
        assert!(report.violations.is_empty(), "{:?}", report.violations);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("no release at or above 0.19.893 is published")),
            "{:?}",
            report.notes
        );
    }
}

#[test]
fn absent_binary_with_a_release_at_or_above_req_names_it() {
    let repo = checkout("0.19.897", &["v0.19.891", "v0.19.895"]);
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Absent)).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    assert!(report.violations[0].contains("v0.19.895"), "{:?}", report.violations);
}

#[test]
fn given_newer_release_stands_in_for_a_skipped_req() {
    let repo = checkout("0.19.897", &["v0.19.891", "v0.19.895"]);
    let bin = fake_daemon(repo.path(), "0.19.895");
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Given(bin))).unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n
                .contains("daemon 0.19.895, the oldest published release at or above 0.19.893")),
        "{:?}",
        report.notes
    );
}

#[test]
fn given_binary_must_be_the_oldest_release_at_or_above_req() {
    let repo = checkout("0.19.899", &["v0.19.891", "v0.19.895", "v0.19.897"]);
    let too_new = fake_daemon(repo.path(), "0.19.897");
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Given(too_new))).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    assert!(report.violations[0].contains("v0.19.895"), "{:?}", report.violations);

    let too_old = fake_daemon(repo.path(), "0.19.891");
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Given(too_old))).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    assert!(
        report.violations[0].contains("below the claimed 0.19.893"),
        "{:?}",
        report.violations
    );
}
