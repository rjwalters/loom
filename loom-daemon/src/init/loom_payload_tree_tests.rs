//! Tests for the `defaults/.loom/` payload walk (#9123).
//!
//! `install-metadata.json`'s `installed_files` comes from
//! `scripts/install/manifest.sh`, which WALKS `defaults/` and registers every
//! file under `defaults/.loom/` as Loom-installed. The copy side — this
//! crate's [`initialize_workspace`] — used to name the members of that subtree
//! one by one, so a file added to `defaults/.loom/` was recorded as installed
//! and never copied. These tests hold the two enumerations to the same set and
//! cover the post-condition that makes [`LOOM_TREE_SCAFFOLDED_FILES`] an
//! alternate-handler list rather than an exclusion list.
//!
//! Lives beside `tests.rs` rather than inside it because `init/tests.rs` is
//! over the `scripts/check-file-size-budget.sh` threshold and frozen at its
//! current size; new test modules go in sibling files (same pattern as
//! `credential_class_tests.rs`).

use super::*;
use tempfile::TempDir;

/// Repo root (`loom-daemon/`'s parent), for tests that run against the real
/// shipped tree. Same resolution the `test_real_defaults_*` tests in
/// `tests.rs` use.
pub(super) fn repo_root_for_tests() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ has a parent")
        .to_path_buf()
}

/// Every file `defaults/.loom/` ships, as target-relative paths
/// (`.loom/<relative path>`), discovered by walking the source tree.
fn defaults_loom_tree_paths(defaults: &Path) -> std::collections::BTreeSet<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut std::collections::BTreeSet<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_transient_artifact(&name) {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => walk(&entry.path(), &rel, out),
                Ok(_) => {
                    out.insert(format!(".loom/{rel}"));
                }
                Err(_) => {}
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(&defaults.join(".loom"), "", &mut out);
    out
}

/// Run `scripts/install/manifest.sh`'s `_emit_loom_ownership_set` — the
/// enumeration that becomes `install-metadata.json`'s `installed_files` — and
/// return it as a set of target-relative paths.
pub(super) fn installed_files_manifest(
    repo_root: &Path,
    target: &Path,
) -> std::collections::BTreeSet<String> {
    let script = repo_root.join("scripts/install/manifest.sh");
    assert!(
        script.is_file(),
        "manifest helper not found at {script:?} — repo layout changed?"
    );

    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(r#"set -euo pipefail; source "$LOOM_ROOT/scripts/install/manifest.sh"; _emit_loom_ownership_set"#)
        .env("LOOM_ROOT", repo_root)
        .env("TARGET_PATH", target)
        .output()
        .expect("failed to run bash for the installed-files manifest");
    assert!(
        out.status.success(),
        "manifest.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[test]
fn test_defaults_loom_tree_copy_and_manifest_enumerations_agree() {
    // Issue #9123. `install-metadata.json`'s `installed_files` comes from
    // `scripts/install/manifest.sh`, which WALKS `defaults/` and registers
    // every file under `defaults/.loom/` as Loom-installed. The copy side
    // (this crate's `initialize_workspace`) used to name the members of that
    // subtree one by one — `.loom/biome.jsonc` and `.loom/bin/` — so
    // `defaults/.loom/credentials.md.example` was recorded as installed and
    // never copied: `install.sh --full` failed its own completeness check and
    // `--quick`, which does not run that check, reported success on the same
    // incomplete install.
    //
    // This asserts the two enumerations produce the SAME SET for a fresh
    // target, which is the property the fix restores. It fails for ANY future
    // file dropped from the `defaults/.loom/` copy, not just that one.
    let repo_root = repo_root_for_tests();
    let defaults = repo_root.join("defaults");
    assert!(defaults.is_dir(), "shipped defaults/ tree not found at {defaults:?}");

    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    fs::create_dir(workspace.join(".git")).unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init against real defaults/ failed: {:?}", result.err());

    // The enumeration that feeds `installed_files`, narrowed to the entries
    // that originate in `defaults/.loom/`. The rest of the manifest comes
    // from other steps of the same init (`defaults/roles/`,
    // `defaults/config/`, and so on); the whole-manifest form of this
    // assertion is `config_dir_tests.rs`'s
    // `test_standalone_init_materializes_the_whole_installed_files_manifest`.
    let shipped = defaults_loom_tree_paths(&defaults);
    assert!(
        !shipped.is_empty(),
        "defaults/.loom/ ships no files — the fixture for this test is gone"
    );
    let manifest: std::collections::BTreeSet<String> =
        installed_files_manifest(&repo_root, workspace)
            .into_iter()
            .filter(|p| shipped.contains(p))
            .collect();

    // Side 1: the manifest walk lists every file the tree ships.
    assert_eq!(
        manifest, shipped,
        "scripts/install/manifest.sh no longer lists every file under defaults/.loom/"
    );

    // Side 2: the copy walk put every one of them on disk.
    let on_disk: std::collections::BTreeSet<String> = shipped
        .iter()
        .filter(|p| workspace.join(p).is_file())
        .cloned()
        .collect();
    assert_eq!(
        on_disk,
        manifest,
        "install-metadata.json's installed_files and the files loom-daemon init \
         actually copies disagree (#9123). Recorded but never copied: {:?}",
        manifest.difference(&on_disk).collect::<Vec<_>>()
    );

    // Sanity: the set is non-trivial and spans both shapes the walk handles —
    // a top-level file inside `.loom/`, a file in a subdirectory of `.loom/`,
    // and a template-substituted member written by scaffolding.
    for expected in &[".loom/biome.jsonc", ".loom/bin/loom", ".loom/CLAUDE.md"] {
        assert!(
            manifest.contains(*expected),
            "{expected} missing from the agreed set — test no longer covers what it claims"
        );
    }
}

#[test]
fn test_init_copies_top_level_file_inside_defaults_loom_tree() {
    // Issue #9123, fixture-tree form: a `defaults/.loom/` carrying a top-level
    // file ALONGSIDE subdirectories. The pre-fix copy path enumerated that
    // subtree by name, so a top-level file nobody had hardcoded (the real one
    // was `credentials.md.example`) was silently skipped while subdirectories
    // still landed. Nothing here is named in `sync_loom_payload_tree` — the
    // file lands because the tree is walked.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join(".loom").join("bin")).unwrap();
    fs::create_dir_all(defaults.join(".loom").join("presets")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();

    // A top-level file inside `.loom/` with no dedicated call site anywhere.
    fs::write(defaults.join(".loom").join("sample.md.example"), "# sample payload\n").unwrap();
    // …a second one, to prove this is not a one-name carve-out.
    fs::write(defaults.join(".loom").join("payload.json"), "{\"a\":1}\n").unwrap();
    // …a subdirectory member, which already worked and must keep working.
    fs::write(defaults.join(".loom").join("bin").join("loom"), "#!/bin/sh\n").unwrap();
    fs::write(defaults.join(".loom").join("presets").join("nested.txt"), "nested\n").unwrap();
    // …and a template-substituted member owned by scaffolding.
    fs::write(defaults.join(".loom").join("CLAUDE.md"), "# Loom {{LOOM_VERSION}}\n").unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {:?}", result.err());
    let report = result.unwrap();

    for (rel, contents) in &[
        ("sample.md.example", "# sample payload\n"),
        ("payload.json", "{\"a\":1}\n"),
        ("bin/loom", "#!/bin/sh\n"),
        ("presets/nested.txt", "nested\n"),
    ] {
        let installed = workspace.join(".loom").join(rel);
        assert!(
            installed.is_file(),
            ".loom/{rel} should be installed from defaults/.loom/{rel} (#9123)"
        );
        assert_eq!(&fs::read_to_string(&installed).unwrap(), contents);
        assert!(
            report.added.contains(&format!(".loom/{rel}")),
            "report should list .loom/{rel} as added, got: {:?}",
            report.added
        );
    }

    // Scaffolding still owns the substituted members.
    let claude = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
    assert!(!claude.contains("{{"), ".loom/CLAUDE.md must be template-substituted: {claude}");
}

#[test]
fn test_init_fails_when_defaults_loom_tree_file_is_not_materialized() {
    // The easy half of the post-condition (#9123): a file the verbatim walk
    // IS responsible for. `orphan.md.example` is not on
    // `LOOM_TREE_SCAFFOLDED_FILES`, so this covers the shape where the copy
    // itself failed — shipped in `defaults/.loom/`, absent from `.loom/`,
    // already claimed by `install-metadata.json` — and pins the two things
    // callers rely on in the error text: the missing path, and why it
    // matters. The hard half, a name the walk deliberately SKIPS because the
    // list says someone else writes it, is
    // `test_scaffolded_name_without_a_handler_fails_the_install` below.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join(".loom")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();

    let err = assert_loom_payload_tree_complete(&defaults, &workspace.join(".loom"));
    assert!(err.is_ok(), "an empty defaults/.loom/ has nothing to miss: {err:?}");

    fs::write(defaults.join(".loom").join("orphan.md.example"), "x\n").unwrap();
    let err = assert_loom_payload_tree_complete(&defaults, &workspace.join(".loom"))
        .expect_err("a file shipped in defaults/.loom/ but absent on disk must fail the init");
    assert!(
        err.contains(".loom/orphan.md.example"),
        "error must name the missing path, got: {err}"
    );
    assert!(
        err.contains("install-metadata.json"),
        "error must explain why it matters (the metadata-vs-disk check), got: {err}"
    );
}

#[test]
fn test_scaffolded_name_without_a_handler_fails_the_install() {
    // The hard half, and the property the entire `LOOM_TREE_SCAFFOLDED_FILES`
    // safety argument rests on (#9123): that list is an ALTERNATE-HANDLER
    // list, never an exclusion list. Putting a name on it moves responsibility
    // for writing the file from the verbatim walk to some other handler; it
    // can never drop the file from the install, because
    // `assert_loom_payload_tree_complete` still requires it on disk.
    //
    // Demonstrating that needs a listed name that no handler writes, which the
    // shipped list cannot supply — both of its members are written
    // unconditionally by `setup_repository_scaffolding` whenever their source
    // exists. Hence the injected list: `ghost.jsonc` stands in for the next
    // name someone adds with nothing behind it.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");
    let loom_path = workspace.join(".loom");

    fs::create_dir_all(defaults.join(".loom")).unwrap();
    fs::create_dir_all(&loom_path).unwrap();
    fs::write(defaults.join(".loom").join("ghost.jsonc"), "{}\n").unwrap();
    // A control, to prove the walk ran at all rather than bailing early.
    fs::write(defaults.join(".loom").join("carried.json"), "{\"a\":1}\n").unwrap();

    let mut report = InitReport::default();
    sync_loom_payload_tree_with(
        &defaults,
        &loom_path,
        false,
        &OwnershipBoundary::default(),
        &mut report,
        &["ghost.jsonc"],
    )
    .expect("the walk itself succeeds — skipping a listed name is not an error");

    // Half 1: the list really does take the file away from the verbatim walk,
    // which is the whole reason it exists (a scaffolded member copied verbatim
    // would install raw `{{LOOM_VERSION}}` source and double-report the path).
    assert!(
        !loom_path.join("ghost.jsonc").exists(),
        "a name on the scaffolded list must be skipped by the verbatim walk"
    );
    assert!(
        loom_path.join("carried.json").is_file(),
        "the walk must still copy everything NOT on the list: {:?}",
        report.added
    );

    // Half 2: and the post-condition still requires it, so an init that
    // reaches this state fails loudly instead of handing back a `.loom/` that
    // `install-metadata.json` already claims is complete — the #9123 failure
    // mode, which `install.sh --full` rolls back and `--quick` used to ship.
    let err = assert_loom_payload_tree_complete(&defaults, &loom_path)
        .expect_err("a listed name that no handler wrote must fail the init");
    assert!(
        err.contains(".loom/ghost.jsonc"),
        "error must name the missing path, got: {err}"
    );
    assert!(
        err.contains("install-metadata.json"),
        "error must explain why it matters (the metadata-vs-disk check), got: {err}"
    );
    assert!(
        !err.contains("carried.json"),
        "only the unwritten file should be reported missing, got: {err}"
    );

    // …and once a handler does write it — what the two real members of the
    // list get from `setup_repository_scaffolding` — the same tree passes.
    fs::write(loom_path.join("ghost.jsonc"), "written by the alternate handler\n").unwrap();
    assert_loom_payload_tree_complete(&defaults, &loom_path)
        .expect("a listed name written by its handler satisfies the post-condition");
}

#[test]
fn test_every_scaffolded_name_is_still_required_on_disk() {
    // The same property, asserted over the SHIPPED list rather than an
    // injected one, so it keeps holding as the list changes: for every name on
    // `LOOM_TREE_SCAFFOLDED_FILES`, a source that exists with no destination
    // written is an error. This is what forbids "optimizing"
    // `assert_loom_payload_tree_complete` by skipping the listed names — the
    // edit that would turn the alternate-handler list back into an exclusion
    // list and re-open #9123.
    assert!(
        !LOOM_TREE_SCAFFOLDED_FILES.is_empty(),
        "an empty list makes this test vacuous — if the list is gone, delete the test"
    );
    for name in LOOM_TREE_SCAFFOLDED_FILES {
        let temp_dir = TempDir::new().unwrap();
        let defaults = temp_dir.path().join("defaults");
        let loom_path = temp_dir.path().join(".loom");
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::create_dir_all(&loom_path).unwrap();
        fs::write(defaults.join(".loom").join(name), "# template\n").unwrap();

        let err = assert_loom_payload_tree_complete(&defaults, &loom_path).expect_err(
            "a scaffolded name shipped in defaults/.loom/ but absent on disk must fail the init",
        );
        assert!(
            err.contains(&format!(".loom/{name}")),
            "error must name the missing scaffolded file .loom/{name}, got: {err}"
        );
    }
}
