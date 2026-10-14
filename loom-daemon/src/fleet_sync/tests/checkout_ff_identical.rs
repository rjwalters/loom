//! Row 9's exception (#11553): a checkout whose every dirty tracked file is
//! already byte-identical to `origin/<default>` is fast-forwarded, the
//! identical edits discarded immediately before the merge. Anything else
//! stays `dirty`, untouched.
//!
//! Real git in temp dirs only, as in the parent module.

use super::*;

fn read(fx: &Fixture, path: &str) -> String {
    fs::read_to_string(fx.host.join(path)).unwrap()
}

#[test]
fn identical_dirt_unstaged_is_discarded_and_fast_forwarded() {
    let fx = Fixture::new();
    let tip = fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");

    let found = pass(&fx, &Knobs::default());

    let report = only(&found);
    assert_eq!(report.state, CheckoutState::FastForwarded, "{report:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "", "and the tree is clean");
    assert_eq!(read(&fx, "README.md"), "two\n");
    let detail = report.detail.as_deref().unwrap();
    assert!(
        detail.contains("identical to origin") && detail.contains("README.md"),
        "{detail}"
    );
}

#[test]
fn identical_dirt_staged_copy_of_origin_is_fast_forwarded() {
    let fx = Fixture::new();
    let tip = fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    git(&fx.host, &["add", "README.md"]);

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "");
}

#[test]
fn identical_dirt_differing_file_stays_dirty() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "mine\n");

    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert!(report.detail.as_deref().unwrap().contains("README.md"), "{report:?}");
}

#[test]
fn identical_dirt_mixed_identical_and_differing_stays_dirty() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    write(&fx.host.join(".loom/scripts/spawn.sh"), "#!/bin/sh\necho mine\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "README.md"), "two\n", "the identical file is not discarded either");
}

#[test]
fn identical_dirt_staged_but_different_stays_dirty() {
    // The working tree equals origin, but the index holds a third blob: real
    // local work that `checkout HEAD --` would lose.
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "staged third\n");
    git(&fx.host, &["add", "README.md"]);
    write(&fx.host.join("README.md"), "two\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(git(&fx.host, &["show", ":README.md"]), "staged third");
}

#[test]
fn identical_dirt_diverged_is_unchanged() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    fx.commit_locally("src/local.rs");
    write(&fx.host.join("README.md"), "two\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Diverged);
}

#[test]
fn identical_dirt_with_autoapply_off_writes_nothing() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    let off = Knobs {
        write: false,
        ..Knobs::default()
    };

    let report = assert_skipped(&fx, &off, CheckoutState::Behind);
    assert!(
        report
            .detail
            .as_deref()
            .unwrap()
            .contains("identical to origin"),
        "{report:?}"
    );
}

#[test]
fn identical_dirt_on_a_held_host_writes_nothing() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    let held = Knobs {
        held: Some("draining"),
        ..Knobs::default()
    };

    assert_skipped(&fx, &held, CheckoutState::Behind);
}

/// `paths` land upstream holding `"one\n"` and the host pulls them, so each
/// is in HEAD before the change under test.
fn in_head(fx: &Fixture, paths: &[&str]) {
    for path in paths {
        fx.push(path, "one\n");
    }
    git(&fx.host, &["pull", "--quiet", "--ff-only", "origin", fx.branch]);
}

#[test]
fn identical_dirt_non_ascii_path_is_fast_forwarded() {
    let fx = Fixture::new();
    in_head(&fx, &["é.txt"]);
    let tip = fx.push("é.txt", "two\n");
    write(&fx.host.join("é.txt"), "two\n");

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "");
}

#[test]
fn identical_dirt_non_ascii_path_that_differs_stays_dirty() {
    let fx = Fixture::new();
    in_head(&fx, &["é.txt"]);
    fx.push("é.txt", "two\n");
    write(&fx.host.join("é.txt"), "mine\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "é.txt"), "mine\n");
}

#[test]
fn identical_dirt_glob_path_does_not_vouch_for_its_lookalike() {
    let fx = Fixture::new();
    in_head(&fx, &["a[1].txt", "a1.txt"]);
    fx.push("a[1].txt", "two\n");
    write(&fx.host.join("a[1].txt"), "two\n");
    write(&fx.host.join("a1.txt"), "mine\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "a1.txt"), "mine\n", "a1.txt is untouched");
    assert_eq!(read(&fx, "a[1].txt"), "two\n");
}

#[test]
fn identical_dirt_path_with_spaces_is_fast_forwarded() {
    let fx = Fixture::new();
    in_head(&fx, &["with space.txt"]);
    let tip = fx.push("with space.txt", "two\n");
    write(&fx.host.join("with space.txt"), "two\n");

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(read(&fx, "with space.txt"), "two\n");
}

#[test]
fn identical_dirt_rename_stays_dirty() {
    // The new name is not in HEAD, so `checkout HEAD --` could not restore
    // it: never vouched for.
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    git(&fx.host, &["mv", "README.md", "moved.md"]);

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "moved.md"), "one\n");
    assert!(!fx.host.join("README.md").exists());
}

// ── The filesystem behind a vouched-for path (#11558 P1) ────────────────

/// Someone else deletes `path` upstream.
fn delete_upstream(fx: &Fixture, path: &str) -> String {
    git(&fx.seed, &["pull", "--quiet", "--ff-only", "origin", fx.branch]);
    git(&fx.seed, &["rm", "--quiet", "--", path]);
    git(&fx.seed, &["commit", "--quiet", "-m", &format!("delete {path}")]);
    git(&fx.seed, &["push", "--quiet", "origin", &format!("HEAD:{}", fx.branch)]);
    fx.origin_tip()
}

#[test]
fn identical_dirt_deleted_path_now_a_directory_stays_dirty() {
    // Upstream deletes `victim`; locally `victim` is now a directory of
    // untracked and ignored files. Git's diff calls it "deleted" (identical
    // to origin), but `checkout HEAD -- victim` would remove the directory.
    let fx = Fixture::new();
    in_head(&fx, &["victim"]);
    delete_upstream(&fx, "victim");
    fs::remove_file(fx.host.join("victim")).unwrap();
    write(&fx.host.join("victim/local-only.txt"), "precious\n");
    write(&fx.host.join("victim/build.log"), "ignored\n");
    write(&fx.host.join(".git/info/exclude"), "*.log\n");
    let index = git(&fx.host, &["ls-files", "-s"]);

    // `assert_skipped` also pins HEAD, the index bytes and every file.
    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "victim/local-only.txt"), "precious\n");
    assert_eq!(read(&fx, "victim/build.log"), "ignored\n");
    assert_eq!(git(&fx.host, &["ls-files", "-s"]), index);
}

#[test]
fn identical_dirt_untracked_file_on_an_unstaged_deletion_stays_dirty() {
    // `git rm --cached victim` + a new untracked `victim`: invisible to
    // `git diff <ref>`, and the discard would overwrite it.
    let fx = Fixture::new();
    in_head(&fx, &["victim"]);
    delete_upstream(&fx, "victim");
    git(&fx.host, &["rm", "--quiet", "--cached", "--", "victim"]);
    write(&fx.host.join("victim"), "precious\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "victim"), "precious\n");
}

#[cfg(unix)]
#[test]
fn identical_dirt_file_to_symlink_type_change_stays_dirty() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("elsewhere.txt"), "two\n");
    fs::remove_file(fx.host.join("README.md")).unwrap();
    std::os::unix::fs::symlink("elsewhere.txt", fx.host.join("README.md")).unwrap();

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    let meta = fs::symlink_metadata(fx.host.join("README.md")).unwrap();
    assert!(meta.file_type().is_symlink(), "the symlink is left alone");
}

#[test]
fn identical_dirt_plain_deletion_is_fast_forwarded() {
    // The path is truly absent locally and upstream deletes it too.
    let fx = Fixture::new();
    in_head(&fx, &["victim"]);
    let tip = delete_upstream(&fx, "victim");
    fs::remove_file(fx.host.join("victim")).unwrap();

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "");
    assert!(!fx.host.join("victim").exists());
}

// ── Raw bytes, never the filtered view (#11558 third pass) ──────────────

/// A lossy clean filter `strip` (drops `OUTPUT` lines) on `path`, set in the
/// host's own config and `.git/info/attributes`.
fn lossy_filter(fx: &Fixture, path: &str) {
    git(&fx.host, &["config", "filter.strip.clean", "grep -v '^OUTPUT' || true"]);
    git(&fx.host, &["config", "filter.strip.smudge", "cat"]);
    write(&fx.host.join(".git/info/attributes"), &format!("{path} filter=strip\n"));
}

#[test]
fn identical_dirt_lossy_clean_filter_stays_dirty() {
    // The filtered view equals origin; the bytes on disk do not. The discard
    // would delete "OUTPUT precious".
    let fx = Fixture::new();
    in_head(&fx, &["f.nb"]);
    fx.push("f.nb", "two\n");
    lossy_filter(&fx, "f.nb");
    write(&fx.host.join("f.nb"), "two\nOUTPUT precious\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "f.nb"), "two\nOUTPUT precious\n");
}

#[test]
fn identical_dirt_raw_identical_under_a_filter_is_fast_forwarded() {
    let fx = Fixture::new();
    in_head(&fx, &["f.nb"]);
    let tip = fx.push("f.nb", "two\n");
    lossy_filter(&fx, "f.nb");
    write(&fx.host.join("f.nb"), "two\n");

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(read(&fx, "f.nb"), "two\n");
}

#[test]
fn identical_dirt_crlf_only_difference_stays_dirty() {
    let fx = Fixture::new();
    in_head(&fx, &["t.txt"]);
    fx.push("t.txt", "two\n");
    write(&fx.host.join(".git/info/attributes"), "t.txt text=auto\n");
    write(&fx.host.join("t.txt"), "two\r\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(fs::read(fx.host.join("t.txt")).unwrap(), b"two\r\n");
}
