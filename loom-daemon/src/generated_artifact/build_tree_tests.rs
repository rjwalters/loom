//! Tests for name-independent build-tree discovery and proof (#11193).
//! [`cargo_tree`] and friends are shared with the reaper's tests.

use super::*;
use std::os::unix::fs::symlink;

pub(crate) const SIG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";
pub(crate) const RUSTC_INFO: &str = r#"{"rustc_fingerprint":123,"outputs":{}}"#;

pub(crate) fn write(path: &Path, body: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A realistic cargo target root at `dir`: both markers, a `debug/` profile
/// with cargo's own subdirectories, an executable, a dep-info file and a
/// `.cargo-lock`, a target-triple `release/` profile, and `doc/`.
pub(crate) fn cargo_tree(dir: &Path) {
    write(&dir.join("CACHEDIR.TAG"), SIG.as_bytes());
    write(&dir.join(".rustc_info.json"), RUSTC_INFO.as_bytes());
    write(&dir.join("debug/.fingerprint/x-1/lib-x"), b"fp");
    write(&dir.join("debug/deps/libx-1.rlib"), &[7u8; 4096]);
    write(&dir.join("debug/build/x-1/out/generated.rs"), b"pub const X: u8 = 1;");
    write(&dir.join("debug/incremental/x-1/s-1/dep-graph.bin"), b"g");
    write(&dir.join("debug/.cargo-lock"), b"");
    write(&dir.join("debug/x.d"), b"x: src/main.rs");
    write(&dir.join("debug/x"), b"\x7fELF");
    fs::set_permissions(dir.join("debug/x"), fs::Permissions::from_mode(0o755)).unwrap();
    write(&dir.join("x86_64-unknown-linux-gnu/release/.fingerprint/y-1/f"), b"fp");
    write(&dir.join("x86_64-unknown-linux-gnu/release/deps/liby-1.rmeta"), b"m");
    write(&dir.join("doc/x/index.html"), b"<html>");
}

/// A tagged directory carrying user-authored source beside cargo output.
pub(crate) fn mixed_tree(dir: &Path) {
    cargo_tree(dir);
    write(&dir.join("notes.rs"), b"fn hand_written() {}\n");
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

pub(crate) fn seeded_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
    write(&dir.join("tracked.txt"), b"x");
    git(dir, &["add", "tracked.txt"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn rels(roots: &[BuildRoot]) -> Vec<(&str, bool)> {
    roots.iter().map(|r| (r.rel.as_str(), r.symlink)).collect()
}

#[test]
fn markers_require_the_full_signature() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, body, want) in [
        ("valid", SIG, true),
        ("invalid", "Signature: not-cargo\n", false),
        ("truncated", "Signature: 8a477f597d28", false),
        ("empty", "", false),
    ] {
        write(&tmp.path().join(name).join("CACHEDIR.TAG"), body.as_bytes());
        assert_eq!(has_regular_marker(&tmp.path().join(name)), want, "{name}");
    }
}

#[test]
fn discovery_finds_nested_roots_at_any_name_and_never_descends_into_them() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path();
    cargo_tree(&wt.join(".loom/target-doctor-1"));
    cargo_tree(&wt.join(".loom-target"));
    cargo_tree(&wt.join("target-wt"));
    cargo_tree(&wt.join(".cargo-target-doctor-2"));
    // A nested marker inside a found root is not a second root.
    write(&wt.join(".loom-target/debug/build/inner/CACHEDIR.TAG"), SIG.as_bytes());
    // Untagged or badly tagged look-alikes are not roots.
    write(&wt.join("target-plain/debug/x"), b"x");
    write(&wt.join("target-bad/CACHEDIR.TAG"), b"Signature: 8a47");
    // Too deep to be discovered.
    cargo_tree(&wt.join("a/b/c/deep-target"));
    // A worktree-root marker never makes the worktree a root.
    write(&wt.join("CACHEDIR.TAG"), SIG.as_bytes());

    assert_eq!(
        rels(&discover_build_roots(wt)),
        vec![
            (".cargo-target-doctor-2", false),
            (".loom-target", false),
            (".loom/target-doctor-1", false),
            ("target-wt", false),
        ]
    );
}

#[test]
fn discovery_reports_symlinked_roots_without_walking_them() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    cargo_tree(&outside.join("tree"));
    let wt = tmp.path().join("wt");
    fs::create_dir_all(wt.join(".loom")).unwrap();
    symlink(outside.join("tree"), wt.join(".loom-target")).unwrap();
    // A symlinked parent is never descended into.
    symlink(&outside, wt.join("linked-parent")).unwrap();
    // A marker that is itself a symlink escaping the worktree proves nothing.
    write(&wt.join("escaping/debug/x.d"), b"d");
    symlink(outside.join("tree/CACHEDIR.TAG"), wt.join("escaping/CACHEDIR.TAG")).unwrap();

    assert_eq!(rels(&discover_build_roots(&wt)), vec![(".loom-target", true)]);
    assert!(prove_build_tree(&wt.join(".loom-target"))
        .unwrap_err()
        .contains("symlink"));
}

#[test]
fn proof_accepts_only_cargo_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let ok = tmp.path().join("ok");
    cargo_tree(&ok);
    assert_eq!(prove_build_tree(&ok), Ok(()));

    let mixed = tmp.path().join("mixed");
    mixed_tree(&mixed);
    assert!(prove_build_tree(&mixed).unwrap_err().contains("notes.rs"));

    let src = tmp.path().join("src-dir");
    cargo_tree(&src);
    write(&src.join("src/lib.rs"), b"pub fn f() {}");
    assert!(prove_build_tree(&src).unwrap_err().contains("`src`"));

    // A non-executable, extensionless file in a profile dir is not a binary.
    let note = tmp.path().join("note");
    cargo_tree(&note);
    write(&note.join("debug/TODO"), b"remember");
    assert!(prove_build_tree(&note).unwrap_err().contains("`debug`"));

    let linked = tmp.path().join("linked");
    cargo_tree(&linked);
    symlink(tmp.path(), linked.join("escape")).unwrap();
    assert!(prove_build_tree(&linked).unwrap_err().contains("`escape`"));

    let unmarked = tmp.path().join("unmarked");
    cargo_tree(&unmarked);
    fs::remove_file(unmarked.join("CACHEDIR.TAG")).unwrap();
    fs::write(unmarked.join(".rustc_info.json"), b"{}").unwrap();
    assert!(prove_build_tree(&unmarked).unwrap_err().contains("marker"));

    // `.rustc_info.json` alone suffices as the marker (#9748 shape).
    let info_only = tmp.path().join("info-only");
    cargo_tree(&info_only);
    fs::remove_file(info_only.join("CACHEDIR.TAG")).unwrap();
    assert_eq!(prove_build_tree(&info_only), Ok(()));
}

#[test]
fn proven_untracked_build_output_is_not_user_work() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path();
    seeded_repo(wt);
    write(&wt.join(".loom-managed"), b"");
    cargo_tree(&wt.join(".loom/target-doctor-1"));
    cargo_tree(&wt.join(".loom-target"));
    assert!(!has_untracked_user_work(wt));
}

#[test]
fn anything_beyond_proven_build_output_is_user_work() {
    type Setup = fn(&Path);
    let cases: [(&str, Setup); 5] = [
        ("ordinary untracked .rs", |wt| write(&wt.join("scratch.rs"), b"fn f() {}")),
        ("tagged dir with user source", |wt| mixed_tree(&wt.join(".loom-target"))),
        ("truncated signature", |wt| {
            write(&wt.join("t/CACHEDIR.TAG"), b"Signature: 8a477f");
            write(&wt.join("t/debug/x.d"), b"d");
        }),
        ("invalid signature", |wt| {
            write(&wt.join("t/CACHEDIR.TAG"), b"Signature: 0000\n");
            write(&wt.join("t/debug/x.d"), b"d");
        }),
        ("tagged dir beside an untagged user dir", |wt| {
            write(&wt.join("notes/plan.md"), b"plan");
        }),
    ];
    for (name, setup) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        seeded_repo(wt);
        cargo_tree(&wt.join(".loom/target-doctor-1"));
        setup(wt);
        assert!(has_untracked_user_work(wt), "{name}");
    }
}

#[test]
fn a_root_git_tracks_is_not_build_output() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path();
    seeded_repo(wt);
    write(&wt.join("vendored-target/CACHEDIR.TAG"), SIG.as_bytes());
    git(wt, &["add", "vendored-target/CACHEDIR.TAG"]);
    git(wt, &["commit", "-q", "-m", "vendor"]);
    cargo_tree(&wt.join("vendored-target"));
    assert_eq!(tracks_files_under(wt, "vendored-target"), Some(true));
    assert_eq!(tracks_files_under(wt, "vendored"), Some(false));
    assert!(has_untracked_user_work(wt));
}

#[test]
fn a_non_git_directory_reports_no_user_work() {
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("loose.rs"), b"x");
    assert!(!has_untracked_user_work(tmp.path()));
    assert_eq!(tracks_files_under(tmp.path(), "x"), None);
}
