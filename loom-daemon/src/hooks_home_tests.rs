#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! 2am#4501: the sessions a daemon spawns run the hooks of its own release.

use super::*;
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
fn resolve_without_a_home_is_none() {
    assert_eq!(resolve(None, None, &sample(), "1.2.3"), None);
}

/// The real embedded payload carries what the wired hooks execute and
/// source: if a hook moved or a library it sources fell out of the payload,
/// the session would fall back to a stale checkout without anyone noticing.
#[test]
fn the_embedded_payload_carries_the_wired_hooks_and_their_libraries() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = crate::init::payload::embedded_archive();
    let dir = materialize(tmp.path(), archive, env!("CARGO_PKG_VERSION")).unwrap();
    for rel in [
        "defaults/hooks/guard-destructive.sh",
        "defaults/hooks/guard-destructive-generic.sh",
        "defaults/hooks/guard-loom-workflow.sh",
        "defaults/hooks/guard-worktree-paths.sh",
        "defaults/hooks/guard-mcp-tools.sh",
        "defaults/hooks/guard-uncommitted-work.sh",
        "defaults/hooks/roll-pause.sh",
        "defaults/hooks/gh-front-env.sh",
        "defaults/hooks/hook-wiring.sh",
        "defaults/scripts/lib/locate-daemon-bin.sh",
        "defaults/scripts/lib/config-resolver.sh",
    ] {
        assert!(dir.join(rel).is_file(), "{rel} missing from the hooks home");
    }
    assert!(!dir.join("defaults/scripts/tests").exists());
}

#[test]
fn agent_env_names_the_variable_the_wrapper_reads() {
    let env = agent_env();
    assert_eq!(env.len(), 1);
    assert_eq!(env[0].0, "LOOM_HOOKS_HOME");
}
