//! Direction B's old-daemon selection (#10716): the oldest PUBLISHED release
//! at or above `REQUIRES_DAEMON`, not the exact tag, because releases skip
//! versions. No network: the release listing is a closure, and the end-to-end
//! cases use a scratch git repo with no installed shell, so no forge call and
//! no subcommand probe runs.
//!
//! Then #10868: the three-valued subcommand probe against fake daemons, the
//! note that names a release still uploading, and the bounded download retry
//! (the download is a closure too).

use super::*;
use crate::test_git_helpers::run as git;

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
    assert_eq!(got.0, OldRelease::Published(v("0.19.895"), "v0.19.895".into()));
    assert!(got.1.is_empty(), "nothing was skipped: {:?}", got.1);
    assert_eq!(asked, ["v0.19.895"], "one lookup when the oldest answers");
}

#[test]
fn no_release_at_or_above_req_is_unpublished_without_a_lookup() {
    // After the post-merge bump VERSION == req, but no release is cut yet.
    let candidates = tags_at_or_above(&tags(&["v0.19.890", "v0.19.891"]), v("0.19.893"));
    let got = pick_oldest_published(&candidates, |_| panic!("no lookup expected")).unwrap();
    assert_eq!(got, (OldRelease::Unpublished, Vec::new()));
}

#[test]
fn a_release_still_uploading_is_skipped() {
    let candidates = tags_at_or_above(&tags(&["v0.19.893", "v0.19.895"]), v("0.19.893"));
    let got = pick_oldest_published(&candidates, |tag| Some(tag != "v0.19.893")).unwrap();
    assert_eq!(got.0, OldRelease::Published(v("0.19.895"), "v0.19.895".into()));
    assert_eq!(got.1, ["v0.19.893"], "the skipped tag is handed back");

    let got = pick_oldest_published(&candidates, |_| Some(false)).unwrap();
    assert_eq!(got.0, OldRelease::Unpublished, "nothing uploaded yet");
    assert_eq!(got.1, ["v0.19.893", "v0.19.895"]);
}

// ---- #10868: the note names the release still uploading ----

#[test]
fn stand_in_note_names_the_skipped_tag_and_the_reason() {
    let candidates = tags_at_or_above(&tags(&["v0.19.893", "v0.19.895"]), v("0.19.893"));
    let (picked, skipped) = pick_oldest_published(&candidates, |_| Some(false)).unwrap();
    assert_eq!(picked, OldRelease::Unpublished);
    let note = stand_in_note("B", v("0.19.893"), &skipped);
    assert!(note.contains("no release at or above 0.19.893 is published yet"), "{note}");
    assert!(note.contains("v0.19.893"), "names the first skipped tag: {note}");
    assert!(note.contains("assets are not uploaded yet"), "{note}");
    assert!(note.contains("the new daemon stands in"), "{note}");

    let one = stand_in_note("B", v("0.19.893"), &["v0.19.893".to_string()]);
    assert!(
        one.contains("release v0.19.893 is tagged but its assets are not uploaded yet"),
        "{one}"
    );
}

#[test]
fn stand_in_note_keeps_its_wording_when_no_release_is_tagged() {
    assert_eq!(
        stand_in_note("B", v("0.19.893"), &[]),
        "B: no release at or above 0.19.893 is published yet, so the new daemon stands in for it"
    );
    assert_eq!(skipped_releases_text(&[]), None);
}

// ---- #10868: a listed asset that fails to download is retried ----

#[test]
fn download_that_fails_twice_then_succeeds_passes() {
    let mut calls = 0;
    download_with_retry("v0.19.895", "loom-daemon-test", Duration::ZERO, || {
        calls += 1;
        calls == 3
    })
    .unwrap();
    assert_eq!(calls, 3);
}

#[test]
fn download_succeeding_first_time_is_tried_once() {
    let mut calls = 0;
    download_with_retry("v0.19.895", "loom-daemon-test", Duration::ZERO, || {
        calls += 1;
        true
    })
    .unwrap();
    assert_eq!(calls, 1);
}

#[test]
fn download_that_always_fails_stops_at_the_bound_and_names_tag_and_asset() {
    let mut calls = 0;
    let err = download_with_retry("v0.19.895", "loom-daemon-test", Duration::ZERO, || {
        calls += 1;
        false
    })
    .unwrap_err()
    .to_string();
    assert_eq!(calls, DOWNLOAD_ATTEMPTS);
    assert!(err.contains("v0.19.895"), "{err}");
    assert!(err.contains("loom-daemon-test"), "{err}");
    assert!(err.contains("may still be uploading"), "{err}");
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
        oldest_installed: false,
        supports_installed: v("0.19.0"),
        requires_daemon: v(req),
        invoked_files: Vec::new(),
    }
}

/// A stand-in old daemon that reports `version`.
fn fake_daemon(dir: &Path, version: &str) -> PathBuf {
    script(dir, &format!("fake-{version}"), &format!("echo 'loom-daemon {version}'"))
}

/// An executable `sh` script named `name` in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let bin = dir.join(name);
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
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

// ---- #10868: `--prev-ref` naming the claim itself is checked, not skipped ----

#[test]
fn prev_ref_equal_to_the_claim_is_checked_not_skipped_as_a_declared_break() {
    // The daily proof runs `--prev-ref v<SUPPORTS_INSTALLED>`.
    let repo = checkout("0.19.893", &["v0.19.0", "v0.19.891"]);
    let mut o = opts(repo.path(), "0.19.893", OldDaemon::Absent);
    o.prev_ref = Some("v0.19.0".into());
    let report = run(&o).unwrap();
    assert!(!report.notes.iter().any(|n| n.contains("declared break")), "{:?}", report.notes);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.starts_with("A (v0.19.0 installed files vs new daemon): ")),
        "{:?}",
        report.notes
    );

    // One patch below the claim is the declared break, and is skipped.
    o.supports_installed = v("0.19.1");
    let report = run(&o).unwrap();
    assert!(report.notes.iter().any(|n| n.contains("declared break")), "{:?}", report.notes);
}

// ---- #10868: the three-valued subcommand probe ----

fn oldest_installed_report(root: &Path, floor: &str) -> Result<Report> {
    let mut o = opts(root, "0.19.894", OldDaemon::Absent);
    o.oldest_installed = true;
    o.supports_installed = v(floor);
    let mut report = Report::default();
    direction_a(&o, v("0.19.893"), &mut Probe::default(), &mut report)?;
    Ok(report)
}

#[test]
fn oldest_installed_uses_the_exact_or_oldest_newer_tag_not_the_latest() {
    let repo = checkout("0.19.893", &["v0.19.890", "v0.19.892", "v0.19.893"]);
    for (floor, chosen) in [("0.19.890", "v0.19.890"), ("0.19.891", "v0.19.892")] {
        let report = oldest_installed_report(repo.path(), floor).unwrap();
        assert!(report.violations.is_empty(), "{:?}", report.violations);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.starts_with(&format!("A ({chosen} installed files vs new daemon): "))),
            "{:?}",
            report.notes
        );
        assert!(!report.notes.iter().any(|n| n.contains("declared break")));
    }
}

#[test]
fn oldest_installed_prospective_floor_runs_both_directions_without_a_release() {
    let repo = checkout("0.19.893", &["v0.19.891"]);
    let mut o = opts(repo.path(), "0.19.894", OldDaemon::Absent);
    o.oldest_installed = true;
    o.supports_installed = v("0.19.894");
    let report = run(&o).unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.starts_with("A (prospective current installed files vs new daemon): ")),
        "{:?}",
        report.notes
    );
    assert!(report
        .notes
        .iter()
        .any(|n| n.contains("does not prove compatibility with an absent release")));
    assert!(report
        .notes
        .iter()
        .any(|n| n.starts_with("B (new installed files vs daemon 0.19.894): ")));

    // The stand-in still exercises the current tree's invoked-file contract.
    o.invoked_files.push(".loom/scripts/missing.sh".into());
    let report = run(&o).unwrap();
    assert!(report
        .violations
        .iter()
        .any(|n| n.contains("A (prospective") && n.contains("missing.sh")));
}

#[test]
fn oldest_installed_rejects_missing_reached_floors_and_excludes_future_tags() {
    let repo = checkout("0.19.893", &["v0.19.890", "v0.19.895"]);
    // Future tags cannot satisfy a floor for this checkout. Nor may an
    // already-reached or beyond-next-patch floor masquerade as prospective.
    for floor in ["0.19.892", "0.19.893", "0.19.895"] {
        let err = oldest_installed_report(repo.path(), floor)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no eligible installed release tag"), "{err}");
        assert!(err.contains("fetch the release tags"), "{err}");
    }
    let report = oldest_installed_report(repo.path(), "0.19.894").unwrap();
    assert!(report.notes.iter().any(|n| n.contains("prospective")));
    assert!(!report.notes.iter().any(|n| n.starts_with("A (v0.19.895")));
}

#[test]
fn oldest_installed_does_not_replace_a_selected_tree_missing_required_files() {
    let repo = checkout("0.19.893", &["v0.19.891"]);
    // Present in current defaults, absent from the selected release tree.
    std::fs::write(repo.path().join("defaults/docs/new.md"), "new").unwrap();
    let mut o = opts(repo.path(), "0.19.894", OldDaemon::Absent);
    o.oldest_installed = true;
    o.supports_installed = v("0.19.891");
    o.invoked_files = vec![".loom/docs/new.md".into()];
    let report = run(&o).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    assert!(report.violations[0].contains("A (v0.19.891"));
    assert!(report.violations[0].contains("new.md"));
}

#[test]
fn oldest_installed_selected_tree_missing_or_broken_probes_fail_without_fallback() {
    for sub in ["gone", "killed"] {
        let repo = checkout("0.19.893", &[]);
        let root = repo.path();
        std::fs::create_dir_all(root.join("defaults/scripts")).unwrap();
        std::fs::write(root.join("defaults/scripts/call.sh"), format!("loom-daemon {sub}\n"))
            .unwrap();
        git(root, &["add", "defaults/scripts/call.sh"]);
        git(root, &["commit", "-qm", "old installed caller"]);
        git(root, &["tag", "v0.19.891"]);
        // Current files no longer need it; selecting them would hide the bug.
        std::fs::remove_file(root.join("defaults/scripts/call.sh")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../scripts/check-daemon-subcommand-versions.sh"),
            root.join(DETECTOR_PATH),
        )
        .unwrap();
        let mut o = opts(root, "0.19.894", OldDaemon::Absent);
        o.oldest_installed = true;
        o.supports_installed = v("0.19.891");
        o.new_daemon = probe_daemon(root);
        let report = run(&o).unwrap();
        assert!(!report.violations.is_empty(), "{sub}: {:?}", report.notes);
        assert!(report.violations.iter().any(|n| n.contains("A (v0.19.891")));
        assert!(!report.notes.iter().any(|n| n.contains("prospective")));
    }
}

#[test]
fn oldest_installed_git_lookup_failure_is_not_a_prospective_proof() {
    let dir = tempfile::tempdir().unwrap();
    let err = oldest_installed_report(dir.path(), "0.19.894")
        .unwrap_err()
        .to_string();
    assert!(err.contains("git tag -l failed"), "{err}");
}

/// A fake daemon whose answer to `<sub> --help` depends on `<sub>`.
fn probe_daemon(dir: &Path) -> PathBuf {
    script(
        dir,
        "probe-daemon",
        r#"case "$1" in
  ok) echo "Usage: loom-daemon ok"; exit 0 ;;
  own-usage) echo "usage: gh-shim <args>" >&2; exit 2 ;;
  gone) echo "error: unrecognized subcommand 'gone'" >&2; exit 2 ;;
  killed) kill -KILL $$ ;;
  panics) echo "thread 'main' panicked at src/main.rs:1:1:" >&2; exit 101 ;;
  exit-101) echo "refused" >&2; exit 101 ;;
esac"#,
    )
}

#[test]
fn probe_tells_present_missing_and_broken_apart() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = probe_daemon(dir.path());
    let mut probe = Probe::default();
    assert_eq!(probe.probe(&daemon, "ok"), Probed::Present);
    assert_eq!(
        probe.probe(&daemon, "own-usage"),
        Probed::Present,
        "a non-zero exit with its own usage is still the subcommand answering"
    );
    assert_eq!(
        probe.probe(&daemon, "exit-101"),
        Probed::Present,
        "exit 101 alone is not a panic"
    );
    assert_eq!(probe.probe(&daemon, "gone"), Probed::Missing);
    let Probed::Broken(why) = probe.probe(&daemon, "killed") else {
        panic!("a daemon killed by a signal must be broken");
    };
    assert!(why.contains("was killed") && why.contains("signal"), "{why}");
    let Probed::Broken(why) = probe.probe(&daemon, "panics") else {
        panic!("a panicking daemon must be broken");
    };
    assert!(why.contains("panicked") && why.contains("101"), "{why}");
}

fn pairs(list: &[(&str, &str)]) -> BTreeSet<(PathBuf, String)> {
    list.iter()
        .map(|(file, sub)| (PathBuf::from(file), (*sub).to_string()))
        .collect()
}

#[test]
fn a_crashing_probe_is_one_violation_and_never_a_per_file_gap() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = probe_daemon(dir.path());
    let mut probe = Probe::default();
    for sub in ["killed", "panics"] {
        // Two files call the same subcommand: still one violation.
        let gaps = probe.gaps(
            &daemon,
            pairs(&[
                ("a.sh", sub),
                ("b.sh", sub),
                ("a.sh", "ok"),
                ("a.sh", "gone"),
            ]),
        );
        assert_eq!(
            gaps.missing.keys().collect::<Vec<_>>(),
            ["gone"],
            "{sub}: only clap's refusal is a per-file gap"
        );
        assert_eq!(gaps.broken.len(), 1, "{sub}: {:?}", gaps.broken);
        let text = &gaps.broken[0];
        assert!(text.contains("probe-daemon"), "names the binary: {text}");
        assert!(text.contains(&format!("{sub} --help")), "names the subcommand: {text}");
        let status = if sub == "killed" {
            "signal"
        } else {
            "exit status: 101"
        };
        assert!(text.contains(status), "names the exit status: {text}");

        let mut report = Report::default();
        record_gaps("A", &gaps, dir.path(), &mut report);
        assert_eq!(report.violations.len(), 2, "{:?}", report.violations);
        assert!(gaps
            .summary("A", 2)
            .ends_with("1 subcommand gaps, 1 broken probes"));

        // The second direction to probe the same binary does not repeat it.
        let again = probe.gaps(&daemon, pairs(&[("c.sh", sub)]));
        assert!(again.broken.is_empty(), "{:?}", again.broken);
        assert!(again.missing.is_empty(), "never cached as a gap or as present");
    }
}

#[test]
fn a_daemon_that_cannot_be_executed_is_broken_and_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let not_executable = dir.path().join("not-executable");
    std::fs::write(&not_executable, "#!/bin/sh\nexit 0\n").unwrap();
    for daemon in [not_executable, dir.path().join("no-such-daemon")] {
        let mut probe = Probe::default();
        let gaps = probe
            .gaps(&daemon, pairs(&[("a.sh", "forge"), ("a.sh", "merge-pr"), ("b.sh", "lease")]));
        assert!(gaps.missing.is_empty(), "{:?}", gaps.missing);
        assert_eq!(gaps.broken.len(), 1, "once, not once per subcommand: {:?}", gaps.broken);
        assert!(gaps.broken[0].contains("could not be executed"), "{:?}", gaps.broken);
        assert!(gaps.broken[0].contains(&daemon.display().to_string()), "{:?}", gaps.broken);
        assert!(probe
            .gaps(&daemon, pairs(&[("c.sh", "init")]))
            .broken
            .is_empty());
    }
}

#[test]
fn a_clean_probe_summary_keeps_its_wording() {
    let gaps = Gaps::default();
    assert_eq!(gaps.summary("A", 3), "A: 3 shell files, 0 subcommand gaps");
}
