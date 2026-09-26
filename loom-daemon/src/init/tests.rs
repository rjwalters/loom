use super::*;
use tempfile::TempDir;

#[test]
fn test_direct_init_writes_install_metadata_and_substitutes_version() {
    // #4050: a direct `loom-daemon init` (no LOOM_* env exported by any
    // shell wrapper) must still write `.loom/install-metadata.json` with a
    // real version, and must substitute a real version into `.loom/CLAUDE.md`
    // rather than leaking the literal "unknown".
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    // The defaults dir MUST be named `defaults` so loom_source is derivable.
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join(".loom")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    // A CLAUDE.md template carrying the version placeholder.
    fs::write(
        defaults.join(".loom").join("CLAUDE.md"),
        "# Loom\n\n**Loom Version**: {{LOOM_VERSION}}\n**Loom Commit**: {{LOOM_COMMIT}}\n",
    )
    .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");

    // 1. install-metadata.json exists with a non-"unknown" version/commit.
    let meta_raw =
        fs::read_to_string(workspace.join(".loom").join("install-metadata.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).unwrap();
    let version = meta["loom_version"].as_str().unwrap();
    assert!(!version.is_empty(), "loom_version must not be empty");
    assert_ne!(version, "unknown", "loom_version must not be the literal unknown");
    assert!(!version.contains("{{"), "loom_version must not be an unsubstituted placeholder");
    // #5624: install-metadata.json is committed, so it must never carry
    // the installing machine's absolute path. The derived source root is
    // recorded only in the gitignored `.loom/loom-source-path` sidecar.
    assert!(
        meta.get("loom_source").is_none(),
        "install-metadata.json must never record loom_source (#5624)"
    );
    let sidecar_src = fs::read_to_string(workspace.join(".loom").join("loom-source-path")).unwrap();
    assert!(!sidecar_src.trim().is_empty());

    // 2. .loom/CLAUDE.md has no leftover placeholder and no "unknown" version.
    let claude = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
    assert!(!claude.contains("{{"), "CLAUDE.md must have no unsubstituted placeholder");
    assert!(
        !claude.contains("**Loom Version**: unknown"),
        "CLAUDE.md must not render the unknown version: {claude}"
    );

    // 3. Re-running init is idempotent — the metadata file still parses and
    //    carries the same schema (no duplicate/garbled JSON).
    let result2 =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), true);
    assert!(result2.is_ok(), "reinstall failed: {result2:?}");
    let meta_raw2 =
        fs::read_to_string(workspace.join(".loom").join("install-metadata.json")).unwrap();
    let meta2: serde_json::Value = serde_json::from_str(&meta_raw2).unwrap();
    assert_eq!(meta2["loom_version"].as_str().unwrap(), version);
}

#[test]
fn test_is_loom_source_repo_marker_file() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();

    // Initially not a Loom source repo
    assert!(!is_loom_source_repo(workspace));

    // Create marker file
    fs::write(workspace.join(".loom-source"), "").unwrap();

    // Now it should be detected as Loom source repo
    assert!(is_loom_source_repo(workspace));
}

#[test]
fn test_is_loom_source_repo_directory_structure() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();

    // Initially not a Loom source repo
    assert!(!is_loom_source_repo(workspace));

    // Create partial structure (not enough)
    fs::create_dir(workspace.join("loom-api")).unwrap();
    assert!(!is_loom_source_repo(workspace));

    // Create more structure
    fs::create_dir(workspace.join("loom-daemon")).unwrap();
    assert!(!is_loom_source_repo(workspace));

    // Create defaults directory
    fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
    fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();

    // Now it should be detected as Loom source repo
    assert!(is_loom_source_repo(workspace));
}

#[test]
fn test_self_install_returns_validation_report() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();

    // Create git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create Loom source structure
    fs::create_dir(workspace.join("loom-api")).unwrap();
    fs::create_dir(workspace.join("loom-daemon")).unwrap();
    fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
    fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();

    // Create minimal .loom structure
    fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
    fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
    fs::write(workspace.join(".loom").join("roles").join("builder.md"), "").unwrap();
    fs::write(workspace.join(".loom").join("scripts").join("worktree.sh"), "").unwrap();

    // Create .claude/commands/loom/
    fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
    for cmd in [
        "builder.md",
        "judge.md",
        "curator.md",
        "doctor.md",
        "shepherd.md",
    ] {
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join(cmd),
            "",
        )
        .unwrap();
    }

    // Create .claude/agents/ (subagent definitions — required for
    // native Claude Code subagent dispatch). See issue #3310.
    fs::create_dir_all(workspace.join(".claude").join("agents")).unwrap();
    for agent in [
        "loom-builder.md",
        "loom-judge.md",
        "loom-curator.md",
        "loom-doctor.md",
        "loom-shepherd.md",
    ] {
        fs::write(workspace.join(".claude").join("agents").join(agent), "").unwrap();
    }

    // Create roles to satisfy the >=5 role-count check (issue #3310 makes
    // agents/ subject to the same kind of minimum-count audit and the
    // validation report now bails on too-few defaults across the board).
    for role in [
        "builder.md",
        "judge.md",
        "curator.md",
        "doctor.md",
        "shepherd.md",
    ] {
        fs::write(workspace.join(".loom").join("roles").join(role), "").unwrap();
    }

    // Create a couple of scripts to satisfy the >=2 script-count check.
    fs::write(workspace.join(".loom").join("scripts").join("daemon.sh"), "").unwrap();

    // Create docs
    fs::write(workspace.join("CLAUDE.md"), "").unwrap();

    // Create labels.yml
    fs::create_dir_all(workspace.join(".github")).unwrap();
    fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

    // Run initialization
    let result = initialize_workspace(
        workspace.to_str().unwrap(),
        "nonexistent-defaults", // Should not be used for self-install
        false,
    );

    assert!(result.is_ok());
    let report = result.unwrap();

    // Verify self-install detection
    assert!(report.is_self_install);
    assert!(report.validation.is_some());

    let validation = report.validation.unwrap();
    assert!(validation.roles_found.contains(&"builder".to_string()));
    assert!(validation.scripts_found.contains(&"worktree".to_string()));
    assert!(validation.commands_found.contains(&"builder".to_string()));
    // Issue #3310: subagent definitions must be discovered for native
    // Claude Code `subagent_type` dispatch to work.
    assert!(validation
        .agents_found
        .contains(&"loom-builder".to_string()));
    assert!(validation.has_claude_md);
    assert!(validation.has_labels_yml);
    // Verify the missing-agents-directory issue is NOT raised when
    // the directory exists with the expected fixtures.
    assert!(!validation
        .issues
        .iter()
        .any(|i| i.contains("Missing .claude/agents/")));
}

#[test]
fn test_self_install_flags_missing_agents_directory() {
    // Issue #3310: a self-installed Loom checkout that is missing
    // `.claude/agents/` cannot dispatch subagents. The validation
    // report must surface this as an explicit issue so downstream
    // tooling (installer reconciliation, `loom-daemon init`) can
    // fail loudly instead of silently producing a broken install.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();

    // Mark this directory as a Loom source repo via the marker file.
    // We deliberately do NOT create `.claude/agents/` here.
    fs::create_dir(workspace.join(".git")).unwrap();
    fs::write(workspace.join(".loom-source"), "").unwrap();
    fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
    fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
    fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
    fs::write(workspace.join("CLAUDE.md"), "").unwrap();
    fs::create_dir_all(workspace.join(".github")).unwrap();
    fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

    let result = initialize_workspace(workspace.to_str().unwrap(), "nonexistent-defaults", false);
    assert!(result.is_ok());

    let report = result.unwrap();
    assert!(report.is_self_install);
    let validation = report.validation.expect("validation report present");
    assert!(validation.agents_found.is_empty());
    assert!(
        validation
            .issues
            .iter()
            .any(|i| i == "Missing .claude/agents/ directory"),
        "Expected missing-agents-directory issue, got: {:?}",
        validation.issues
    );
}

#[cfg(unix)]
#[test]
fn test_self_install_creates_dogfood_symlinks_on_fresh_clone() {
    // Issue #6440: a fresh clone of the Loom source repo has NO
    // `.claude/commands/loom` or `.claude/agents` yet (they are
    // gitignored, hand-created setup state) — only `defaults/.claude/...`
    // is tracked. `loom-daemon init` (the call `fleet add-worker` makes
    // when provisioning a daemon workspace) must create both symlinks
    // itself, not merely report them missing.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir(workspace.join("loom-api")).unwrap();
    fs::create_dir(workspace.join("loom-daemon")).unwrap();
    fs::create_dir_all(workspace.join("defaults").join("roles")).unwrap();
    fs::write(workspace.join("defaults").join("config.json"), "{}").unwrap();

    // `defaults/.claude/commands/loom/` and `defaults/.claude/agents/`
    // are the TRACKED source of truth — populate them, but deliberately
    // do NOT create `.claude/commands/loom` or `.claude/agents` at the
    // workspace root (the fresh-clone shape this issue is about).
    let cmd_src = workspace
        .join("defaults")
        .join(".claude")
        .join("commands")
        .join("loom");
    fs::create_dir_all(&cmd_src).unwrap();
    for cmd in [
        "builder.md",
        "judge.md",
        "curator.md",
        "doctor.md",
        "shepherd.md",
    ] {
        fs::write(cmd_src.join(cmd), "").unwrap();
    }
    let agents_src = workspace.join("defaults").join(".claude").join("agents");
    fs::create_dir_all(&agents_src).unwrap();
    for agent in [
        "loom-builder.md",
        "loom-judge.md",
        "loom-curator.md",
        "loom-doctor.md",
        "loom-shepherd.md",
    ] {
        fs::write(agents_src.join(agent), "").unwrap();
    }

    fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
    fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
    for role in [
        "builder.md",
        "judge.md",
        "curator.md",
        "doctor.md",
        "shepherd.md",
    ] {
        fs::write(workspace.join(".loom").join("roles").join(role), "").unwrap();
    }
    fs::write(workspace.join(".loom").join("scripts").join("daemon.sh"), "").unwrap();
    fs::write(workspace.join("CLAUDE.md"), "").unwrap();
    fs::create_dir_all(workspace.join(".github")).unwrap();
    fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

    let result = initialize_workspace(workspace.to_str().unwrap(), "nonexistent-defaults", false);
    assert!(result.is_ok());
    let report = result.unwrap();
    assert!(report.is_self_install);

    let commands_link = workspace.join(".claude").join("commands").join("loom");
    let agents_link = workspace.join(".claude").join("agents");
    assert!(
        fs::symlink_metadata(&commands_link)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        ".claude/commands/loom must be created as a symlink"
    );
    assert!(
        fs::symlink_metadata(&agents_link)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        ".claude/agents must be created as a symlink"
    );

    // Validation ran AFTER symlink creation, so it must see the
    // newly-linked content rather than reporting it missing.
    let validation = report.validation.expect("validation report present");
    assert!(validation.commands_found.contains(&"builder".to_string()));
    assert!(validation
        .agents_found
        .contains(&"loom-builder".to_string()));
    assert!(!validation
        .issues
        .iter()
        .any(|i| i.contains(".claude/commands/loom")));
    assert!(!validation
        .issues
        .iter()
        .any(|i| i.contains(".claude/agents")));
}

#[cfg(unix)]
#[test]
fn test_dogfood_symlink_creation_never_fires_on_a_consumer_repo() {
    // Safety-critical scoping test (Issue #6440's own complexity note):
    // `link_dogfood_symlinks` performs filesystem writes (symlink
    // creation, and directory removal in the "safe to replace" case)
    // that would silently corrupt a CONSUMER repo's tracked, real
    // `.claude/commands/loom/` and `.claude/agents/` content if that
    // logic ever ran outside `is_loom_source_repo()`. This workspace
    // deliberately has none of the loom-source markers (no
    // `.loom-source`, no `loom-api`/`loom-daemon` directories) — the
    // ordinary consumer-repo shape — with pre-existing REAL,
    // non-defaults-tracked command/agent files already on disk, exactly
    // as a real consumer project would have after installing Loom.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    // Ordinary consumer git repo — NOT the Loom source repo.
    fs::create_dir(workspace.join(".git")).unwrap();

    // Defaults ships a loom/ command dir and an agents dir, same shape a
    // real `defaults/` tree would.
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    let cmd_src = defaults.join(".claude").join("commands").join("loom");
    fs::create_dir_all(&cmd_src).unwrap();
    fs::write(cmd_src.join("sweep.md"), "# sweep (from defaults)").unwrap();
    let agents_src = defaults.join(".claude").join("agents");
    fs::create_dir_all(&agents_src).unwrap();
    fs::write(agents_src.join("loom-builder.md"), "# builder (from defaults)").unwrap();

    // The consumer workspace already has REAL, tracked content at both
    // paths — including a custom file that exists ONLY in the workspace,
    // never in defaults/. If dogfood-symlink creation ever fired here,
    // this custom content would be silently discarded when the real
    // directory is replaced by a symlink.
    let cmd_dst = workspace.join(".claude").join("commands").join("loom");
    fs::create_dir_all(&cmd_dst).unwrap();
    fs::write(cmd_dst.join("sweep.md"), "# sweep (consumer's installed copy)").unwrap();
    fs::write(cmd_dst.join("custom-local.md"), "consumer-only content").unwrap();
    let agents_dst = workspace.join(".claude").join("agents");
    fs::create_dir_all(&agents_dst).unwrap();
    fs::write(agents_dst.join("loom-builder.md"), "# builder (consumer's installed copy)").unwrap();
    fs::write(agents_dst.join("custom-local-agent.md"), "consumer-only agent").unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");
    let report = result.unwrap();

    assert!(!report.is_self_install, "an ordinary consumer repo must never self-install");

    // Neither path was ever replaced with a symlink.
    assert!(
        !fs::symlink_metadata(&cmd_dst)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        ".claude/commands/loom must stay a real directory in a consumer repo, not a symlink"
    );
    assert!(
        !fs::symlink_metadata(&agents_dst)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        ".claude/agents must stay a real directory in a consumer repo, not a symlink"
    );

    // The consumer's local-only content survived untouched.
    assert!(
        cmd_dst.join("custom-local.md").is_file(),
        "consumer-only command file must never be discarded"
    );
    assert_eq!(
        fs::read_to_string(cmd_dst.join("custom-local.md")).unwrap(),
        "consumer-only content"
    );
    assert!(
        agents_dst.join("custom-local-agent.md").is_file(),
        "consumer-only agent file must never be discarded"
    );
    assert_eq!(
        fs::read_to_string(agents_dst.join("custom-local-agent.md")).unwrap(),
        "consumer-only agent"
    );
}

#[test]
fn test_self_install_skips_retired_file_cleanup() {
    // Issue #3576: the retired-file cleanup is placed AFTER the self-install
    // short-circuit in `initialize_workspace`, so it must never touch the
    // Loom source tree. A stray `.claude/commands/loom/release.md` in a
    // self-install workspace is left exactly as-is: not removed, and not
    // even recorded in `report.preserved` (which would signal the cleanup
    // ran and evaluated it — i.e. the call was misplaced before the early
    // return).
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::write(workspace.join(".loom-source"), "").unwrap();
    fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
    fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
    fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
    fs::create_dir_all(workspace.join(".claude").join("agents")).unwrap();
    fs::write(workspace.join("CLAUDE.md"), "").unwrap();
    fs::create_dir_all(workspace.join(".github")).unwrap();
    fs::write(workspace.join(".github").join("labels.yml"), "").unwrap();

    // A retired stray on disk in the source tree.
    let stray = workspace
        .join(".claude")
        .join("commands")
        .join("loom")
        .join("release.md");
    fs::write(&stray, "some release.md content\n").unwrap();

    let result = initialize_workspace(workspace.to_str().unwrap(), "nonexistent-defaults", false);
    assert!(result.is_ok());
    let report = result.unwrap();

    assert!(report.is_self_install);
    // Cleanup never ran: file untouched, and it appears in neither list.
    assert!(stray.exists(), "self-install must not remove the stray release.md");
    let retired = ".claude/commands/loom/release.md".to_string();
    assert!(!report.removed.contains(&retired));
    assert!(!report.preserved.contains(&retired));
}

#[test]
fn test_roles_cleaned_and_updated_on_reinstall() {
    // On reinstall, managed directories (roles/, scripts/) are cleaned first
    // to remove stale files, then fresh defaults are copied in.
    // Custom files that aren't in defaults are removed (not preserved).
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    // Setup git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with roles
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "new builder content v2").unwrap();
    fs::write(defaults.join("roles").join("judge.md"), "new judge content").unwrap();

    // Create existing .loom directory (simulates previous install)
    fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
    fs::write(
        workspace.join(".loom").join("roles").join("builder.md"),
        "old builder content v1",
    )
    .unwrap();
    fs::write(workspace.join(".loom").join("roles").join("stale-role.md"), "stale role").unwrap();
    // #5971: the sweep retires a file only when something attributes it to
    // Loom — here, the previous install's own record.
    write_prev_manifest(workspace, &[".loom/roles/stale-role.md"]);

    // Run initialization WITHOUT force flag (simulates normal reinstall)
    let result = initialize_workspace(
        workspace.to_str().unwrap(),
        defaults.to_str().unwrap(),
        false, // No force flag
    );

    assert!(result.is_ok());
    let report = result.unwrap();

    // Verify: builder.md has new content from defaults
    let builder =
        fs::read_to_string(workspace.join(".loom").join("roles").join("builder.md")).unwrap();
    assert_eq!(builder, "new builder content v2");

    // Verify: judge.md was ADDED (new default role)
    let judge = fs::read_to_string(workspace.join(".loom").join("roles").join("judge.md")).unwrap();
    assert_eq!(judge, "new judge content");

    // Verify: stale-role.md was REMOVED (not in defaults)
    assert!(
        !workspace
            .join(".loom")
            .join("roles")
            .join("stale-role.md")
            .exists(),
        "Stale role file should have been removed on reinstall"
    );

    // Verify report reflects the removal
    assert!(
        report
            .removed
            .contains(&".loom/roles/stale-role.md".to_string()),
        "Report should list stale-role.md as removed, got: {:?}",
        report.removed
    );

    // Both files from defaults should be reported as added (directory was cleaned first)
    assert!(report.added.contains(&".loom/roles/builder.md".to_string()));
    assert!(report.added.contains(&".loom/roles/judge.md".to_string()));
}

#[test]
fn test_loom_bin_directory_copied_on_install() {
    // Regression: `.loom/bin/` (the loom CLI wrapper) must be copied from
    // `defaults/.loom/bin/`. The install manifest generator walks
    // `defaults/.loom/` and lists `.loom/bin/loom` as a shipped file, so
    // if initialize_workspace omits the copy, the installer's
    // post-install metadata-vs-disk check fails with
    // "MISSING: .loom/bin/loom" and rolls the whole install back.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    // Minimal git repo + defaults.
    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(&defaults).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();

    // Ship a loom CLI wrapper under defaults/.loom/bin/.
    fs::create_dir_all(defaults.join(".loom").join("bin")).unwrap();
    let wrapper = "#!/usr/bin/env bash\necho loom\n";
    fs::write(defaults.join(".loom").join("bin").join("loom"), wrapper).unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");

    // The wrapper must land at .loom/bin/loom with identical contents.
    let installed = workspace.join(".loom").join("bin").join("loom");
    assert!(installed.exists(), ".loom/bin/loom should be copied from defaults/.loom/bin/");
    assert_eq!(fs::read_to_string(&installed).unwrap(), wrapper);
}

#[test]
fn test_loom_biome_config_copied_on_install() {
    // Regression (#6031): `.loom/biome.jsonc` — the nested Biome config that
    // excludes the machine-managed `.loom/` tree from a consumer's repo-wide
    // `biome check .` — must be copied from `defaults/.loom/biome.jsonc`.
    // The install manifest generator walks `defaults/.loom/` and lists it as
    // a shipped file, so omitting the copy fails the installer's post-install
    // metadata-vs-disk check with "MISSING: .loom/biome.jsonc".
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join(".loom")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();

    let biome_config = "{\n  \"root\": false,\n  \"files\": { \"includes\": [\"!**\"] }\n}\n";
    fs::write(defaults.join(".loom").join("biome.jsonc"), biome_config).unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");

    let installed = workspace.join(".loom").join("biome.jsonc");
    assert!(
        installed.exists(),
        ".loom/biome.jsonc should be copied from defaults/.loom/biome.jsonc"
    );
    assert_eq!(fs::read_to_string(&installed).unwrap(), biome_config);

    let report = result.unwrap();
    assert!(
        report.added.contains(&".loom/biome.jsonc".to_string()),
        "Report should list .loom/biome.jsonc as added, got: {:?}",
        report.added
    );
}

#[test]
fn test_reinstall_removes_stale_files() {
    // Verifies that files in destination but not in source are removed on reinstall
    // This is the core behavior change for issue #1798
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    // Setup git repo
    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with scripts (simulates Python port: old shell scripts removed)
    fs::create_dir_all(defaults.join("scripts")).unwrap();
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("scripts").join("worktree.sh"), "#!/bin/bash\n# kept").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder role").unwrap();

    // Create existing .loom with stale scripts (simulates pre-port state)
    fs::create_dir_all(workspace.join(".loom").join("scripts")).unwrap();
    fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
    fs::write(
        workspace.join(".loom").join("scripts").join("worktree.sh"),
        "#!/bin/bash\n# old",
    )
    .unwrap();
    fs::write(
        workspace
            .join(".loom")
            .join("scripts")
            .join("validate-phase.sh"),
        "#!/bin/bash\n# ported to python",
    )
    .unwrap();
    fs::write(
        workspace
            .join(".loom")
            .join("scripts")
            .join("agent-metrics.sh"),
        "#!/bin/bash\n# ported to python",
    )
    .unwrap();
    fs::write(workspace.join(".loom").join("roles").join("builder.md"), "old builder").unwrap();
    fs::write(workspace.join(".loom").join("roles").join("obsolete.md"), "removed role").unwrap();
    // #5971: the sweep retires a file only when something attributes it to
    // Loom — here, the previous install's own record.
    write_prev_manifest(
        workspace,
        &[
            ".loom/scripts/validate-phase.sh",
            ".loom/scripts/agent-metrics.sh",
            ".loom/roles/obsolete.md",
        ],
    );

    // Run reinstall
    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok());
    let report = result.unwrap();

    // Stale scripts should be removed
    assert!(
        !workspace
            .join(".loom")
            .join("scripts")
            .join("validate-phase.sh")
            .exists(),
        "validate-phase.sh should have been removed"
    );
    assert!(
        !workspace
            .join(".loom")
            .join("scripts")
            .join("agent-metrics.sh")
            .exists(),
        "agent-metrics.sh should have been removed"
    );

    // Stale role should be removed
    assert!(
        !workspace
            .join(".loom")
            .join("roles")
            .join("obsolete.md")
            .exists(),
        "obsolete.md should have been removed"
    );

    // Current files should exist with fresh content
    let worktree =
        fs::read_to_string(workspace.join(".loom").join("scripts").join("worktree.sh")).unwrap();
    assert_eq!(worktree, "#!/bin/bash\n# kept");

    let builder =
        fs::read_to_string(workspace.join(".loom").join("roles").join("builder.md")).unwrap();
    assert_eq!(builder, "builder role");

    // Report should track removals
    assert!(report
        .removed
        .contains(&".loom/scripts/validate-phase.sh".to_string()));
    assert!(report
        .removed
        .contains(&".loom/scripts/agent-metrics.sh".to_string()));
    assert!(report
        .removed
        .contains(&".loom/roles/obsolete.md".to_string()));
}

/// Write the previous install's ownership record — the same
/// `.loom/install-metadata.json` shape `scripts/install-loom.sh` writes.
/// Issue #5971: the reinstall clean sweep only retires a file this record
/// (or the current `defaults/` tree) attributes to Loom, so a test that
/// asserts a stale file is removed must say Loom installed it.
fn write_prev_manifest(workspace: &Path, installed: &[&str]) {
    let loom = workspace.join(".loom");
    fs::create_dir_all(&loom).unwrap();
    let json = serde_json::json!({
        "loom_version": "0.0.0-test",
        "installed_files": installed,
    });
    fs::write(loom.join("install-metadata.json"), serde_json::to_string_pretty(&json).unwrap())
        .unwrap();
}

#[test]
fn test_reinstall_preserves_unmanaged_hook_file() {
    // Issue #5971 (the reported incident): a consumer repo's own
    // `.loom/hooks/post-worktree.sh` — a documented extension point Loom
    // invokes from worktree.sh, and one that is DELIBERATELY excluded from
    // the installed-files manifest (scripts/install/manifest.sh, #4262) —
    // was deleted by the reinstall clean sweep. The hook then silently
    // stopped firing. It must survive, byte-for-byte, and be reported.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join("hooks")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("hooks").join("guard-destructive.sh"), "#!/bin/bash\n# guard v2")
        .unwrap();

    // Previous install: hooks/* is never recorded, exactly as in production.
    write_prev_manifest(workspace, &[".loom/scripts/worktree.sh"]);

    let hooks = workspace.join(".loom").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("guard-destructive.sh"), "#!/bin/bash\n# guard v1").unwrap();
    let repo_hook = "#!/bin/bash\n# REPO-OWNED project hook\nuv sync --frozen --extra dev\n";
    fs::write(hooks.join("post-worktree.sh"), repo_hook).unwrap();

    let report =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("init should succeed");

    // The repo-owned hook survives untouched.
    assert!(
        hooks.join("post-worktree.sh").exists(),
        "repo-owned hook must survive a reinstall"
    );
    assert_eq!(
        fs::read_to_string(hooks.join("post-worktree.sh")).unwrap(),
        repo_hook,
        "repo-owned hook content must be byte-for-byte unchanged"
    );

    // …and the outcome is reported, not silent.
    assert!(
        report
            .preserved_unmanaged
            .contains(&".loom/hooks/post-worktree.sh".to_string()),
        "preservation must be reported, got: {:?}",
        report.preserved_unmanaged
    );
    assert!(
        !report
            .removed
            .contains(&".loom/hooks/post-worktree.sh".to_string()),
        "repo-owned hook must not appear in the removal list"
    );

    // No regression: a Loom-shipped hook is still refreshed.
    assert_eq!(
        fs::read_to_string(hooks.join("guard-destructive.sh")).unwrap(),
        "#!/bin/bash\n# guard v2"
    );
}

#[test]
fn test_reinstall_preserves_file_pinned_in_resync_ignore() {
    // Issue #5971 AC #3: `.loom/resync-ignore` is the declared-ownership
    // mechanism. A path pinned there is repo-owned and survives even when
    // the previous install's manifest claims Loom wrote it (a stale
    // over-broad manifest must not be able to reach a declared file).
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join("scripts")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("scripts").join("worktree.sh"), "shipped").unwrap();

    write_prev_manifest(workspace, &[".loom/scripts/project-local.sh"]);
    fs::write(
        workspace.join(".loom").join("resync-ignore"),
        "# repo-owned, do not delete\nscripts/project-local.sh\n",
    )
    .unwrap();

    let scripts = workspace.join(".loom").join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::write(scripts.join("project-local.sh"), "repo-owned body").unwrap();

    let report =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("init should succeed");

    assert!(
        scripts.join("project-local.sh").exists(),
        "a path pinned in .loom/resync-ignore must never be deleted"
    );
    assert_eq!(fs::read_to_string(scripts.join("project-local.sh")).unwrap(), "repo-owned body");
    assert!(
        report
            .preserved_repo_owned
            .contains(&".loom/scripts/project-local.sh".to_string()),
        "declared repo-owned files are reported separately, got: {:?}",
        report.preserved_repo_owned
    );
}

#[test]
fn test_reinstall_preserves_unmanaged_file_in_a_subdirectory() {
    // The sweep recurses; the same ownership boundary must hold one level
    // down, and the surviving file must keep its parent directory alive
    // (the old code called remove_dir_all on any dest-only directory).
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join("scripts")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("scripts").join("worktree.sh"), "shipped").unwrap();

    write_prev_manifest(workspace, &[".loom/scripts/worktree.sh"]);

    let local = workspace.join(".loom").join("scripts").join("project");
    fs::create_dir_all(&local).unwrap();
    fs::write(local.join("build.sh"), "repo-owned nested").unwrap();

    let report =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("init should succeed");

    assert!(
        local.join("build.sh").exists(),
        "nested repo-owned file must survive, and keep its directory"
    );
    assert!(report
        .preserved_unmanaged
        .contains(&".loom/scripts/project/build.sh".to_string()));
}

#[test]
fn test_reinstall_still_removes_a_file_the_previous_manifest_recorded() {
    // The other half of #5971: ownership-gating must not neuter the sweep.
    // A file the previous install RECORDED as Loom-installed is still
    // retired on reinstall even though the current defaults/ dropped it.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = workspace.join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join("scripts")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("scripts").join("worktree.sh"), "shipped").unwrap();

    write_prev_manifest(workspace, &[".loom/scripts/retired-by-upstream.sh"]);

    let scripts = workspace.join(".loom").join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::write(scripts.join("retired-by-upstream.sh"), "old loom script").unwrap();

    let report =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("init should succeed");

    assert!(
        !scripts.join("retired-by-upstream.sh").exists(),
        "a recorded Loom file dropped upstream must still be retired"
    );
    assert!(report
        .removed
        .contains(&".loom/scripts/retired-by-upstream.sh".to_string()));
}

#[test]
fn test_init_report_includes_verification() {
    // Full initialization should include verification with no failures
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("scripts")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(defaults.join("scripts").join("test.sh"), "#!/bin/bash").unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok());
    let report = result.unwrap();

    // Fresh install should have zero verification failures
    assert!(
        report.verification_failures.is_empty(),
        "Expected no verification failures, got: {:?}",
        report.verification_failures
    );
}

#[test]
fn test_hooks_installed_on_fresh_install() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with hooks
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("hooks")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(defaults.join("hooks").join("guard-destructive.sh"), "#!/bin/bash\n# guard hook")
        .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok());
    let report = result.unwrap();

    // Hook should be installed
    let hook_path = workspace
        .join(".loom")
        .join("hooks")
        .join("guard-destructive.sh");
    assert!(hook_path.exists(), "Hook file should be installed");
    let content = fs::read_to_string(&hook_path).unwrap();
    assert_eq!(content, "#!/bin/bash\n# guard hook");

    // Report should list the hook as added
    assert!(report
        .added
        .contains(&".loom/hooks/guard-destructive.sh".to_string()));
}

#[test]
fn test_hooks_preserved_on_reinstall() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with hooks
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("hooks")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(
        defaults.join("hooks").join("guard-destructive.sh"),
        "#!/bin/bash\n# updated guard hook v2",
    )
    .unwrap();

    // Simulate existing installation with old hook
    fs::create_dir_all(workspace.join(".loom").join("hooks")).unwrap();
    fs::write(
        workspace
            .join(".loom")
            .join("hooks")
            .join("guard-destructive.sh"),
        "#!/bin/bash\n# old guard hook v1",
    )
    .unwrap();

    // Run reinstall
    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok());

    // Hook should have new content (clean-then-copy)
    let hook_path = workspace
        .join(".loom")
        .join("hooks")
        .join("guard-destructive.sh");
    assert!(hook_path.exists(), "Hook file should exist after reinstall");
    let content = fs::read_to_string(&hook_path).unwrap();
    assert_eq!(content, "#!/bin/bash\n# updated guard hook v2");
}

#[test]
fn test_scripts_lib_subdirectory_copied_on_fresh_install() {
    // Verifies that scripts/lib/ subdirectory (containing loom-tools.sh
    // and pipe-pane-cmd.sh) is correctly copied during initialization.
    // This is the specific scenario reported in issue #2392.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults mirroring real structure with scripts/lib/
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(
        defaults.join("scripts").join("agent-spawn.sh"),
        "#!/bin/bash\nsource \"$SCRIPT_DIR/lib/loom-tools.sh\"",
    )
    .unwrap();
    fs::write(
        defaults.join("scripts").join("lib").join("loom-tools.sh"),
        "#!/bin/bash\n# shared helper library",
    )
    .unwrap();
    fs::write(
        defaults
            .join("scripts")
            .join("lib")
            .join("pipe-pane-cmd.sh"),
        "#!/bin/bash\n# pipe pane command",
    )
    .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok());
    let report = result.unwrap();

    // Verify scripts/lib/ subdirectory exists
    let lib_dir = workspace.join(".loom").join("scripts").join("lib");
    assert!(lib_dir.exists(), "scripts/lib/ directory should exist");
    assert!(lib_dir.is_dir(), "scripts/lib/ should be a directory");

    // Verify both lib files were copied
    let loom_tools = lib_dir.join("loom-tools.sh");
    assert!(loom_tools.exists(), "lib/loom-tools.sh should exist");
    let content = fs::read_to_string(&loom_tools).unwrap();
    assert_eq!(content, "#!/bin/bash\n# shared helper library");

    let pipe_pane = lib_dir.join("pipe-pane-cmd.sh");
    assert!(pipe_pane.exists(), "lib/pipe-pane-cmd.sh should exist");

    // Verify the parent script was also copied
    let agent_spawn = workspace
        .join(".loom")
        .join("scripts")
        .join("agent-spawn.sh");
    assert!(agent_spawn.exists(), "agent-spawn.sh should exist");

    // Verify report includes subdirectory files
    assert!(
        report
            .added
            .contains(&".loom/scripts/lib/loom-tools.sh".to_string()),
        "Report should include lib/loom-tools.sh, got: {:?}",
        report.added
    );
    assert!(
        report
            .added
            .contains(&".loom/scripts/lib/pipe-pane-cmd.sh".to_string()),
        "Report should include lib/pipe-pane-cmd.sh, got: {:?}",
        report.added
    );

    // Verify no verification failures
    assert!(
        report.verification_failures.is_empty(),
        "Expected no verification failures, got: {:?}",
        report.verification_failures
    );
}

#[test]
fn test_scripts_lib_subdirectory_restored_on_reinstall() {
    // On reinstall, scripts/lib/ should be cleaned and re-copied.
    // This tests the case where lib/ existed but with stale content.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with scripts/lib/
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(
        defaults.join("scripts").join("lib").join("loom-tools.sh"),
        "#!/bin/bash\n# v2 helper",
    )
    .unwrap();

    // Simulate existing installation with old lib/ content and a stale file
    fs::create_dir_all(workspace.join(".loom").join("scripts").join("lib")).unwrap();
    fs::write(
        workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("loom-tools.sh"),
        "#!/bin/bash\n# v1 helper (old)",
    )
    .unwrap();
    fs::write(
        workspace
            .join(".loom")
            .join("scripts")
            .join("lib")
            .join("obsolete.sh"),
        "#!/bin/bash\n# should be removed",
    )
    .unwrap();
    // #5971: the sweep retires a file only when something attributes it to
    // Loom — here, the previous install's own record.
    write_prev_manifest(workspace, &[".loom/scripts/lib/obsolete.sh"]);

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok());
    let report = result.unwrap();

    // lib/loom-tools.sh should have new content
    let loom_tools = workspace
        .join(".loom")
        .join("scripts")
        .join("lib")
        .join("loom-tools.sh");
    let content = fs::read_to_string(&loom_tools).unwrap();
    assert_eq!(content, "#!/bin/bash\n# v2 helper");

    // Stale file should be removed
    let obsolete = workspace
        .join(".loom")
        .join("scripts")
        .join("lib")
        .join("obsolete.sh");
    assert!(!obsolete.exists(), "Stale file in lib/ should be removed on reinstall");

    // Report should track the removal
    assert!(
        report
            .removed
            .contains(&".loom/scripts/lib/obsolete.sh".to_string()),
        "Report should list obsolete.sh as removed, got: {:?}",
        report.removed
    );
}

#[test]
fn test_docs_subdirectory_copied_on_fresh_install() {
    // Regression-guard for issue #3470: the `.loom/docs/` managed
    // directory (containing static reference docs like
    // `ci-integration.md` from issue #3333) must be copied during
    // initialization. The line-169 `sync_managed_dir(..., "docs", ...)`
    // call already exists on main — this test prevents the entry from
    // silently being deleted or refactored away in the future, which
    // would re-introduce the v0.10 field failure where consumers got
    // `MISSING: .loom/docs/ci-integration.md` from the post-install
    // metadata verification (the #3287 safety net).
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults mirroring the real shipped shape: docs live at
    // `defaults/docs/` alongside `defaults/roles/` (issue #3476 moved
    // them there from `defaults/.loom/docs/`, which `sync_managed_dir`
    // never looked at). The companion test
    // `test_real_defaults_tree_ships_docs_at_top_level` pins the actual
    // shipped tree to this layout so the two can't silently diverge.
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("docs")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(
        defaults.join("docs").join("ci-integration.md"),
        "# CI Integration\n\nStatic reference documentation.",
    )
    .unwrap();
    // A second doc, to confirm we copy the whole directory and not
    // just one named file.
    fs::write(
        defaults.join("docs").join("troubleshooting.md"),
        "# Troubleshooting\n\nMore docs.",
    )
    .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok(), "init failed: {:?}", result.err());
    let report = result.unwrap();

    // The `.loom/docs/` directory must exist on disk post-init.
    let docs_dir = workspace.join(".loom").join("docs");
    assert!(docs_dir.exists(), ".loom/docs/ directory should exist");
    assert!(docs_dir.is_dir(), ".loom/docs/ should be a directory");

    // The specific file the field failure flagged must be present.
    let ci_md = docs_dir.join("ci-integration.md");
    assert!(
        ci_md.exists(),
        ".loom/docs/ci-integration.md should exist (this is the file the \
             v0.10 install regression reported as MISSING in issue #3470)"
    );
    let content = fs::read_to_string(&ci_md).unwrap();
    assert_eq!(content, "# CI Integration\n\nStatic reference documentation.");

    // The sibling doc must also be present (whole-directory copy).
    let troubleshooting = docs_dir.join("troubleshooting.md");
    assert!(troubleshooting.exists(), ".loom/docs/troubleshooting.md should exist");

    // Report bookkeeping: both docs files should be tracked as added.
    assert!(
        report
            .added
            .contains(&".loom/docs/ci-integration.md".to_string()),
        "Report should include docs/ci-integration.md, got: {:?}",
        report.added
    );
    assert!(
        report
            .added
            .contains(&".loom/docs/troubleshooting.md".to_string()),
        "Report should include docs/troubleshooting.md, got: {:?}",
        report.added
    );

    // No verification failures: the fail-fast assertion inside
    // sync_managed_dir (the #3220/#3287 safety net) should be quiet,
    // and the post-copy `verify_all_copied_files` walk over the docs
    // dir should not produce any content mismatches.
    assert!(
        report.verification_failures.is_empty(),
        "Expected no verification failures, got: {:?}",
        report.verification_failures
    );
}

#[test]
fn test_docs_subdirectory_restored_on_reinstall() {
    // Reinstall analog of test_docs_subdirectory_copied_on_fresh_install:
    // the docs dir must be cleaned and re-copied so stale files are
    // removed and updated content lands. This pins the #3470 fix on
    // the reinstall path (which is the path the field failure
    // exercised — Studio was upgrading v0.9 -> v0.10).
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Defaults with updated docs.
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("docs")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(defaults.join("docs").join("ci-integration.md"), "# CI Integration v2").unwrap();

    // Pre-existing install with stale content + a stale file.
    fs::create_dir_all(workspace.join(".loom").join("docs")).unwrap();
    fs::write(
        workspace
            .join(".loom")
            .join("docs")
            .join("ci-integration.md"),
        "# CI Integration v1 (old)",
    )
    .unwrap();
    fs::write(workspace.join(".loom").join("docs").join("obsolete-doc.md"), "stale").unwrap();
    // #5971: the sweep retires a file only when something attributes it to
    // Loom — here, the previous install's own record.
    write_prev_manifest(workspace, &[".loom/docs/obsolete-doc.md"]);

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok(), "reinstall failed: {:?}", result.err());
    let report = result.unwrap();

    // Updated file has new content.
    let ci_md = workspace
        .join(".loom")
        .join("docs")
        .join("ci-integration.md");
    let content = fs::read_to_string(&ci_md).unwrap();
    assert_eq!(content, "# CI Integration v2");

    // Stale file removed.
    let obsolete = workspace.join(".loom").join("docs").join("obsolete-doc.md");
    assert!(!obsolete.exists(), "Stale docs file should be removed on reinstall");
    assert!(
        report
            .removed
            .contains(&".loom/docs/obsolete-doc.md".to_string()),
        "Report should list obsolete-doc.md as removed, got: {:?}",
        report.removed
    );
}

#[test]
fn test_runtimes_subdirectory_copied_on_fresh_install() {
    // Regression-guard for #4688: `.loom/runtimes/` (the per-runtime
    // capability manifests `runtime_admission::roots()` reads) must be
    // copied during initialization, mirroring the `docs` regression
    // guard above (#3470). Before this fix `sync_managed_dir` was never
    // called for "runtimes" at all, so every fresh `loom-daemon init`
    // left `.loom/runtimes/` unpopulated and the admission gate fell
    // through to a nonexistent `defaults/runtimes/...` on every
    // dispatch.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("runtimes")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(
        defaults.join("runtimes").join("claude.json"),
        r#"{"runtime":"claude","capabilities":{"mcp":"yes"}}"#,
    )
    .unwrap();
    fs::write(
        defaults.join("runtimes").join("codex.json"),
        r#"{"runtime":"codex","capabilities":{"mcp":"yes"}}"#,
    )
    .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok(), "init failed: {:?}", result.err());
    let report = result.unwrap();

    let runtimes_dir = workspace.join(".loom").join("runtimes");
    assert!(runtimes_dir.exists(), ".loom/runtimes/ directory should exist");
    assert!(runtimes_dir.is_dir(), ".loom/runtimes/ should be a directory");

    let claude_json = runtimes_dir.join("claude.json");
    assert!(claude_json.exists(), ".loom/runtimes/claude.json should exist");
    assert_eq!(
        fs::read_to_string(&claude_json).unwrap(),
        r#"{"runtime":"claude","capabilities":{"mcp":"yes"}}"#
    );
    assert!(
        runtimes_dir.join("codex.json").exists(),
        ".loom/runtimes/codex.json should exist (whole-directory copy)"
    );

    assert!(
        report
            .added
            .contains(&".loom/runtimes/claude.json".to_string()),
        "Report should include runtimes/claude.json, got: {:?}",
        report.added
    );
    assert!(
        report.verification_failures.is_empty(),
        "Expected no verification failures, got: {:?}",
        report.verification_failures
    );
}

#[test]
fn test_runtimes_subdirectory_restored_on_reinstall() {
    // Reinstall analog of test_runtimes_subdirectory_copied_on_fresh_install:
    // stale runtime manifests must be cleaned and updated content copied
    // fresh, and — critically — a workspace that NEVER had
    // `.loom/runtimes/` at all (the exact #4688 incident layout) must
    // have it backfilled by a reinstall, not silently skipped.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("runtimes")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(
        defaults.join("runtimes").join("claude.json"),
        r#"{"runtime":"claude","capabilities":{"mcp":"yes"}}"#,
    )
    .unwrap();

    // Pre-existing install that predates #4688: `.loom/roles/` exists
    // but `.loom/runtimes/` was never provisioned at all.
    fs::create_dir_all(workspace.join(".loom").join("roles")).unwrap();
    assert!(!workspace.join(".loom").join("runtimes").exists());

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok(), "reinstall failed: {:?}", result.err());

    let claude_json = workspace.join(".loom").join("runtimes").join("claude.json");
    assert!(
        claude_json.exists(),
        ".loom/runtimes/claude.json should be backfilled by reinstall even though \
             .loom/runtimes/ never existed before"
    );
    assert_eq!(
        fs::read_to_string(&claude_json).unwrap(),
        r#"{"runtime":"claude","capabilities":{"mcp":"yes"}}"#
    );
}

#[test]
fn test_real_defaults_tree_ships_docs_at_top_level() {
    // Regression guard for #3476 Bug 2. The tempdir tests above fabricate
    // a defaults/ layout, so they structurally cannot catch the failure
    // mode where the SHIPPED tree diverges from what `sync_managed_dir`
    // expects: v0.10.0 shipped docs at `defaults/.loom/docs/` while
    // `sync_managed_dir(&defaults, ..., "docs", ...)` resolved
    // `defaults/docs/`, so the copy silently no-oped and real installs
    // failed the #3287 metadata check with
    // `MISSING: .loom/docs/ci-integration.md`.
    //
    // This test runs against the actual repository tree (resolved via
    // CARGO_MANIFEST_DIR) and asserts every managed dir that
    // `initialize_workspace` syncs — including docs — exists at the
    // top level of defaults/ where `sync_managed_dir` will find it.
    let defaults = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ has a parent")
        .join("defaults");
    assert!(
        defaults.is_dir(),
        "shipped defaults/ tree not found at {defaults:?} — did the repo layout change?"
    );

    for dir_name in &["roles", "scripts", "hooks", "docs", "runtimes"] {
        let managed = defaults.join(dir_name);
        assert!(
            managed.is_dir(),
            "defaults/{dir_name}/ is missing — sync_managed_dir(\"{dir_name}\") would \
                 silently no-op and installs would diverge from the manifest (#3476)"
        );
    }

    // The specific file the field failure flagged.
    assert!(
        defaults.join("docs").join("ci-integration.md").is_file(),
        "defaults/docs/ci-integration.md missing — the #3287 metadata guard would \
             report it MISSING on every install (#3476)"
    );

    // Reference docs extracted from the retired root `defaults/CLAUDE.md`
    // template in #4143 (Phase 2 of #4052); that template was deleted in
    // Phase 3 (#4144). They must ship from defaults/docs/ so live
    // cross-references (CLAUDE.md guard catalog,
    // docs/model-selection-retune.md) do not orphan.
    for doc in &[
        "guard-hooks.md",
        "model-selection.md",
        "model-cost-experiment.md",
        "health-monitoring.md",
        "advanced-hooks.md",
    ] {
        assert!(
            defaults.join("docs").join(doc).is_file(),
            "defaults/docs/{doc} missing — the #4143 reference-doc extraction \
                 must ship it to <target>/.loom/docs/ or its cross-reference dangles"
        );
    }

    // The old nested location must stay gone: a file reappearing at
    // defaults/.loom/docs/ would be manifest-listed (via the `.loom/*`
    // literal rule in scripts/install/manifest.sh) but never copied by
    // sync_managed_dir — the exact divergence this issue fixed.
    assert!(
        !defaults.join(".loom").join("docs").exists(),
        "defaults/.loom/docs/ has reappeared — docs must live at defaults/docs/ \
             so sync_managed_dir copies them (#3476)"
    );
}

/// Extract markdown link/image targets (`[text](target)` / `![alt](target)`)
/// from `content`. A minimal parser sufficient for the link shapes used in
/// CLAUDE.md/AGENTS.md (no nested parens inside a target) — mirrors the
/// approach `scripts/check-dangling-links.sh` uses for the same purpose.
fn extract_markdown_link_targets(content: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel_open) = content[search_from..].find("](") {
        let open = search_from + rel_open + 2;
        let Some(rel_close) = content[open..].find(')') else {
            break;
        };
        targets.push(content[open..open + rel_close].to_string());
        search_from = open + rel_close + 1;
    }
    targets
}

#[test]
fn test_real_defaults_claude_md_links_resolve_after_install() {
    // Issue #5975: every relative markdown link target in
    // defaults/.loom/CLAUDE.md is authored resolving from repo root, but
    // the FULL template is also installed verbatim to `.loom/CLAUDE.md`
    // itself — one directory level deeper — where an un-rebased target
    // 404s (e.g. `.loom/docs/daemon-reference.md` resolves to the
    // nonexistent `.loom/.loom/docs/daemon-reference.md`).
    //
    // This runs the REAL installer against the actual shipped
    // `defaults/` tree (not a synthetic fixture, resolved via
    // CARGO_MANIFEST_DIR — same pattern as
    // test_real_defaults_tree_ships_docs_at_top_level above) into a
    // scratch workspace, then walks every markdown link target in the
    // resulting `.loom/CLAUDE.md` and asserts it resolves to a real file
    // relative to `.loom/CLAUDE.md`'s own directory — i.e. a genuine
    // "link check over an installed tree" (issue #5975's AC #2).
    let defaults = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ has a parent")
        .join("defaults");
    assert!(
        defaults.is_dir(),
        "shipped defaults/ tree not found at {defaults:?} — did the repo layout change?"
    );

    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    fs::create_dir(workspace.join(".git")).unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init against real defaults/ failed: {:?}", result.err());

    let claude_md_path = workspace.join(".loom").join("CLAUDE.md");
    assert!(claude_md_path.exists(), ".loom/CLAUDE.md should be installed");
    let content = fs::read_to_string(&claude_md_path).unwrap();

    // Sanity: the old, broken `.loom/docs/...`-from-`.loom/CLAUDE.md`
    // link-target form must be fully gone.
    assert!(
        !content.contains("](.loom/"),
        ".loom/CLAUDE.md must not contain unrewritten `.loom/...` link targets, got: {content}"
    );

    let targets = extract_markdown_link_targets(&content);
    assert!(
        targets.iter().any(|t| t.starts_with("docs/")),
        "expected at least one localized docs/... link target, got: {targets:?}"
    );

    let claude_md_dir = claude_md_path.parent().unwrap();
    for target in &targets {
        if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("mailto:")
            || target.starts_with('#')
        {
            continue;
        }
        let path_part = target.split('#').next().unwrap_or(target);
        if path_part.is_empty() {
            continue;
        }
        let resolved = claude_md_dir.join(path_part);
        assert!(
            resolved.exists(),
            ".loom/CLAUDE.md link target {target:?} does not resolve to an existing file at \
                 {resolved:?} — a repo-root-relative link leaked into the .loom/CLAUDE.md copy \
                 unrewritten (issue #5975)"
        );
    }

    // .loom/AGENTS.md must pass the same check (issue #5975 AC #3) — a
    // no-op today since it has zero markdown links, but this guards
    // against a future edit silently reintroducing the same bug class.
    let agents_md_path = workspace.join(".loom").join("AGENTS.md");
    assert!(agents_md_path.exists(), ".loom/AGENTS.md should be installed");
    let agents_content = fs::read_to_string(&agents_md_path).unwrap();
    assert!(
        !agents_content.contains("](.loom/"),
        ".loom/AGENTS.md must not contain unrewritten `.loom/...` link targets"
    );
    let agents_md_dir = agents_md_path.parent().unwrap();
    for target in extract_markdown_link_targets(&agents_content) {
        if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("mailto:")
            || target.starts_with('#')
        {
            continue;
        }
        let path_part = target.split('#').next().unwrap_or(&target);
        if path_part.is_empty() {
            continue;
        }
        let resolved = agents_md_dir.join(path_part);
        assert!(
            resolved.exists(),
            ".loom/AGENTS.md link target {target:?} does not resolve to an existing file at \
                 {resolved:?}"
        );
    }

    // Root CLAUDE.md only ever receives the short pointer text (no
    // `docs/`-relative links), so the rewrite must not have touched it —
    // confirm no localized `docs/` targets leaked into the root copy.
    let root_claude_md = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
    assert!(
        !root_claude_md.contains("](docs/"),
        "root CLAUDE.md must not contain rewritten docs/ targets — it only carries the \
             short pointer, and if it ever did carry doc links they'd need the ORIGINAL \
             .loom/docs/... form to resolve from repo root"
    );
}

#[test]
fn test_filter_preserved_from_verification_failures_removes_preserved() {
    // Files preserved by merge strategy must not appear as verification failures
    // (this is the regression case from issue #3218).
    let mut report = InitReport {
        preserved: vec![
            ".claude/settings.json".to_string(),
            ".github/labels.yml".to_string(),
        ],
        verification_failures: vec![
            ".claude/settings.json (content mismatch: source 100 bytes, installed 200 bytes)"
                .to_string(),
            ".github/labels.yml (content mismatch: source 50 bytes, installed 75 bytes)"
                .to_string(),
            ".loom/scripts/genuine.sh (content mismatch: source 10 bytes, installed 20 bytes)"
                .to_string(),
        ],
        ..Default::default()
    };

    filter_preserved_from_verification_failures(&mut report);

    // Only the genuine non-preserved failure should remain
    assert_eq!(report.verification_failures.len(), 1);
    assert!(report.verification_failures[0].contains(".loom/scripts/genuine.sh"));
}

#[test]
fn test_filter_preserved_from_verification_failures_no_preserved() {
    // When nothing is preserved, all failures pass through unchanged
    let mut report = InitReport {
        preserved: vec![],
        verification_failures: vec![".loom/scripts/foo.sh (content mismatch)".to_string()],
        ..Default::default()
    };
    filter_preserved_from_verification_failures(&mut report);
    assert_eq!(report.verification_failures.len(), 1);
}

#[test]
fn test_filter_preserved_from_verification_failures_no_failures() {
    // No-op when there are no failures, even if there are preserved files
    let mut report = InitReport {
        preserved: vec![".claude/settings.json".to_string()],
        verification_failures: vec![],
        ..Default::default()
    };
    filter_preserved_from_verification_failures(&mut report);
    assert!(report.verification_failures.is_empty());
}

#[test]
fn test_preserved_files_excluded_from_verification_failures_end_to_end() {
    // End-to-end: a consumer .github/labels.yml is block-merged on reinstall
    // (issue #4187) — its Loom BEGIN/END LOOM LABELS block is refreshed while
    // consumer-authored labels outside the block survive. The resulting file
    // differs from the shipped source, so it must be recorded as `preserved`
    // and must NOT surface as a verification failure. This also covers the
    // original issue #3218 regression (preserved file leaking into failures).
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Minimal defaults
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();

    // Shipped .github/labels.yml with a Loom-managed marker block.
    fs::create_dir_all(defaults.join(".github")).unwrap();
    fs::write(
        defaults.join(".github").join("labels.yml"),
        "# BEGIN LOOM LABELS\n- name: loom:issue\n  color: ffffff\n# END LOOM LABELS\n",
    )
    .unwrap();

    // Pre-existing consumer .github/labels.yml: a stale Loom block plus a
    // consumer-authored label OUTSIDE the block.
    fs::create_dir_all(workspace.join(".github")).unwrap();
    fs::write(
            workspace.join(".github").join("labels.yml"),
            "# BEGIN LOOM LABELS\n- name: loom:issue\n  color: 000000\n# END LOOM LABELS\n\n- name: team:frontend\n  color: 00ff00\n  description: consumer label\n",
        )
        .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok());
    let report = result.unwrap();

    // The block-merged file must be reported as preserved (consumer-owned).
    assert!(
        report.preserved.contains(&".github/labels.yml".to_string()),
        "preserved should contain .github/labels.yml, got: {:?}",
        report.preserved
    );

    // The Loom block was refreshed to the shipped color; the consumer label
    // outside the block survived untouched.
    let installed = fs::read_to_string(workspace.join(".github").join("labels.yml")).unwrap();
    assert!(
        installed.contains("color: ffffff"),
        "Loom block should be refreshed: {installed}"
    );
    assert!(
        installed.contains("- name: team:frontend"),
        "consumer label must survive: {installed}"
    );

    // And it must NOT appear as a verification failure (issue #3218).
    let leaked: Vec<&String> = report
        .verification_failures
        .iter()
        .filter(|f| f.contains(".github/labels.yml"))
        .collect();
    assert!(
        leaked.is_empty(),
        "preserved file leaked into verification_failures: {:?}",
        report.verification_failures
    );
}

#[test]
fn test_settings_json_co_owner_merge_excluded_from_verification_failures_end_to_end() {
    // End-to-end regression for issue #5396: when another tool (e.g. Repo
    // Skills, github.com/rjwalters/repo) already owns .claude/settings.json
    // via its own PreToolUse/SessionStart hooks, Loom's install deep-merges
    // its own hook/permission defaults into that file rather than
    // overwriting it. The merged file is legitimately larger/different from
    // the shipped source — that divergence must be recorded as `preserved`
    // and must NOT surface as an "unexpected file divergence" verification
    // failure.
    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Minimal defaults
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();

    // Shipped .claude/settings.json with a Loom-owned PreToolUse hook.
    fs::create_dir_all(defaults.join(".claude")).unwrap();
    fs::write(
        defaults.join(".claude").join("settings.json"),
        r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": ".loom/hooks/guard-destructive-generic.sh" }
        ]
      }
    ]
  },
  "permissions": {
    "allow": ["Bash(git status:*)"]
  }
}"#,
    )
    .unwrap();

    // Pre-existing consumer .claude/settings.json owned by Repo Skills: a
    // SessionStart hook Loom does not define at all, plus a co-owned
    // PreToolUse matcher with a foreign command Loom's merge must preserve
    // alongside its own.
    fs::create_dir_all(workspace.join(".claude")).unwrap();
    fs::write(
        workspace.join(".claude").join("settings.json"),
        r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "repo-skills/hooks/pre-tool-use.sh" }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "",
        "hooks": [
          { "type": "command", "command": "repo-skills/hooks/session-start.sh" }
        ]
      }
    ]
  },
  "permissions": {
    "allow": ["Bash(gh pr view:*)"]
  }
}"#,
    )
    .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok());
    let report = result.unwrap();

    // The merged file must be reported as preserved (consumer/co-owner-owned).
    assert!(
        report
            .preserved
            .contains(&".claude/settings.json".to_string()),
        "preserved should contain .claude/settings.json, got: {:?}",
        report.preserved
    );

    // Should not also be double-recorded as added/updated by the preceding
    // directory copy.
    assert!(
        !report.added.contains(&".claude/settings.json".to_string()),
        "settings.json must not also appear in added: {:?}",
        report.added
    );
    assert!(
        !report
            .updated
            .contains(&".claude/settings.json".to_string()),
        "settings.json must not also appear in updated: {:?}",
        report.updated
    );

    // Both tools' hooks must survive the merge.
    let installed = fs::read_to_string(workspace.join(".claude").join("settings.json")).unwrap();
    assert!(
        installed.contains("repo-skills/hooks/session-start.sh"),
        "Repo Skills' SessionStart hook must survive: {installed}"
    );
    assert!(
        installed.contains("repo-skills/hooks/pre-tool-use.sh"),
        "Repo Skills' PreToolUse hook must survive: {installed}"
    );
    assert!(
        installed.contains("guard-destructive-generic.sh"),
        "Loom's own PreToolUse hook must survive: {installed}"
    );

    // And it must NOT appear as a verification failure (the bug in #5396).
    let leaked: Vec<&String> = report
        .verification_failures
        .iter()
        .filter(|f| f.contains(".claude/settings.json"))
        .collect();
    assert!(
        leaked.is_empty(),
        "co-owned settings.json merge leaked into verification_failures: {:?}",
        report.verification_failures
    );
}

#[cfg(unix)]
#[test]
fn test_scripts_made_executable_including_subdirectories() {
    // Verifies that make_shell_scripts_executable works recursively
    // on scripts/ and its subdirectories (e.g., scripts/lib/).
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = TempDir::new().unwrap();
    let workspace = temp_dir.path();
    let defaults = temp_dir.path().join("defaults");

    fs::create_dir(workspace.join(".git")).unwrap();

    // Create defaults with scripts that are NOT executable
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
    fs::write(defaults.join("config.json"), "{}").unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    fs::write(defaults.join("scripts").join("worktree.sh"), "#!/bin/bash\n# worktree helper")
        .unwrap();
    fs::write(
        defaults.join("scripts").join("lib").join("loom-tools.sh"),
        "#!/bin/bash\n# shared helper",
    )
    .unwrap();

    // Remove execute bit from source files to simulate git clone stripping perms
    for path in &[
        defaults.join("scripts").join("worktree.sh"),
        defaults.join("scripts").join("lib").join("loom-tools.sh"),
    ] {
        let metadata = fs::metadata(path).unwrap();
        let mut perms = metadata.permissions();
        perms.set_mode(0o644); // rw-r--r-- (no execute)
        fs::set_permissions(path, perms).unwrap();
    }

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);

    assert!(result.is_ok());

    // Both scripts should be executable after init
    let worktree_sh = workspace.join(".loom").join("scripts").join("worktree.sh");
    let perms = fs::metadata(&worktree_sh).unwrap().permissions();
    assert!(
        perms.mode() & 0o111 != 0,
        "worktree.sh should be executable, mode: {:o}",
        perms.mode()
    );

    let loom_tools = workspace
        .join(".loom")
        .join("scripts")
        .join("lib")
        .join("loom-tools.sh");
    let perms = fs::metadata(&loom_tools).unwrap().permissions();
    assert!(
        perms.mode() & 0o111 != 0,
        "lib/loom-tools.sh should be executable, mode: {:o}",
        perms.mode()
    );
}

#[test]
fn test_find_missing_files_empty_when_all_present() {
    // Regression test for issue #3220: the post-copy assertion in
    // sync_managed_dir uses find_missing_files to detect partial copies.
    let temp_dir = TempDir::new().unwrap();
    let src = temp_dir.path().join("src");
    let dst = temp_dir.path().join("dst");

    fs::create_dir_all(src.join("lib")).unwrap();
    fs::create_dir_all(dst.join("lib")).unwrap();
    fs::write(src.join("a.sh"), "a").unwrap();
    fs::write(dst.join("a.sh"), "a").unwrap();
    fs::write(src.join("lib").join("b.sh"), "b").unwrap();
    fs::write(dst.join("lib").join("b.sh"), "b").unwrap();

    let missing = find_missing_files(&src, &dst);
    assert!(missing.is_empty(), "Expected no missing files, got: {missing:?}");
}

#[test]
fn test_find_missing_files_detects_missing_subdirectory_file() {
    // Specifically verifies the issue #3220 scenario: a file in a
    // subdirectory (e.g., scripts/lib/forge-helpers.sh) is missing
    // from the destination.
    let temp_dir = TempDir::new().unwrap();
    let src = temp_dir.path().join("src");
    let dst = temp_dir.path().join("dst");

    fs::create_dir_all(src.join("lib")).unwrap();
    fs::create_dir_all(dst.join("lib")).unwrap();
    fs::write(src.join("a.sh"), "a").unwrap();
    fs::write(dst.join("a.sh"), "a").unwrap();
    fs::write(src.join("lib").join("loom-tools.sh"), "tools").unwrap();
    fs::write(dst.join("lib").join("loom-tools.sh"), "tools").unwrap();
    // Source has forge-helpers.sh but destination does NOT — this is
    // the exact failure mode from issue #3220.
    fs::write(src.join("lib").join("forge-helpers.sh"), "helpers").unwrap();

    let missing = find_missing_files(&src, &dst);
    assert_eq!(missing.len(), 1, "Expected 1 missing file, got: {missing:?}");
    assert_eq!(missing[0], "lib/forge-helpers.sh");
}

#[test]
fn test_find_missing_files_detects_entire_missing_subdir() {
    // If a whole subdirectory is missing, every file under it should be reported.
    let temp_dir = TempDir::new().unwrap();
    let src = temp_dir.path().join("src");
    let dst = temp_dir.path().join("dst");

    fs::create_dir_all(src.join("lib")).unwrap();
    fs::create_dir_all(&dst).unwrap();
    fs::write(src.join("lib").join("a.sh"), "a").unwrap();
    fs::write(src.join("lib").join("b.sh"), "b").unwrap();

    let missing = find_missing_files(&src, &dst);
    assert_eq!(missing.len(), 2, "Expected 2 missing files, got: {missing:?}");
    let mut sorted = missing;
    sorted.sort();
    assert_eq!(sorted, vec!["lib/a.sh".to_string(), "lib/b.sh".to_string()]);
}

// ------------------------------------------------------------------
// config.json merge (issue #3598)
// ------------------------------------------------------------------

/// Write a minimal defaults/ tree with the given config.json body and
/// return (workspace, defaults) paths for `initialize_workspace`.
fn setup_config_merge_repo(
    temp: &TempDir,
    template: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace = temp.path().to_path_buf();
    let defaults = workspace.join("defaults");
    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::write(defaults.join("config.json"), template).unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    (workspace, defaults)
}

#[test]
fn test_config_worktree_root_survives_reinstall() {
    // The core issue #3598 repro: a committed config.json with a
    // worktree.root override must retain that key after reinstall.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "offlineMode": false}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);

    // Pre-existing consumer config carrying a worktree.root override.
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(
        workspace.join(".loom").join("config.json"),
        r#"{"version": "2", "worktree": {"root": "/Volumes/Stripe"}}"#,
    )
    .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");
    let report = result.unwrap();

    let merged: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap(),
    )
    .unwrap();

    // Consumer override preserved...
    assert_eq!(merged["worktree"]["root"], Value::String("/Volumes/Stripe".to_string()));
    // ...and a template key absent from the consumer file was added.
    assert_eq!(merged["offlineMode"], Value::Bool(false));

    // Merged (not clobbered) → reported as preserved so verification stays green.
    assert!(
        report.preserved.contains(&".loom/config.json".to_string()),
        "config.json should be reported preserved, got: {:?}",
        report.preserved
    );
}

#[test]
fn test_config_deep_merge_preserves_unknown_keys_and_conflict_resolution() {
    // Deep merge at any depth: unknown consumer keys survive, and on a
    // key present in BOTH files the consumer value wins while new template
    // keys are still delivered.
    let temp = TempDir::new().unwrap();
    let template = r#"{
          "version": "2",
          "reflection": {"enabled": true, "categories": ["bug", "enhancement"]},
          "newTemplateKey": "shipped"
        }"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);

    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(
        workspace.join(".loom").join("config.json"),
        r#"{
              "version": "2",
              "reflection": {"enabled": false, "upstream_repo": "me/fork"},
              "worktree": {"root": "/Volumes/X"},
              "customConsumerKey": {"nested": [1, 2, 3]}
            }"#,
    )
    .unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");

    let merged: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap(),
    )
    .unwrap();

    // Conflict on reflection.enabled → consumer (false) wins.
    assert_eq!(merged["reflection"]["enabled"], Value::Bool(false));
    // Consumer-only nested key preserved.
    assert_eq!(merged["reflection"]["upstream_repo"], Value::String("me/fork".to_string()));
    // Template-only nested key delivered.
    assert_eq!(merged["reflection"]["categories"], serde_json::json!(["bug", "enhancement"]));
    // New top-level template key delivered.
    assert_eq!(merged["newTemplateKey"], Value::String("shipped".to_string()));
    // Unknown consumer keys (including deeply nested arrays) preserved.
    assert_eq!(merged["worktree"]["root"], Value::String("/Volumes/X".to_string()));
    assert_eq!(merged["customConsumerKey"]["nested"], serde_json::json!([1, 2, 3]));
}

#[test]
fn test_config_merge_is_idempotent_across_repeat_reinstalls() {
    // A second consecutive reinstall must leave config.json byte-identical
    // (same bar as the #3590 .gitignore fix).
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "offlineMode": false, "terminals": []}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);

    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(
        workspace.join(".loom").join("config.json"),
        r#"{"version": "2", "worktree": {"root": "/Volumes/Stripe"}}"#,
    )
    .unwrap();

    let config_path = workspace.join(".loom").join("config.json");

    initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
        .expect("first reinstall");
    let after_first = fs::read_to_string(&config_path).unwrap();

    initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
        .expect("second reinstall");
    let after_second = fs::read_to_string(&config_path).unwrap();

    assert_eq!(
        after_first, after_second,
        "config.json must be byte-identical across repeat reinstalls"
    );
    // The override still survives the second pass.
    let merged: Value = serde_json::from_str(&after_second).unwrap();
    assert_eq!(merged["worktree"]["root"], Value::String("/Volumes/Stripe".to_string()));
}

#[test]
fn test_config_fresh_install_is_exact_template_copy() {
    // No existing .loom/config.json → exact byte-for-byte template copy,
    // reported as added (fresh-install behavior unchanged).
    let temp = TempDir::new().unwrap();
    let template = "{\n  \"version\": \"2\",\n  \"offlineMode\": false\n}\n";
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");
    let report = result.unwrap();

    let installed = fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap();
    assert_eq!(installed, template, "fresh install must be an exact template copy");
    assert!(
        report.added.contains(&".loom/config.json".to_string()),
        "fresh config.json should be reported added, got: {:?}",
        report.added
    );
}

#[test]
fn test_config_fresh_install_and_reinstall_are_byte_identical() {
    // Issue #3619: the fresh-install write path and the reinstall-merge
    // write path must emit BYTE-IDENTICAL output for the same logical
    // content. Before the fix, fresh install did a raw `fs::copy` of the
    // hand-formatted template (semantic key order, inline arrays) while the
    // reinstall merge re-serialized via `to_string_pretty` (expanded
    // arrays), so the first reinstall reformatted config.json and left it
    // permanently dirty. Now both paths serialize, so a fresh install
    // followed by a reinstall is a byte-for-byte no-op.
    let temp = TempDir::new().unwrap();
    // A template exercising the two axes that used to diverge: an inline
    // array (expanded by to_string_pretty) and multiple keys in a
    // non-alphabetical semantic order (preserved by `preserve_order`).
    let template = r#"{
  "version": "2",
  "offlineMode": false,
  "reflection": {
    "enabled": true,
    "categories": ["bug", "enhancement", "documentation"]
  },
  "terminals": []
}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    let config_path = workspace.join(".loom").join("config.json");

    // Fresh install (no existing .loom/config.json).
    let first =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("fresh install");
    assert!(
        first.added.contains(&".loom/config.json".to_string()),
        "fresh install should report config.json as added, got: {:?}",
        first.added
    );
    let after_fresh = fs::read_to_string(&config_path).unwrap();

    // Second run: now the file exists → the merge path runs. Its output
    // must be byte-identical to the fresh-install output (the crux of
    // #3619 — a reinstall over a freshly-installed config leaves it clean).
    initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
        .expect("reinstall merge");
    let after_reinstall = fs::read_to_string(&config_path).unwrap();

    assert_eq!(
        after_fresh, after_reinstall,
        "fresh-install and reinstall-merge output must be byte-identical (#3619)"
    );

    // Sanity: the serialized form is canonical (expanded array, trailing
    // newline, template key order preserved by `preserve_order`).
    assert!(
        after_fresh.ends_with("}\n"),
        "serialized config.json should end with a single trailing newline"
    );
    let version_pos = after_fresh.find("\"version\"").unwrap();
    let offline_pos = after_fresh.find("\"offlineMode\"").unwrap();
    assert!(
        version_pos < offline_pos,
        "preserve_order must retain template key order (version before offlineMode)"
    );
}

#[test]
fn test_config_invalid_existing_json_falls_back_to_template() {
    // A corrupt existing config.json falls back to the template copy with a
    // warning (does not abort) and is recorded as updated.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "offlineMode": false}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);

    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(workspace.join(".loom").join("config.json"), "{ this is not valid json ,,,").unwrap();

    let result =
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false);
    assert!(result.is_ok(), "init must not abort on invalid config: {result:?}");
    let report = result.unwrap();

    // The template must have replaced the corrupt file (valid JSON now).
    let installed: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap(),
    )
    .expect("post-fallback config.json must be valid JSON");
    assert_eq!(installed["offlineMode"], Value::Bool(false));
    assert!(
        report.updated.contains(&".loom/config.json".to_string()),
        "fallback config.json should be reported updated, got: {:?}",
        report.updated
    );
}

// ------------------------------------------------------------------
// config.json rewrite observability + repeated-init safety (issue #4641)
//
// Context: an operator-tuned `autonomous.workFinder.maxConcurrent` was
// silently reverted on a fleet worker with no log line naming the writer.
// `merge_config_file` is the only production writer of `.loom/config.json`,
// and `fleet add-worker` re-invoked it on every provisioning re-run.
// ------------------------------------------------------------------

/// Collect only this module's `init: config.json:` lines from a capture.
fn config_log_lines(records: &[(log::Level, String)]) -> Vec<(log::Level, String)> {
    records
        .iter()
        .filter(|(_, msg)| msg.contains("init: config.json:"))
        .cloned()
        .collect()
}

#[test]
fn test_operator_nested_key_survives_repeated_init() {
    // AC3 (#4641): the reported loss was of a nested, operator-only knob on
    // a host that runs provisioning repeatedly — so once is not enough.
    // Five consecutive `init` passes must leave the tuned value intact and
    // the file byte-stable.
    let temp = TempDir::new().unwrap();
    // The shipped template deliberately has NO maxConcurrent key — exactly
    // the shape that makes a template-wins or fallback-copy bug show up as
    // a silent revert to the built-in default.
    let template = r#"{
          "version": "2",
          "autonomous": {"workFinder": {"enabled": true}}
        }"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);

    let config_path = workspace.join(".loom").join("config.json");
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(
        &config_path,
        r#"{
              "version": "2",
              "autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 10}}
            }"#,
    )
    .unwrap();

    let mut previous: Option<String> = None;
    for pass in 1..=5 {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .unwrap_or_else(|e| panic!("init pass {pass} failed: {e}"));

        let raw = fs::read_to_string(&config_path).unwrap();
        let merged: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            merged["autonomous"]["workFinder"]["maxConcurrent"],
            serde_json::json!(10),
            "operator-tuned maxConcurrent lost on init pass {pass}: {raw}"
        );
        // Sibling template keys still delivered, not clobbered by the merge.
        assert_eq!(merged["autonomous"]["workFinder"]["enabled"], Value::Bool(true));

        if let Some(prev) = &previous {
            assert_eq!(
                prev, &raw,
                "config.json must be byte-stable from pass 2 onward (changed on pass {pass})"
            );
        }
        previous = Some(raw);
    }
}

#[test]
fn test_repeated_init_logs_merge_branch_and_no_effective_change() {
    // AC1 (#4641): every call names its branch. A steady-state reinstall is
    // a `merge-preserved` no-op and must say so, so an operator reading
    // daemon.log can tell "init ran and changed nothing" apart from "init
    // ran and rewrote your config".
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "autonomous": {"workFinder": {"enabled": true}}}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(
        workspace.join(".loom").join("config.json"),
        r#"{"version": "2", "autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 10}}}"#,
    )
    .unwrap();

    // Pass 1 delivers nothing new here (consumer is a superset), pass 2 is
    // the steady state either way.
    initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
        .expect("first init");

    let records = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("second init");
    });
    let lines = config_log_lines(&records);
    assert_eq!(lines.len(), 1, "exactly one config.json branch line expected, got {lines:?}");
    let (level, msg) = &lines[0];
    assert_eq!(*level, log::Level::Info, "a preserving merge is not a warning: {msg}");
    assert!(msg.contains("merge-preserved"), "branch must be named: {msg}");
    assert!(
        msg.contains("no effective config change"),
        "steady state must be explicit: {msg}"
    );
}

#[test]
fn test_merge_logs_diff_of_changed_keys() {
    // AC1 (#4641): when the write actually changes effective config, the
    // log carries a per-key diff — the artifact that was missing when the
    // reported revert happened.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "newTemplateKey": "shipped", "nested": {"added": 7}}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(workspace.join(".loom").join("config.json"), r#"{"version": "2"}"#).unwrap();

    let records = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("merge init");
    });
    let lines = config_log_lines(&records);
    assert_eq!(lines.len(), 1, "exactly one config.json branch line expected, got {lines:?}");
    let (level, msg) = &lines[0];
    assert_eq!(*level, log::Level::Info);
    assert!(msg.contains("merge-preserved"), "branch must be named: {msg}");
    assert!(msg.contains("2 key(s) changed"), "change count must be reported: {msg}");
    assert!(
        msg.contains(r#"+ newTemplateKey = "shipped""#),
        "added top-level key must appear in the diff: {msg}"
    );
    assert!(
        msg.contains("+ nested.added = 7"),
        "added nested key must appear with its dotted path: {msg}"
    );
}

#[test]
fn test_fresh_write_logs_branch() {
    // AC1 (#4641): the fresh-install branch is distinguishable from a merge.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "offlineMode": false}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);

    let records = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("fresh init");
    });
    let lines = config_log_lines(&records);
    assert_eq!(lines.len(), 1, "exactly one config.json branch line expected, got {lines:?}");
    let (level, msg) = &lines[0];
    assert_eq!(*level, log::Level::Info, "a fresh install is not a warning: {msg}");
    assert!(msg.contains("fresh-write"), "branch must be named: {msg}");
    assert!(msg.contains("2 key(s)"), "key count must be reported: {msg}");
}

#[test]
fn test_invalid_json_fallback_warns_and_names_discarded_keys() {
    // AC4 (#4641): the one branch that discards operator config wholesale
    // must log at warn! and name what it threw away. Presence alone is not
    // enough — the level assertion is what stops a regression back to a
    // bare eprintln!/debug! that never reaches daemon.log.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "offlineMode": false}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    // A torn/partial write: recognizable keys, unparseable overall.
    let corrupt = r#"{"version": "2", "autonomous": {"workFinder": {"maxConcurrent": 10"#;
    fs::write(workspace.join(".loom").join("config.json"), corrupt).unwrap();

    let records = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("init must not abort on invalid config");
    });
    let lines = config_log_lines(&records);
    assert_eq!(lines.len(), 1, "exactly one config.json branch line expected, got {lines:?}");
    let (level, msg) = &lines[0];
    assert_eq!(*level, log::Level::Warn, "the clobbering branch must warn, not inform: {msg}");
    assert!(msg.contains("invalid-JSON-fallback-overwrite"), "branch must be named: {msg}");
    for key in ["version", "autonomous", "workFinder", "maxConcurrent"] {
        assert!(msg.contains(key), "discarded key `{key}` must be named: {msg}");
    }

    // The discarded bytes are recoverable, not gone.
    let backup = workspace.join(".loom").join("config.json.bak");
    assert!(backup.exists(), "a rescue copy must be written before overwriting");
    assert_eq!(fs::read_to_string(&backup).unwrap(), corrupt);
    assert!(
        msg.contains("config.json.bak"),
        "the warning must point at the rescue copy: {msg}"
    );
}

#[test]
fn test_valid_json_but_not_an_object_hits_fallback_and_warns() {
    // Edge case from the #4641 test plan: valid JSON, wrong shape (an
    // array). It takes the same clobbering branch, so it needs the same
    // warn-level evidence.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "offlineMode": false}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(workspace.join(".loom").join("config.json"), r#"[{"maxConcurrent": 10}]"#).unwrap();

    let records = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("init must not abort on a non-object config");
    });
    let lines = config_log_lines(&records);
    assert_eq!(lines.len(), 1, "exactly one config.json branch line expected, got {lines:?}");
    let (level, msg) = &lines[0];
    assert_eq!(*level, log::Level::Warn, "the clobbering branch must warn: {msg}");
    assert!(msg.contains("invalid-JSON-fallback-overwrite"), "branch must be named: {msg}");
    assert!(
        msg.contains("valid JSON but not an object"),
        "the reason must distinguish this case from a parse error: {msg}"
    );
    assert!(msg.contains("maxConcurrent"), "discarded key must be named: {msg}");
    assert!(workspace.join(".loom").join("config.json.bak").exists());
}

#[test]
fn test_template_invalid_skip_warns_and_leaves_consumer_untouched() {
    // AC1 (#4641): the fourth branch. A broken shipped template must not
    // silently no-op — the consumer file survives, but the operator is told
    // their install did not deliver template updates.
    let temp = TempDir::new().unwrap();
    let (workspace, defaults) = setup_config_merge_repo(&temp, "{ not json at all ,,,");
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    let consumer = r#"{"version":"2","autonomous":{"workFinder":{"maxConcurrent":10}}}"#;
    fs::write(workspace.join(".loom").join("config.json"), consumer).unwrap();

    let records = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("init must not abort on an invalid template");
    });
    let lines = config_log_lines(&records);
    assert_eq!(lines.len(), 1, "exactly one config.json branch line expected, got {lines:?}");
    let (level, msg) = &lines[0];
    assert_eq!(*level, log::Level::Warn);
    assert!(msg.contains("template-invalid-skip"), "branch must be named: {msg}");
    assert_eq!(
        fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap(),
        consumer,
        "consumer config must be left byte-identical"
    );
}

#[test]
fn test_describe_config_changes_unit() {
    let before = serde_json::json!({
        "kept": 1,
        "changed": {"deep": "old"},
        "dropped": true,
        "arr": [1, 2]
    });
    let after = serde_json::json!({
        "kept": 1,
        "changed": {"deep": "new"},
        "added": {"nested": 9},
        "arr": [1, 2]
    });
    let changes = describe_config_changes(&before, &after);

    assert!(
        changes
            .iter()
            .any(|c| c == r#"~ changed.deep: "old" -> "new""#),
        "changed leaf must render old -> new: {changes:?}"
    );
    assert!(
        changes.iter().any(|c| c == "+ added.nested = 9"),
        "added leaf must render with a dotted path: {changes:?}"
    );
    assert!(
        changes.iter().any(|c| c == "- dropped (was true)"),
        "dropped leaf must be reported: {changes:?}"
    );
    // Unchanged scalars and unchanged arrays produce no noise.
    assert_eq!(changes.len(), 3, "unexpected extra changes: {changes:?}");
    assert!(describe_config_changes(&before, &before).is_empty());
}

#[test]
fn test_salvage_key_names_unit() {
    // Truncated mid-write — serde gives us nothing, so the scanner is the
    // only source of "what did we just discard".
    let keys =
        salvage_key_names(r#"{"version": "2", "autonomous": {"workFinder": {"maxConcurrent": 10"#);
    assert_eq!(keys, vec!["version", "autonomous", "workFinder", "maxConcurrent"]);

    // Escaped quotes inside a value must not desynchronize the scan, and a
    // repeated key is reported once.
    let keys = salvage_key_names(r#"{"a": "he said \"hi\": not a key", "b": 1, "a": 2"#);
    assert_eq!(keys, vec!["a", "b"]);

    // No key-shaped text at all.
    assert!(salvage_key_names("[1, 2, 3]").is_empty());
    assert!(salvage_key_names("").is_empty());
}

#[test]
fn test_summarize_list_elides_past_cap() {
    let short: Vec<String> = (0..3).map(|i| format!("k{i}")).collect();
    assert_eq!(summarize_list(&short), "k0; k1; k2");

    let long: Vec<String> = (0..MAX_LOGGED_ENTRIES + 5)
        .map(|i| format!("k{i}"))
        .collect();
    let rendered = summarize_list(&long);
    assert!(rendered.contains("k0"), "{rendered}");
    assert!(rendered.ends_with("… and 5 more"), "{rendered}");
    assert!(!rendered.contains("k24"), "entries past the cap must be elided: {rendered}");
}

#[test]
fn test_deep_merge_existing_wins_unit() {
    // Direct unit coverage of the merge primitive.
    let mut base = serde_json::json!({
        "a": 1,
        "shared": {"x": "template", "onlyTemplate": true},
        "arr": [1, 2]
    });
    let overlay = serde_json::json!({
        "shared": {"x": "consumer", "onlyConsumer": 9},
        "arr": [9],
        "b": 2
    });
    deep_merge_existing_wins(&mut base, &overlay);

    // Template-only top-level key retained.
    assert_eq!(base["a"], serde_json::json!(1));
    // Overlay-only top-level key added.
    assert_eq!(base["b"], serde_json::json!(2));
    // Nested object merged; overlay wins on conflict, both-only keys kept.
    assert_eq!(base["shared"]["x"], serde_json::json!("consumer"));
    assert_eq!(base["shared"]["onlyTemplate"], serde_json::json!(true));
    assert_eq!(base["shared"]["onlyConsumer"], serde_json::json!(9));
    // Arrays are replaced wholesale by the overlay (non-object value).
    assert_eq!(base["arr"], serde_json::json!([9]));
}

// ---------------------------------------------------------------------------
// #9123: the copy enumeration and the installed-files enumeration must agree
// ---------------------------------------------------------------------------

/// Repo root (`loom-daemon/`'s parent), for tests that run against the real
/// shipped tree. Same resolution the `test_real_defaults_*` tests above use.
fn repo_root_for_tests() -> std::path::PathBuf {
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
            if is_loom_payload_artifact(&name) {
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
fn installed_files_manifest(repo_root: &Path, target: &Path) -> std::collections::BTreeSet<String> {
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
    // that originate in `defaults/.loom/` (the rest of the manifest is
    // materialized by other surfaces — `defaults/roles/`, `defaults/config/`
    // via install-loom.sh, and so on).
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
    // The post-condition that makes `LOOM_TREE_SCAFFOLDED_FILES` safe (#9123):
    // a name on that list is skipped by the verbatim walk, so if the handler
    // that was supposed to write it does not, the init must FAIL rather than
    // hand back a `.loom/` that is missing a file `install-metadata.json`
    // already claims. `AGENTS.md` is on the list and
    // `setup_repository_scaffolding` only writes it when the workspace has a
    // root `AGENTS.md` anchor to pair it with — here we make the source exist
    // while blocking the destination, to prove the assertion actually fires.
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
