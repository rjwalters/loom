//! Tests for the resync payload (#10717): materialize, diff and apply, on
//! temp workspaces only. Most use a small synthetic `defaults/` tree so each
//! case states its own payload; the embedded-payload tests at the bottom run
//! the real one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serial_test::serial;
use tempfile::TempDir;

use super::payload::{
    apply, gate_metadata, materialize_payload, materialize_with, resync_gate,
    resync_workspace_with, Payload, ResyncOutcome, ResyncRefusal, Stamp,
};
use super::{install_payload_files, InitReport};
use crate::install_compat::{Compat, DaemonCompat, InstallMeta, Version, INSTALL_METADATA_PATH};

const META: &str = INSTALL_METADATA_PATH;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn stamp(version: &str) -> Stamp {
    Stamp {
        version: v(version),
        commit: Some("a".repeat(40)),
        requires_daemon: "0.19.772".to_string(),
        release_build: true,
    }
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[cfg(unix)]
fn set_exec(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn set_exec(_path: &Path) {}

/// A small `defaults/` tree touching every surface the payload step writes.
fn fake_defaults(root: &Path) -> PathBuf {
    let d = root.join("defaults");
    write(&d.join(".loom-README.md"), "readme\n");
    write(&d.join("pricing.json"), "{}\n");
    write(&d.join("roles/builder.md"), "builder\n");
    write(&d.join("scripts/a.sh"), "#!/bin/sh\necho a\n");
    write(&d.join("scripts/lib/b.sh"), "#!/bin/sh\necho b\n");
    set_exec(&d.join("scripts/a.sh"));
    write(&d.join("hooks/h.sh"), "#!/bin/sh\n");
    write(&d.join("docs/d.md"), "doc\n");
    write(&d.join("runtimes/r.json"), "{}\n");
    write(&d.join(".loom/bin/loom"), "#!/bin/sh\n");
    write(&d.join(".loom/biome.jsonc"), "{}\n");
    // Scaffolded (template-substituted), never part of the payload diff.
    write(&d.join(".loom/CLAUDE.md"), "installed {{INSTALL_DATE}}\n");
    write(&d.join(".claude/commands/loom/builder.md"), "cmd\n");
    write(&d.join(".claude/commands/loom/internal.md"), "internal\n");
    write(&d.join(".loom-internal.list"), ".claude/commands/loom/internal.md\n");
    d
}

/// A workspace with Loom installed from `defaults` at version `installed`.
fn installed_workspace(root: &Path, defaults: &Path, installed: &str) -> PathBuf {
    let ws = root.join("ws");
    fs::create_dir_all(ws.join(".git")).unwrap();
    write(
        &ws.join(META),
        &format!(
            "{{\n  \"loom_version\": \"{installed}\",\n  \"loom_commit\": \"old\",\n  \
             \"install_date\": \"2026-01-01\",\n  \"installed_files\": [\".loom/scripts/lib/b.sh\"]\n}}\n"
        ),
    );
    let mut report = InitReport::default();
    install_payload_files(&ws, defaults, &ws.join(".loom"), true, &mut report).unwrap();
    write(&ws.join(".claude/commands/loom/builder.md"), "cmd\n");
    // A repo-owned file inside a managed dir: no ownership evidence, kept.
    write(&ws.join(".loom/hooks/post-worktree.sh"), "#!/bin/sh\nrepo\n");
    // Outside the payload surface entirely.
    write(&ws.join(".loom/config.json"), "{\"repo\": true}\n");
    ws
}

/// Every file under `root` with its bytes and mtime, after pinning all
/// mtimes to a fixed past instant so any write is visible.
fn freeze(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let mut out = BTreeMap::new();
    walk(root, &mut |p| {
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(past).unwrap();
    });
    walk(root, &mut |p| {
        out.insert(
            p.to_path_buf(),
            (fs::read(p).unwrap(), fs::metadata(p).unwrap().modified().unwrap()),
        );
    });
    out
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    walk(root, &mut |p| {
        out.insert(
            p.to_path_buf(),
            (fs::read(p).unwrap(), fs::metadata(p).unwrap().modified().unwrap()),
        );
    });
    out
}

fn walk(root: &Path, f: &mut dyn FnMut(&Path)) {
    for entry in fs::read_dir(root).unwrap().flatten() {
        let ft = entry.file_type().unwrap();
        if ft.is_dir() {
            walk(&entry.path(), f);
        } else if ft.is_file() {
            f(&entry.path());
        }
    }
}

/// Repo-relative paths whose bytes or mtime differ, plus created/deleted ones.
fn touched(
    ws: &Path,
    before: &BTreeMap<PathBuf, (Vec<u8>, SystemTime)>,
    after: &BTreeMap<PathBuf, (Vec<u8>, SystemTime)>,
) -> Vec<String> {
    let mut out: Vec<String> = before
        .keys()
        .chain(after.keys())
        .filter(|p| before.get(*p) != after.get(*p))
        .map(|p| p.strip_prefix(ws).unwrap().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn meta_json(ws: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(ws.join(META)).unwrap()).unwrap()
}

#[test]
fn identical_payload_is_an_empty_diff_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let before = freeze(&ws);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(diff.is_empty(), "{diff:?}");
    assert_eq!(apply(&ws, &diff).unwrap(), ResyncOutcome::Unchanged);
    assert_eq!(resync_workspace_with(&payload, &ws).unwrap(), ResyncOutcome::Unchanged);

    let after = snapshot(&ws);
    assert_eq!(touched(&ws, &before, &after), Vec::<String>::new());
    // In particular no version bump, though the payload is newer.
    assert_eq!(meta_json(&ws)["loom_version"], "0.19.880");
}

#[test]
fn changed_file_writes_exactly_it_plus_metadata() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    write(&defaults.join("scripts/a.sh"), "#!/bin/sh\necho a2\n");
    set_exec(&defaults.join("scripts/a.sh"));
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let before = freeze(&ws);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert_eq!(diff.changed, vec![".loom/scripts/a.sh"]);
    assert!(diff.added.is_empty() && diff.removed.is_empty(), "{diff:?}");

    let outcome = apply(&ws, &diff).unwrap();
    let expect = vec![".loom/scripts/a.sh".to_string(), META.to_string()];
    assert_eq!(
        outcome,
        ResyncOutcome::Applied {
            written: expect.clone()
        }
    );
    let mut sorted = expect;
    sorted.sort();
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), sorted);
    assert_eq!(
        fs::read_to_string(ws.join(".loom/scripts/a.sh")).unwrap(),
        "#!/bin/sh\necho a2\n"
    );

    let meta = meta_json(&ws);
    assert_eq!(meta["loom_version"], "0.19.881");
    assert_eq!(meta["loom_commit"], "a".repeat(40));
    assert_eq!(meta["requires_daemon"], "0.19.772");
    assert!(meta["last_resync"].is_string());
    // Every other key is kept.
    assert_eq!(meta["install_date"], "2026-01-01");
    assert_eq!(meta["installed_files"][0], ".loom/scripts/lib/b.sh");
}

#[cfg(unix)]
#[test]
fn executable_bit_alone_is_a_change() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    let installed = ws.join(".loom/roles/builder.md");
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let diff = materialize_with(&payload, &ws).unwrap();
    assert_eq!(diff.changed, vec![".loom/roles/builder.md"]);
    apply(&ws, &diff).unwrap();
    assert_eq!(fs::metadata(&installed).unwrap().permissions().mode() & 0o111, 0);
}

#[test]
fn new_file_is_added_and_written() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    write(&defaults.join("scripts/new/c.sh"), "#!/bin/sh\nnew\n");
    write(&defaults.join(".claude/commands/loom/new.md"), "new cmd\n");
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let before = freeze(&ws);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert_eq!(diff.added, vec![".claude/commands/loom/new.md", ".loom/scripts/new/c.sh"]);
    assert!(diff.changed.is_empty() && diff.removed.is_empty(), "{diff:?}");

    apply(&ws, &diff).unwrap();
    assert_eq!(
        touched(&ws, &before, &snapshot(&ws)),
        vec![
            ".claude/commands/loom/new.md",
            META,
            ".loom/scripts/new/c.sh"
        ]
    );
    // The installer's own rule: shell scripts are executable.
    assert!(is_exec(&ws.join(".loom/scripts/new/c.sh")));
    // Both are now recorded as Loom's, after the entry that was there.
    assert_eq!(
        meta_json(&ws)["installed_files"],
        serde_json::json!([
            ".loom/scripts/lib/b.sh",
            ".claude/commands/loom/new.md",
            ".loom/scripts/new/c.sh"
        ])
    );
}

/// A file a resync adds is one a later resync can retire (#10878 review).
#[test]
fn file_added_by_a_resync_is_retired_by_a_later_one() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    write(&defaults.join("scripts/new/c.sh"), "#!/bin/sh\nnew\n");
    let payload = Payload::from_defaults(defaults.clone(), stamp("0.19.881"));
    assert!(matches!(
        resync_workspace_with(&payload, &ws).unwrap(),
        ResyncOutcome::Applied { .. }
    ));
    assert!(ws.join(".loom/scripts/new/c.sh").is_file());

    fs::remove_file(defaults.join("scripts/new/c.sh")).unwrap();
    let payload = Payload::from_defaults(defaults, stamp("0.19.882"));
    let diff = materialize_with(&payload, &ws).unwrap();
    assert_eq!(diff.removed, vec![".loom/scripts/new/c.sh"], "{diff:?}");
    apply(&ws, &diff).unwrap();
    assert!(!ws.join(".loom/scripts/new").exists());
    assert_eq!(meta_json(&ws)["installed_files"], serde_json::json!([".loom/scripts/lib/b.sh"]));
}

#[test]
fn removed_loom_owned_file_is_deleted_and_repo_owned_files_survive() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    // `.loom/scripts/lib/b.sh` is in installed_files; `docs/d.md` is shipped
    // today but nothing records it once the payload stops shipping it.
    fs::remove_file(defaults.join("scripts/lib/b.sh")).unwrap();
    fs::remove_file(defaults.join("docs/d.md")).unwrap();
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let before = freeze(&ws);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert_eq!(diff.removed, vec![".loom/scripts/lib/b.sh"], "{diff:?}");
    assert!(diff.added.is_empty() && diff.changed.is_empty(), "{diff:?}");

    apply(&ws, &diff).unwrap();
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), vec![META, ".loom/scripts/lib/b.sh"]);
    assert!(!ws.join(".loom/scripts/lib").exists(), "emptied dir pruned");
    assert!(ws.join(".loom/hooks/post-worktree.sh").is_file());
    assert!(!is_exec(&ws.join(".loom/hooks/post-worktree.sh")), "repo-owned: mode untouched");
    assert!(ws.join(".loom/docs/d.md").is_file(), "no ownership evidence: kept");
    assert_eq!(meta_json(&ws)["installed_files"], serde_json::json!([]));
}

#[test]
fn pinned_file_is_never_overwritten() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    write(&ws.join(".loom/resync-ignore"), "scripts/a.sh\n");
    write(&ws.join(".loom/scripts/a.sh"), "#!/bin/sh\nlocal fix\n");
    write(&defaults.join("scripts/a.sh"), "#!/bin/sh\nupstream\n");
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(diff.is_empty(), "{diff:?}");
}

#[cfg(unix)]
#[test]
fn symlinked_install_targets_are_left_alone() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    let target = tmp.path().join("elsewhere.md");
    write(&target, "outside\n");
    fs::remove_file(ws.join(".loom/roles/builder.md")).unwrap();
    std::os::unix::fs::symlink(&target, ws.join(".loom/roles/builder.md")).unwrap();
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(diff.is_empty(), "{diff:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "outside\n");
}

/// The operator's downgrade case (#10863 review): Compatible by `classify`,
/// but resyncing from this older payload would roll the repo back.
#[test]
fn repo_ahead_of_daemon_is_refused_and_nothing_is_written() {
    for requires in [Some("0.19.772"), None] {
        let tmp = TempDir::new().unwrap();
        let defaults = fake_defaults(tmp.path());
        let ws = installed_workspace(tmp.path(), &defaults, "0.19.900");
        if let Some(req) = requires {
            let mut meta = meta_json(&ws);
            meta["requires_daemon"] = req.into();
            write(&ws.join(META), &serde_json::to_string_pretty(&meta).unwrap());
        }
        write(&defaults.join("scripts/a.sh"), "#!/bin/sh\nolder\n");
        let payload = Payload::from_defaults(defaults, stamp("0.19.880"));

        let before = freeze(&ws);
        let expected = ResyncOutcome::Refused(ResyncRefusal::RepoAheadOfDaemon {
            installed: v("0.19.900"),
            running: v("0.19.880"),
        });
        assert_eq!(resync_workspace_with(&payload, &ws).unwrap(), expected);
        // Applying a diff materialized by hand is refused the same way.
        let diff = materialize_with(&payload, &ws).unwrap();
        assert!(!diff.is_empty());
        assert_eq!(apply(&ws, &diff).unwrap(), expected);
        assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());
    }
}

#[test]
fn needs_newer_daemon_is_refused_and_nothing_is_written() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.870");
    let mut meta = meta_json(&ws);
    meta["requires_daemon"] = "0.19.890".into();
    write(&ws.join(META), &serde_json::to_string_pretty(&meta).unwrap());
    write(&defaults.join("scripts/a.sh"), "#!/bin/sh\nother\n");
    let payload = Payload::from_defaults(defaults, stamp("0.19.880"));

    let before = freeze(&ws);
    let outcome = resync_workspace_with(&payload, &ws).unwrap();
    let ResyncOutcome::Refused(refusal) = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(
        refusal,
        ResyncRefusal::NeedsNewerDaemon {
            requires: v("0.19.890"),
            running: v("0.19.880")
        }
    );
    assert!(refusal.repo_ahead_of_daemon());
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());
}

#[test]
fn not_installed_is_refused() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = tmp.path().join("bare");
    fs::create_dir_all(ws.join(".git")).unwrap();
    let payload = Payload::from_defaults(defaults, stamp("0.19.880"));
    assert_eq!(
        resync_workspace_with(&payload, &ws).unwrap(),
        ResyncOutcome::Refused(ResyncRefusal::NotInstalled)
    );
    assert!(!ws.join(".loom").exists());
}

#[test]
fn unreadable_metadata_is_refused_and_nothing_is_written() {
    for bad in ["not json at all", "[]", "{\"loom_version\": 19}"] {
        let tmp = TempDir::new().unwrap();
        let defaults = fake_defaults(tmp.path());
        let ws = installed_workspace(tmp.path(), &defaults, "0.19.870");
        write(&ws.join(META), bad);
        write(&defaults.join("scripts/a.sh"), "#!/bin/sh\nother\n");
        let payload = Payload::from_defaults(defaults, stamp("0.19.880"));

        let before = freeze(&ws);
        let outcome = resync_workspace_with(&payload, &ws).unwrap();
        assert!(
            matches!(outcome, ResyncOutcome::Refused(ResyncRefusal::UnreadableMetadata(_))),
            "{bad:?}: {outcome:?}"
        );
        // A diff materialized by hand is refused by `apply` the same way.
        let diff = materialize_with(&payload, &ws).unwrap();
        assert!(!diff.is_empty());
        assert!(matches!(
            apply(&ws, &diff).unwrap(),
            ResyncOutcome::Refused(ResyncRefusal::UnreadableMetadata(_))
        ));
        assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());
    }
}

#[test]
fn loom_source_repo_is_refused_and_nothing_is_written() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.870");
    // The explicit marker `is_loom_source_repo` honours.
    write(&ws.join(".loom-source"), "");
    write(&defaults.join("scripts/a.sh"), "#!/bin/sh\nother\n");
    let payload = Payload::from_defaults(defaults, stamp("0.19.880"));

    let before = freeze(&ws);
    let expected = ResyncOutcome::Refused(ResyncRefusal::LoomSourceRepo);
    assert_eq!(resync_workspace_with(&payload, &ws).unwrap(), expected);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(!diff.is_empty());
    assert_eq!(apply(&ws, &diff).unwrap(), expected);
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());
}

/// A payload packed from a dirty checkout is not a release (#10878 review):
/// `LOOM_DAEMON_GIT_DIRTY` other than `clean` clears `Stamp::release_build`.
#[test]
fn dirty_build_payload_is_refused_and_nothing_is_written() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.870");
    write(&defaults.join("scripts/a.sh"), "#!/bin/sh\nuncommitted\n");
    let dirty = Stamp {
        release_build: false,
        ..stamp("0.19.880")
    };
    let payload = Payload::from_defaults(defaults, dirty);

    let before = freeze(&ws);
    let expected = ResyncOutcome::Refused(ResyncRefusal::NotAReleaseBuild);
    assert_eq!(resync_workspace_with(&payload, &ws).unwrap(), expected);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(!diff.is_empty());
    assert_eq!(apply(&ws, &diff).unwrap(), expected);
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());
}

/// This binary's own stamp follows the build's tree state, and nothing else.
#[test]
fn this_binary_is_a_release_build_only_when_built_clean() {
    let stamp = Stamp::this_binary().unwrap();
    assert_eq!(stamp.release_build, crate::self_update::BUILT_TREE_STATE == "clean");
}

/// A recorded version that cannot be ordered might be newer than the daemon
/// (#10878 review): refuse, for either contract field.
#[test]
fn unparseable_recorded_version_is_refused_and_nothing_is_written() {
    for (field, value) in [
        ("loom_version", "0.20.0-rc1"),
        ("loom_version", "0.20"),
        ("requires_daemon", "0.20.0-rc1"),
    ] {
        let tmp = TempDir::new().unwrap();
        let defaults = fake_defaults(tmp.path());
        let ws = installed_workspace(tmp.path(), &defaults, "0.19.870");
        let mut meta = meta_json(&ws);
        meta[field] = value.into();
        write(&ws.join(META), &serde_json::to_string_pretty(&meta).unwrap());
        write(&defaults.join("scripts/a.sh"), "#!/bin/sh\nother\n");
        let payload = Payload::from_defaults(defaults, stamp("0.19.880"));

        let before = freeze(&ws);
        let expected = ResyncOutcome::Refused(ResyncRefusal::UnrecognizedVersion {
            field,
            value: value.to_string(),
        });
        assert_eq!(resync_workspace_with(&payload, &ws).unwrap(), expected, "{field}={value}");
        let diff = materialize_with(&payload, &ws).unwrap();
        assert_eq!(apply(&ws, &diff).unwrap(), expected);
        assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());
    }
}

/// `apply` fails part-way: one file is written, the next cannot be. The stamp
/// must not move, and the next resync must see work to do and finish it.
#[test]
fn partial_failure_leaves_the_stamp_alone_and_the_next_resync_retries() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    write(&defaults.join("docs/d.md"), "doc v2\n");
    write(&defaults.join("scripts/a.sh"), "#!/bin/sh\necho a2\n");
    set_exec(&defaults.join("scripts/a.sh"));
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let diff = materialize_with(&payload, &ws).unwrap();
    assert_eq!(diff.changed, vec![".loom/docs/d.md", ".loom/scripts/a.sh"]);
    // Block the second write: a rename cannot replace a non-empty directory.
    let blocked = ws.join(".loom/scripts/a.sh");
    fs::remove_file(&blocked).unwrap();
    write(&blocked.join("in-the-way"), "x\n");

    assert!(apply(&ws, &diff).is_err());
    assert_eq!(fs::read_to_string(ws.join(".loom/docs/d.md")).unwrap(), "doc v2\n");
    let meta = meta_json(&ws);
    assert_eq!(meta["loom_version"], "0.19.880", "the stamp is never ahead of the files");
    assert_eq!(meta["resync_pending"], "0.19.881");

    // The obstruction is cleared; the next resync writes what is left.
    fs::remove_dir_all(&blocked).unwrap();
    let retry = materialize_with(&payload, &ws).unwrap();
    assert!(!retry.is_empty());
    assert_eq!(retry.added, vec![".loom/scripts/a.sh"], "{retry:?}");
    assert_eq!(
        apply(&ws, &retry).unwrap(),
        ResyncOutcome::Applied {
            written: vec![".loom/scripts/a.sh".to_string(), META.to_string()]
        }
    );
    let meta = meta_json(&ws);
    assert_eq!(meta["loom_version"], "0.19.881");
    assert!(meta.get("resync_pending").is_none());
    assert!(materialize_with(&payload, &ws).unwrap().is_empty());
}

/// The case stamping last used to lose: every file was written and only the
/// stamp was not. The file diff is empty, but the resync is still owed.
#[test]
fn interrupted_before_the_stamp_is_not_an_empty_diff() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    let mut meta = meta_json(&ws);
    meta["resync_pending"] = "0.19.881".into();
    write(&ws.join(META), &serde_json::to_string_pretty(&meta).unwrap());
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let before = freeze(&ws);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(diff.added.is_empty() && diff.changed.is_empty() && diff.removed.is_empty());
    assert!(diff.stamp_pending() && !diff.is_empty(), "{diff:?}");
    drop(diff);

    assert_eq!(
        resync_workspace_with(&payload, &ws).unwrap(),
        ResyncOutcome::Applied {
            written: vec![META.to_string()]
        }
    );
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), vec![META]);
    let meta = meta_json(&ws);
    assert_eq!(meta["loom_version"], "0.19.881");
    assert!(meta.get("resync_pending").is_none());
    // Converged: the next one is a true no-op.
    assert_eq!(resync_workspace_with(&payload, &ws).unwrap(), ResyncOutcome::Unchanged);
}

/// #10718: `loom_version` is stamped last, so a tree a NEWER daemon was
/// part-way through still reads as the old release. Without the marker's
/// version an older daemon would "complete" that run from its own payload and
/// roll the newer files back.
#[test]
fn an_interrupted_resync_by_a_newer_daemon_is_refused_and_nothing_is_written() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.870");
    // 0.19.881 got as far as one file before it died.
    write(&ws.join(".loom/roles/builder.md"), "builder, as of 0.19.881\n");
    let mut meta = meta_json(&ws);
    meta["resync_pending"] = "0.19.881".into();
    write(&ws.join(META), &serde_json::to_string_pretty(&meta).unwrap());

    let before = freeze(&ws);
    let older = Payload::from_defaults(defaults.clone(), stamp("0.19.880"));
    assert_eq!(
        resync_workspace_with(&older, &ws).unwrap(),
        ResyncOutcome::Refused(ResyncRefusal::PendingAheadOfDaemon {
            pending: v("0.19.881"),
            running: v("0.19.880"),
        })
    );
    // `apply` checks for itself, whatever the caller did.
    let diff = materialize_with(&older, &ws).unwrap();
    assert!(matches!(
        apply(&ws, &diff).unwrap(),
        ResyncOutcome::Refused(ResyncRefusal::PendingAheadOfDaemon { .. })
    ));
    assert!(touched(&ws, &before, &snapshot(&ws)).is_empty());
    assert_eq!(
        fs::read_to_string(ws.join(".loom/roles/builder.md")).unwrap(),
        "builder, as of 0.19.881\n"
    );

    // The daemon that started it, or a newer one, completes it.
    let same = Payload::from_defaults(defaults, stamp("0.19.881"));
    assert!(matches!(
        resync_workspace_with(&same, &ws).unwrap(),
        ResyncOutcome::Applied { .. }
    ));
    assert!(meta_json(&ws).get("resync_pending").is_none());
}

#[test]
fn the_pending_marker_is_gated_like_a_version() {
    let daemon = DaemonCompat {
        running: v("0.19.880"),
        supports_installed: v("0.19.0"),
        floor: None,
    };
    let meta = |pending: &str| {
        format!(
            r#"{{"loom_version":"0.19.870","requires_daemon":"0.19.772","resync_pending":{pending}}}"#
        )
    };
    assert_eq!(gate_metadata(&meta(r#""0.19.880""#), &daemon), Ok(Compat::Compatible));
    assert_eq!(gate_metadata(&meta(r#""0.19.800""#), &daemon), Ok(Compat::Compatible));
    assert_eq!(gate_metadata(&meta("null"), &daemon), Ok(Compat::Compatible));
    let ahead = gate_metadata(&meta(r#""0.19.881""#), &daemon).unwrap_err();
    assert!(ahead.repo_ahead_of_daemon(), "{ahead}");
    // A marker that cannot be ordered may be newer: refuse.
    for unordered in [r#""0.20.0-rc1""#, r#""""#, "true", "7"] {
        assert!(
            matches!(
                gate_metadata(&meta(unordered), &daemon),
                Err(ResyncRefusal::UnrecognizedVersion {
                    field: "resync_pending",
                    ..
                })
            ),
            "{unordered}"
        );
    }
    // The existing refusals still come first.
    assert!(matches!(
        gate_metadata("[]", &daemon),
        Err(ResyncRefusal::UnreadableMetadata(_))
    ));
    assert!(matches!(
        gate_metadata(r#"{"loom_version":"0.19.900","resync_pending":"0.19.901"}"#, &daemon),
        Err(ResyncRefusal::RepoAheadOfDaemon { .. })
    ));
}

#[test]
fn gate_proceeds_at_or_below_running_and_for_unrecorded_versions() {
    let daemon = DaemonCompat {
        running: v("0.19.880"),
        supports_installed: v("0.19.0"),
        floor: None,
    };
    let meta = |ver: Option<&str>, req: Option<&str>| InstallMeta {
        loom_version: ver.map(str::to_string),
        requires_daemon: req.map(str::to_string),
    };
    assert_eq!(
        resync_gate(&meta(Some("0.19.880"), Some("0.19.772")), &daemon),
        Ok(Compat::Compatible)
    );
    assert_eq!(
        resync_gate(&meta(Some("0.19.100"), Some("0.19.772")), &daemon),
        Ok(Compat::Compatible)
    );
    assert_eq!(resync_gate(&meta(Some("0.19.880"), None), &daemon), Ok(Compat::ResyncOwed));
    assert_eq!(resync_gate(&meta(None, None), &daemon), Ok(Compat::ResyncOwed));
    assert_eq!(resync_gate(&meta(Some("unknown"), None), &daemon), Ok(Compat::ResyncOwed));
    assert_eq!(resync_gate(&meta(Some("0.18.0"), None), &daemon), Ok(Compat::InstalledTooOld));
    assert_eq!(resync_gate(&meta(Some(""), Some("unknown")), &daemon), Ok(Compat::ResyncOwed));
    assert!(resync_gate(&meta(Some("0.19.881"), None), &daemon).is_err());
    assert_eq!(
        resync_gate(&meta(Some("0.20.0-rc1"), Some("0.19.772")), &daemon),
        Err(ResyncRefusal::UnrecognizedVersion {
            field: "loom_version",
            value: "0.20.0-rc1".to_string()
        })
    );
}

fn is_exec(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

// ---- the embedded payload ----

/// The embedded payload is this checkout's tracked `defaults/`, file for file.
#[test]
fn embedded_payload_is_the_tracked_defaults_tree() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "--", "defaults"])
        .output()
    else {
        return; // no git: build.rs walked the tree instead
    };
    let tracked: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| repo.join(l).symlink_metadata().is_ok())
        .map(|l| l.trim_start_matches("defaults/").to_string())
        .collect();
    if tracked.is_empty() {
        return;
    }
    let payload = Payload::embedded().unwrap();
    let mut unpacked = Vec::new();
    walk_all(payload.defaults(), "", &mut unpacked);
    let mut tracked = tracked;
    tracked.sort();
    unpacked.sort();
    assert_eq!(unpacked, tracked);
    // Byte-identical, executable bits included.
    for rel in [
        "scripts/resync-installed.sh",
        "hooks/guard-destructive-generic.sh",
    ] {
        let embedded = payload.defaults().join(rel);
        assert!(
            fs::read(&embedded).unwrap() == fs::read(repo.join("defaults").join(rel)).unwrap(),
            "{rel} differs from the checkout"
        );
        assert!(is_exec(&embedded), "{rel} keeps its executable bit");
    }
    assert_eq!(payload.stamp().version, v(env!("CARGO_PKG_VERSION")));
}

fn walk_all(root: &Path, rel: &str, out: &mut Vec<String>) {
    for entry in fs::read_dir(root.join(rel)).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            walk_all(root, &child, out);
        } else {
            out.push(child);
        }
    }
}

/// The payload never comes from a Loom checkout: point every checkout knob
/// `init` would consult at an old tree, and the diff is unchanged.
#[test]
#[serial]
fn payload_source_is_independent_of_the_loom_checkout() {
    let tmp = TempDir::new().unwrap();
    let embedded = Payload::embedded().unwrap();
    let ws = tmp.path().join("ws");
    fs::create_dir_all(ws.join(".git")).unwrap();
    write(&ws.join(META), "{\"loom_version\": \"0.19.0\", \"installed_files\": []}\n");
    // Install the embedded payload once, through the same apply path. The
    // test binary may be built from a dirty checkout, which `apply` refuses,
    // so this view of the same tree is stamped as a release.
    let release = Payload::from_defaults(
        embedded.defaults().to_path_buf(),
        Stamp {
            release_build: true,
            ..embedded.stamp().clone()
        },
    );
    let first = materialize_with(&release, &ws).unwrap();
    assert!(!first.is_empty());
    assert!(matches!(apply(&ws, &first).unwrap(), ResyncOutcome::Applied { .. }));

    // An "old checkout" that ships a different script and is missing the rest.
    let old = tmp.path().join("old-loom");
    write(&old.join("defaults/scripts/resync-installed.sh"), "#!/bin/sh\nold\n");
    write(&old.join("package.json"), "{\"version\": \"0.1.0\"}\n");
    write(&ws.join(".loom/loom-source-path"), &format!("{}\n", old.display()));
    let saved: Vec<(&str, Option<String>)> = ["LOOM_MACHINE_CHECKOUT", "LOOM_DAEMON_DEFAULTS_DIR"]
        .into_iter()
        .map(|k| (k, std::env::var(k).ok()))
        .collect();
    std::env::set_var("LOOM_MACHINE_CHECKOUT", &old);
    std::env::set_var("LOOM_DAEMON_DEFAULTS_DIR", old.join("defaults"));

    let second = materialize_payload(&ws);

    for (k, val) in saved {
        match val {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
    let second = second.unwrap();
    assert!(second.is_empty(), "{second:?}");
    assert_eq!(second.stamp(), embedded.stamp());
}

/// Drift guard for [`install_payload_files`]: every file it writes into a
/// fresh workspace is one the diff of a fresh workspace reports as added, so
/// a new installer surface cannot silently fall outside resync.
#[test]
fn payload_surface_covers_everything_the_installer_writes() {
    let tmp = TempDir::new().unwrap();
    let embedded = Payload::embedded().unwrap();
    let fresh = |name: &str| {
        let ws = tmp.path().join(name);
        fs::create_dir_all(ws.join(".git")).unwrap();
        write(&ws.join(META), "{\"loom_version\": \"0.19.0\"}\n");
        ws
    };

    let installed = fresh("installed");
    let mut report = InitReport::default();
    install_payload_files(
        &installed,
        embedded.defaults(),
        &installed.join(".loom"),
        false,
        &mut report,
    )
    .unwrap();
    let mut written = Vec::new();
    walk(&installed, &mut |p| {
        written.push(
            p.strip_prefix(&installed)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
    });
    written.retain(|p| p != META);
    assert!(written.len() > 100, "the real payload installs hundreds of files");

    let diff = materialize_with(&embedded, &fresh("probe")).unwrap();
    assert!(diff.changed.is_empty() && diff.removed.is_empty(), "{diff:?}");
    let missed: Vec<&String> = written.iter().filter(|p| !diff.added.contains(p)).collect();
    assert!(missed.is_empty(), "installer writes outside the resync surface: {missed:?}");
    // The only extra surface is the slash commands `init` copies with `.claude/`.
    assert!(diff
        .added
        .iter()
        .all(|p| written.contains(p) || p.starts_with(".claude/commands/loom/")));
}
