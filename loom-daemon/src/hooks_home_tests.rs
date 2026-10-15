#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! 2am#4501: the sessions a daemon spawns run the hooks of its own release.

use super::*;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

/// A `.tar.zst` of `(path, body, mode)` entries, packed the way `build.rs`
/// packs the real payload.
fn archive(files: &[(&str, &str, u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, body, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(*mode);
        header.set_size(body.len() as u64);
        builder
            .append_data(&mut header, path, body.as_bytes())
            .unwrap();
    }
    zstd::encode_all(builder.into_inner().unwrap().as_slice(), 3).unwrap()
}

fn sample() -> Vec<u8> {
    archive(&[
        ("defaults/hooks/guard-destructive.sh", "#!/bin/bash\nexit 0\n", 0o755),
        ("defaults/hooks/tests/test-guard.sh", "#!/bin/bash\n", 0o755),
        ("defaults/scripts/lib/locate-daemon-bin.sh", "# lib\n", 0o644),
        ("defaults/scripts/tests/test-x.sh", "#!/bin/bash\n", 0o755),
        ("defaults/roles/builder.md", "role\n", 0o644),
    ])
}

#[test]
fn unpacks_the_hook_runtime_and_skips_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let dir = materialize(&base, &sample(), "1.2.3").unwrap();

    assert_eq!(dir, base.join(key(&sample(), "1.2.3")));
    assert!(dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("1.2.3-"));
    let hook = dir.join("defaults/hooks/guard-destructive.sh");
    let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "hooks keep their executable bit");
    assert!(dir
        .join("defaults/scripts/lib/locate-daemon-bin.sh")
        .is_file());
    assert!(dir.join("defaults/roles/builder.md").is_file());
    assert!(!dir.join("defaults/hooks/tests").exists());
    assert!(!dir.join("defaults/scripts/tests").exists());
    assert!(dir.join(COMPLETE_MARKER).is_file());
}

#[test]
fn a_complete_copy_is_reused_not_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let dir = materialize(&base, &sample(), "1.2.3").unwrap();
    let hook = dir.join("defaults/hooks/guard-destructive.sh");
    std::fs::write(&hook, "sentinel").unwrap();

    assert_eq!(materialize(&base, &sample(), "1.2.3").unwrap(), dir);
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), "sentinel");
}

#[test]
fn a_crashed_unpack_under_the_same_key_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let partial = base.join(key(&sample(), "1.2.3"));
    std::fs::create_dir_all(partial.join("defaults/hooks")).unwrap();

    let dir = materialize(&base, &sample(), "1.2.3").unwrap();
    assert_eq!(dir, partial);
    assert!(dir.join("defaults/hooks/guard-destructive.sh").is_file());
    assert!(dir.join(COMPLETE_MARKER).is_file());
}

#[test]
fn different_files_get_a_different_directory() {
    let other = archive(&[("defaults/hooks/guard-destructive.sh", "#!/bin/bash\nexit 1\n", 0o755)]);
    assert_ne!(key(&sample(), "1.2.3"), key(&other, "1.2.3"));
    assert_eq!(key(&sample(), "1.2.3"), key(&sample(), "1.2.3"));
}

#[test]
fn a_payload_without_hooks_is_refused_and_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let bad = archive(&[("defaults/roles/builder.md", "role\n", 0o644)]);
    assert!(materialize(&base, &bad, "1.2.3").is_err());
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
}

#[test]
fn entries_outside_defaults_are_not_unpacked() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let mixed = archive(&[
        ("defaults/hooks/a.sh", "#!/bin/bash\n", 0o755),
        ("elsewhere/b.sh", "#!/bin/bash\n", 0o755),
    ]);
    let dir = materialize(&base, &mixed, "1.2.3").unwrap();
    assert!(!dir.join("elsewhere").exists());
}

#[test]
fn prune_keeps_the_current_and_recent_copies_and_young_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let current = base.join("9.9.9-current");
    std::fs::create_dir_all(&current).unwrap();
    for i in 0..5 {
        std::fs::create_dir_all(base.join(format!("1.0.{i}-old"))).unwrap();
    }
    std::fs::create_dir_all(base.join(".staging-crashed")).unwrap();

    // Nothing is older than RETAIN yet: nothing goes.
    prune(base, &current, SystemTime::now());
    assert_eq!(std::fs::read_dir(base).unwrap().count(), 7);

    // A week later, all but the KEEP_RECENT newest (the current one among
    // them) go, and so does the stale staging directory.
    prune(base, &current, SystemTime::now() + Duration::from_secs(7 * 24 * 3600));
    let left: Vec<_> = std::fs::read_dir(base)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left.len(), KEEP_RECENT, "left: {left:?}");
    assert!(current.is_dir());
    assert!(!base.join(".staging-crashed").exists());
}

#[test]
fn resolve_honours_a_pin_and_an_empty_opt_out() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve(Some("/pinned".into()), Some(tmp.path().into()), &sample(), "1.2.3"),
        Some(PathBuf::from("/pinned"))
    );
    assert_eq!(resolve(Some(String::new()), Some(tmp.path().into()), &sample(), "1.2.3"), None);
    // Neither the pin nor the opt-out writes anything.
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);

    let dir = resolve(None, Some(tmp.path().into()), &sample(), "1.2.3").unwrap();
    assert!(dir.starts_with(tmp.path().join(".local/share/loom-daemon/hooks-home")));
    assert!(dir.join("defaults/hooks/guard-destructive.sh").is_file());
}

#[test]
fn resolve_warns_whether_a_pin_would_pass_the_trust_check() {
    let tmp = tempfile::tempdir().unwrap();
    let trusted = tmp.path().join("trusted");
    std::fs::create_dir(&trusted).unwrap();
    std::fs::set_permissions(&trusted, std::fs::Permissions::from_mode(0o700)).unwrap();
    let shared = tmp.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();

    for (pin, ok) in [
        (&trusted, true),
        (&shared, false),
        (&tmp.path().join("missing"), false),
    ] {
        assert_eq!(is_trusted(pin), ok);
        let msg = pin_warning(pin, is_trusted(pin));
        assert!(msg.contains(&pin.display().to_string()));
        assert_eq!(msg.contains("would NOT pass"), !ok, "{msg}");
        // The pin is still passed through unchanged.
        let got = resolve(Some(pin.display().to_string()), None, &sample(), "1.2.3");
        assert_eq!(got.as_deref(), Some(pin.as_path()));
    }
}

#[test]
fn resolve_without_a_home_is_none() {
    assert_eq!(resolve(None, None, &sample(), "1.2.3"), None);
}

/// Every mode bit `walk` sees under `root` that grants group/other write.
fn shared_write(root: &Path) -> Vec<String> {
    let mut bad = Vec::new();
    walk(root, &mut |path, meta| {
        if !meta.file_type().is_symlink() && meta.permissions().mode() & 0o022 != 0 {
            bad.push(format!("{} {:o}", path.display(), meta.permissions().mode()));
        }
        true
    });
    bad
}

/// The source tree this crate was built from.
fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The hooks `provision_loom_hooks` wires: its `_PHOOK_NAMES` array, plus
/// `presence-beat.sh`, which `_phook_presence_apply` wires separately.
fn wired_hook_names() -> BTreeSet<String> {
    let text = std::fs::read_to_string(source_root().join("scripts/install/provision-hooks.sh"))
        .expect("read provision-hooks.sh");
    let block = text
        .split("_PHOOK_NAMES=(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("provision-hooks.sh has a _PHOOK_NAMES=( ... ) array");
    let mut names: BTreeSet<String> = block
        .split_whitespace()
        .filter(|w| !w.starts_with('#'))
        .map(str::to_string)
        .collect();
    names.insert("presence-beat.sh".to_string());
    names
}

/// The `scripts/lib/*.sh` files the shipped hooks reach, transitively: any
/// library a hook (or an already-reached library) names. Matching is by
/// file name, so it errs toward asserting more, never less.
fn reached_libraries() -> BTreeSet<String> {
    let lib_dir = source_root().join("defaults/scripts/lib");
    let libs: Vec<String> = std::fs::read_dir(&lib_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sh"))
        .collect();
    let mut todo: Vec<PathBuf> = std::fs::read_dir(source_root().join("defaults/hooks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "sh"))
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(file) = todo.pop() {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for lib in &libs {
            if text.contains(lib.as_str()) && seen.insert(lib.clone()) {
                todo.push(lib_dir.join(lib));
            }
        }
    }
    seen
}

/// The real embedded payload carries every hook `provision_loom_hooks` wires
/// and every library they source. If one moved or fell out of the payload,
/// sessions would quietly fall back to a stale checkout, so the lists are
/// derived from the installer and the hooks themselves, never hand-kept.
#[test]
fn the_embedded_payload_carries_the_wired_hooks_and_their_libraries() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = crate::init::payload::embedded_archive();
    let dir = materialize(&tmp.path().join("hh"), archive, env!("CARGO_PKG_VERSION")).unwrap();

    let wired = wired_hook_names();
    assert!(wired.len() >= 11, "too few wired hooks parsed: {wired:?}");
    for name in &wired {
        let path = dir.join("defaults/hooks").join(name);
        let mode = std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("wired hook {name} missing from the hooks home: {e}"))
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "wired hook {name} is not executable");
    }
    // Every hook script the source tree ships, wired or exec'd by one.
    for entry in std::fs::read_dir(source_root().join("defaults/hooks")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|x| x == "sh") {
            let name = path.file_name().unwrap();
            assert!(
                dir.join("defaults/hooks").join(name).is_file(),
                "{} missing from the hooks home",
                name.to_string_lossy()
            );
        }
    }
    let libs = reached_libraries();
    for lib in [
        "locate-daemon-bin.sh",
        "config-resolver.sh",
        "canonical-path.sh",
        "installed-file-guard.sh",
        "default-branch.sh",
        "worktree-root.sh",
    ] {
        assert!(libs.contains(lib), "{lib} not found by the derivation: {libs:?}");
    }
    for lib in &libs {
        assert!(
            dir.join("defaults/scripts/lib").join(lib).is_file(),
            "library {lib} missing from the hooks home"
        );
    }
    assert!(!dir.join("defaults/scripts/tests").exists());
    assert_eq!(shared_write(&dir), Vec::<String>::new());
}

const UMASK_CHILD_ENV: &str = "LOOM_HOOKS_HOME_TEST_UMASK_BASE";

/// Run by [`a_permissive_umask_leaves_no_shared_write_bit`] in its own
/// process, because the umask is process-wide.
#[test]
#[ignore = "run in a child process by a_permissive_umask_leaves_no_shared_write_bit"]
fn umask_child() {
    let Ok(base) = std::env::var(UMASK_CHILD_ENV) else {
        return;
    };
    // SAFETY: umask only sets this (child) process's file-creation mask.
    unsafe {
        libc::umask(0);
    }
    let loose = archive(&[
        ("defaults/hooks/guard-destructive.sh", "#!/bin/bash\n", 0o777),
        ("defaults/hooks/nested/deeper/x.sh", "#!/bin/bash\n", 0o775),
        ("defaults/scripts/lib/a.sh", "# lib\n", 0o666),
    ]);
    materialize(Path::new(&base), &loose, "1.2.3").unwrap();
}

#[test]
fn a_permissive_umask_leaves_no_shared_write_bit() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("share/hooks-home");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hooks_home::tests::umask_child",
            "--ignored",
            "--test-threads=1",
        ])
        .env(UMASK_CHILD_ENV, &base)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the umask-0 child failed: {status}");
    let copies: Vec<PathBuf> = std::fs::read_dir(&base)
        .expect("the child unpacked under the base")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(std::fs::metadata(&base).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&copies[0]).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(copies[0]
        .join("defaults/hooks/nested/deeper/x.sh")
        .is_file());
    assert_eq!(shared_write(&tmp.path().join("share")), Vec::<String>::new());
    assert!(is_trusted(&copies[0]));
}

#[test]
fn a_group_writable_copy_is_unpacked_again_not_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let dir = materialize(&base, &sample(), "1.2.3").unwrap();
    let hooks = dir.join("defaults/hooks");
    std::fs::write(hooks.join("guard-destructive.sh"), "planted").unwrap();
    std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(!is_trusted(&dir));

    assert_eq!(materialize(&base, &sample(), "1.2.3").unwrap(), dir);
    assert_eq!(
        std::fs::read_to_string(hooks.join("guard-destructive.sh")).unwrap(),
        "#!/bin/bash\nexit 0\n",
        "the planted hook was replaced by the payload's"
    );
    assert!(is_trusted(&dir));
}

#[test]
fn a_symlink_under_the_key_is_replaced_by_a_real_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("defaults/hooks")).unwrap();
    std::fs::write(elsewhere.join(COMPLETE_MARKER), "x").unwrap();
    std::fs::create_dir_all(&base).unwrap();
    let dest = base.join(key(&sample(), "1.2.3"));
    std::os::unix::fs::symlink(&elsewhere, &dest).unwrap();
    assert!(!is_trusted(&dest));

    let dir = materialize(&base, &sample(), "1.2.3").unwrap();
    assert!(!dir.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(dir.join("defaults/hooks/guard-destructive.sh").is_file());
    assert!(elsewhere.join("defaults/hooks").is_dir(), "the link target is left alone");
}

#[test]
fn a_loose_base_is_tightened_to_0700() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o777)).unwrap();
    materialize(&base, &sample(), "1.2.3").unwrap();
    assert_eq!(std::fs::metadata(&base).unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
fn reuse_refreshes_the_copy_so_pruning_keys_on_last_use() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hooks-home");
    let dir = materialize(&base, &sample(), "1.2.3").unwrap();
    let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
    std::fs::File::open(&dir)
        .unwrap()
        .set_modified(old)
        .unwrap();

    materialize(&base, &sample(), "1.2.3").unwrap();
    let age = SystemTime::now()
        .duration_since(std::fs::metadata(&dir).unwrap().modified().unwrap())
        .unwrap_or_default();
    assert!(age < Duration::from_secs(3600), "mtime not refreshed: {age:?} old");
}

#[test]
fn agent_env_names_the_variable_the_wrapper_reads() {
    let env = agent_env();
    assert_eq!(env.len(), 1);
    assert_eq!(env[0].0, "LOOM_HOOKS_HOME");
}
