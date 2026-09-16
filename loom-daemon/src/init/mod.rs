//! Loom workspace initialization module
//!
//! This module provides functionality for initializing Loom workspaces
//! from the daemon and CLI surface. It can be used from:
//! - CLI mode (loom-daemon init)
//! - MCP tools (shared code)
//!
//! The initialization process:
//! 1. Validates the target is a git repository
//! 2. Detects self-installation (Loom source repo) and runs validation-only mode
//! 3. Copies `.loom/` configuration from `defaults/` (merge mode preserves custom files)
//! 4. Sets up repository scaffolding (CLAUDE.md, .claude/, .codex/)
//! 5. Updates .gitignore with Loom ephemeral patterns
//! 6. Reports which files were preserved vs added
//!
//! # Module Structure
//!
//! - [`git`]: Git detection, validation, and path resolution
//! - [`file_ops`]: File copy/merge/clean operations with reporting
//! - [`templates`]: Template variable substitution
//! - [`scaffolding`]: Repository scaffolding setup (CLAUDE.md, .claude/, etc.)
//! - [`post_init`]: Post-initialization operations (manifest, gitignore)
//! - [`repo_owned`]: Ownership boundary for the reinstall clean sweep (#5971)

mod file_ops;
mod git;
mod post_init;
mod repo_owned;
mod retired;
mod scaffolding;
mod templates;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use file_ops::{clean_managed_dir, copy_dir_with_report, verify_copied_files, TemplateContext};
use post_init::{find_overbroad_loom_patterns, generate_manifest, write_install_metadata};
use repo_owned::OwnershipBoundary;
use retired::cleanup_retired_files;
use scaffolding::setup_repository_scaffolding;

// Re-export public types and functions
pub use git::is_loom_source_repo;
// Re-exported so the `loom-daemon update-gitignore` subcommand (#4280) can
// rewrite the marker-delimited managed block on its own, without running a full
// `init`. The pattern list stays single-sourced in `post_init::EPHEMERAL_PATTERNS`.
pub use post_init::update_gitignore;

// Import the rest for internal use
use git::{
    link_dogfood_symlinks, resolve_defaults_path, validate_git_repository,
    validate_loom_source_repo,
};

/// Report of files affected during initialization
///
/// This struct tracks which files were added from defaults vs preserved
/// from the existing installation, enabling users to identify custom files
/// and deprecated files that may need cleanup.
#[derive(Debug, Default)]
pub struct InitReport {
    /// Files that were added from defaults (didn't exist before)
    pub added: Vec<String>,
    /// Files that were preserved (existed before, not overwritten)
    pub preserved: Vec<String>,
    /// Files that were updated (existed before, overwritten on reinstall)
    pub updated: Vec<String>,
    /// Files that were removed (existed in destination but not in source, cleaned on reinstall)
    pub removed: Vec<String>,
    /// Files inside a managed `.loom/` directory that the reinstall clean
    /// sweep left alone because the repo declared them repo-owned by pinning
    /// them in `.loom/resync-ignore` (issue #5971).
    pub preserved_repo_owned: Vec<String>,
    /// Files inside a managed `.loom/` directory that the reinstall clean
    /// sweep left alone because nothing attributes them to Loom: the current
    /// `defaults/` tree does not ship them and the previous install's
    /// `installed_files` record does not list them (issue #5971). Reported so
    /// the operator can decide — pin them in `.loom/resync-ignore` to make the
    /// intent explicit, or delete them by hand.
    pub preserved_unmanaged: Vec<String>,
    /// Files that failed post-copy verification (destination doesn't match source)
    pub verification_failures: Vec<String>,
    /// Whether this was a self-installation (Loom source repo)
    pub is_self_install: bool,
    /// Validation results for self-installation mode
    pub validation: Option<ValidationReport>,
}

/// Validation report for self-installation mode
#[derive(Debug, Default)]
pub struct ValidationReport {
    /// Role definitions found
    pub roles_found: Vec<String>,
    /// Scripts found
    pub scripts_found: Vec<String>,
    /// Slash commands found
    pub commands_found: Vec<String>,
    /// Subagent definitions found in .claude/agents/
    pub agents_found: Vec<String>,
    /// Whether CLAUDE.md exists
    pub has_claude_md: bool,
    /// Whether AGENTS.md exists (issue #4479, dual-runtime instruction anchor).
    /// Unlike `has_claude_md`, a missing AGENTS.md is NOT recorded as a
    /// validation issue — it is not mandatory for pre-existing installs that
    /// predate the dual-runtime work.
    pub has_agents_md: bool,
    /// Whether .github/labels.yml exists
    pub has_labels_yml: bool,
    /// Issues found during validation
    pub issues: Vec<String>,
}

/// Initialize a Loom workspace in the target directory
///
/// # Arguments
///
/// * `workspace_path` - Path to the workspace directory (must be a git repository)
/// * `defaults_path` - Path to the defaults directory (usually "defaults" or bundled resource)
/// * `force` - If true, overwrite existing files (otherwise merge mode preserves custom files)
///
/// # Returns
///
/// * `Ok(InitReport)` - Workspace successfully initialized with report of changes
/// * `Err(String)` - Initialization failed with error message
///
/// # Behavior
///
/// - **Fresh install** (no .loom directory): Copies all files from defaults
/// - **Reinstall with force=false** (merge mode): Adds new files, preserves ALL existing files
/// - **Reinstall with force=true** (force-merge mode): Updates default files, preserves custom files
///
/// Both reinstall modes preserve custom project roles/commands (files not in defaults).
/// Force mode is useful when you want to update Loom's built-in roles to the latest version.
///
/// # Errors
///
/// This function will return an error if:
/// - The workspace path doesn't exist or isn't a directory
/// - The workspace isn't a git repository (no .git directory)
/// - File operations fail (insufficient permissions, disk full, etc.)
pub fn initialize_workspace(
    workspace_path: &str,
    defaults_path: &str,
    force: bool,
) -> Result<InitReport, String> {
    let workspace = Path::new(workspace_path);
    let loom_path = workspace.join(".loom");
    let mut report = InitReport::default();

    // Validate workspace is a git repository
    validate_git_repository(workspace_path)?;

    // Check for over-broad gitignore patterns that would shadow installed Loom
    // files (e.g., `.loom/scripts/lib/*.sh`). This catches the regression
    // reported in issue #3287, where a target repo's `.gitignore` contained
    // `.loom/` and caused the install worktree's lib files to never be
    // committed to main. We fail fast here, before any file operations.
    let bad_patterns = find_overbroad_loom_patterns(workspace);
    if !bad_patterns.is_empty() {
        return Err(format!(
            "Refusing to install: .gitignore contains pattern(s) that would block \
             installed Loom files from being committed (e.g., .loom/scripts/lib/*.sh). \
             Remove or scope these patterns to specific runtime files before \
             reinstalling. Offending patterns: {}",
            bad_patterns.join(", ")
        ));
    }

    // Check for self-installation (Loom source repo)
    if is_loom_source_repo(workspace) {
        report.is_self_install = true;
        // Issue #6440: idempotently create the dogfood symlinks
        // (`.claude/commands/loom`, `.claude/agents` -> `defaults/...`)
        // BEFORE validating, so a fresh clone's very first `loom-daemon init`
        // — the call `fleet add-worker` actually makes when provisioning a
        // daemon workspace — closes the structural gap itself instead of
        // merely reporting it missing every time. Strictly scoped to this
        // `is_loom_source_repo` branch; see `link_dogfood_symlinks`'s own
        // doc comment for why that scoping must never widen.
        for line in link_dogfood_symlinks(workspace) {
            log::info!("loom-daemon init (dogfood, #6440): {line}");
        }
        report.validation = Some(validate_loom_source_repo(workspace));
        update_gitignore(workspace)?;
        return Ok(report);
    }

    // Resolve defaults path (development mode or bundled resource)
    let defaults = resolve_defaults_path(defaults_path)?;
    let is_reinstall = loom_path.exists();
    let _ = (is_reinstall, force); // These affect behavior in called functions

    // Create .loom directory if it doesn't exist
    fs::create_dir_all(&loom_path).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Copy config and README files.
    //
    // `config.json` is merge-aware (issue #3598): unlike the README (a
    // Loom-owned doc that is safe to overwrite), `.loom/config.json` is
    // committed CONSUMER configuration that may carry local overrides such as
    // `worktree.root`. A bare `fs::copy` from the template would silently drop
    // those keys — see `merge_config_file`.
    merge_config_file(&defaults, &loom_path, &mut report)?;
    copy_single_file(&defaults, &loom_path, ".loom-README.md", ".loom/README.md", &mut report)?;

    // `.loom/biome.jsonc` (#6031): a nested Biome configuration that takes the
    // whole machine-managed `.loom/` tree out of a consumer's repo-wide
    // `biome check .`. Without it the shipped Workflow-tool experiment script
    // (`.loom/scripts/experiments/judge-fanout-workflow.js`, which legally uses
    // top-level `return`) is a hard PARSE error and the installer-emitted JSON
    // stamps are perpetual format diffs — in files the consumer never wrote.
    //
    // The manifest generator (scripts/install/manifest.sh) walks
    // `defaults/.loom/` and registers every file under it as Loom-installed, so
    // this copy must exist or the installer's post-install metadata-vs-disk
    // check fails with "MISSING: .loom/biome.jsonc" and rolls the install back.
    // Overwritten wholesale on reinstall: it is Loom payload, not consumer
    // configuration (contrast `merge_config_file` above).
    copy_single_file(&defaults, &loom_path, ".loom/biome.jsonc", ".loom/biome.jsonc", &mut report)?;

    // Ownership evidence for the reinstall clean sweep (issue #5971). Read
    // BEFORE any sync, because `write_install_metadata` below overwrites
    // `.loom/install-metadata.json` with a stub whose `installed_files` is
    // empty — reading it later would discard the previous install's record.
    let ownership = OwnershipBoundary::load(workspace);

    // Sync managed directories (clean stale Loom-owned files on reinstall,
    // then copy fresh; repo-owned content is preserved and reported)
    sync_managed_dir(&defaults, &loom_path, "roles", is_reinstall, &ownership, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "scripts", is_reinstall, &ownership, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "hooks", is_reinstall, &ownership, &mut report)?;
    // `docs` ships static reference documentation (e.g. ci-integration.md
    // from issue #3333). Sync alongside other managed dirs so installed
    // repos always carry the latest copy.
    sync_managed_dir(&defaults, &loom_path, "docs", is_reinstall, &ownership, &mut report)?;
    // `runtimes` ships the per-runtime capability manifests consumed by
    // `runtime_admission::roots()` (#4688). This directory was declared in
    // the install manifest (scripts/install/manifest.sh) since #4183 but
    // never actually synced by this Rust-native path — every fresh install
    // and Rust-native reinstall left `.loom/runtimes/` unpopulated, which
    // made the admission gate fall through to a nonexistent
    // `defaults/runtimes/...` on every consumer dispatch.
    sync_managed_dir(&defaults, &loom_path, "runtimes", is_reinstall, &ownership, &mut report)?;

    // Sync `.loom/bin/` from `defaults/.loom/bin/`. The manifest generator
    // (scripts/install/manifest.sh) walks `defaults/.loom/` and registers
    // every file under it as Loom-installed, so the bin/ subdirectory must
    // be copied here or the post-install metadata-vs-disk verification
    // fails fast on missing `.loom/bin/loom`. Pass `defaults/.loom` as the
    // helper's `defaults` arg so src=`defaults/.loom/bin` and dst=`.loom/bin`.
    sync_managed_dir(
        &defaults.join(".loom"),
        &loom_path,
        "bin",
        is_reinstall,
        &ownership,
        &mut report,
    )?;

    make_shell_scripts_executable(&loom_path.join("hooks"));
    make_shell_scripts_executable(&loom_path.join("scripts"));
    make_shell_scripts_executable(&loom_path.join("bin"));

    // Update .gitignore and setup scaffolding
    update_gitignore(workspace)?;
    setup_repository_scaffolding(workspace, &defaults, force, &mut report)?;

    // Verify all copied files match their sources
    verify_all_copied_files(workspace, &defaults, &loom_path, &mut report);

    // Filter out verification failures for files that were intentionally preserved.
    // Preserved files (existing user customizations) are expected to differ from the
    // source defaults — flagging them as failures is misleading and the prior
    // "rerun with --force" remediation would clobber the user's intentional edits.
    filter_preserved_from_verification_failures(&mut report);

    // Content-gated cleanup of retired Loom strays (issue #3576). The daemon
    // init sync is source-driven and never removes destination-only files, so a
    // stray `.claude/commands/loom/release.md` (the `/loom:release` skill
    // retired by #3563) lingers on disk for Quick-Install consumers. Remove it
    // iff its sha256 matches a frozen shipped digest (unmodified); preserve a
    // customized copy; no-op when absent. Mirrors the shell-side
    // `LOOM_RETIRED_FILES` block in scripts/install-loom.sh (PR #3575).
    //
    // Placed after scaffolding (post-sync) and before generate_manifest so the
    // manifest reflects on-disk state. The self-install short-circuit above
    // (returns at ~line 147 before scaffolding) means this never runs on the
    // Loom source repo — it must not mutate the source tree.
    cleanup_retired_files(workspace, &mut report);

    // Write .loom/install-metadata.json (and the gitignored loom-source-path
    // sidecar) BEFORE generate_manifest so verify-install.sh reads the fresh
    // loom_commit when it builds manifest.json (#4050). A direct `loom-daemon
    // init` sets no LOOM_* env, so from_env() supplies the binary's compiled-in
    // version/commit rather than the literal "unknown". The shell wrappers run
    // finalize_quick_install after init and overwrite this with richer data.
    write_install_metadata(workspace, &templates::LoomMetadata::from_env(), &defaults);

    // Generate installation manifest (.loom/manifest.json)
    generate_manifest(workspace);

    Ok(report)
}

/// Remove `verification_failures` entries whose path appears in `preserved`.
///
/// Verification failure entries are formatted as `"{rel_path} ({reason})"`. We
/// extract the leading path component and drop the entry if it matches a
/// preserved path. A preserved file is, by definition, expected to differ from
/// the source default (the user customized it), so it should not surface as a
/// "verification failure".
fn filter_preserved_from_verification_failures(report: &mut InitReport) {
    if report.preserved.is_empty() || report.verification_failures.is_empty() {
        return;
    }
    let preserved: HashSet<&str> = report.preserved.iter().map(String::as_str).collect();
    report.verification_failures.retain(|f| {
        let rel_path = f.split(" (").next().unwrap_or(f.as_str());
        !preserved.contains(rel_path)
    });
}

/// Copy a single file from defaults to the loom directory, tracking in report.
fn copy_single_file(
    defaults: &Path,
    loom_path: &Path,
    src_name: &str,
    report_name: &str,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join(src_name);
    // The destination may differ from the source name (e.g., ".loom-README.md" → "README.md")
    let dst_name = report_name.strip_prefix(".loom/").unwrap_or(src_name);
    let dst = loom_path.join(dst_name);
    if src.exists() {
        let existed = dst.exists();
        fs::copy(&src, &dst).map_err(|e| format!("Failed to copy {src_name}: {e}"))?;
        if existed {
            report.updated.push(report_name.to_string());
        } else {
            report.added.push(report_name.to_string());
        }
    }
    Ok(())
}

/// Merge-aware copy of `.loom/config.json` from defaults (issue #3598).
///
/// `.loom/config.json` is committed consumer configuration, not a runtime
/// artifact. A bare `fs::copy` (as `copy_single_file` performs) would clobber
/// consumer keys such as the documented `worktree.root` override every time the
/// installer reran. This function instead:
///
/// - **Destination missing** → the template is parsed and re-emitted through
///   the same canonical `to_string_pretty` serialize path the merge branch
///   uses (issue #3619), recorded as `added`. Routing the fresh install through
///   serialization (rather than a raw `fs::copy` of the hand-formatted
///   template) makes the on-disk file canonical from the very first install, so
///   a later reinstall merge re-emits byte-identical output and config.json is
///   never left dirty. (If the template is not valid JSON, falls back to a raw
///   copy.)
/// - **Destination is a valid JSON object** → deep-merge with the shipped
///   template as the base and the **existing consumer values winning** on
///   conflict; keys new in the template are added, unknown consumer keys at any
///   depth are preserved. Written with deterministic pretty serialization so
///   repeat reinstalls are byte-idempotent. Recorded as `preserved`.
/// - **Destination exists but is invalid JSON (or not an object)** → the
///   previous contents are first copied aside to `.loom/config.json.bak`, then
///   the template replaces the file, with a loud `warn!`; the install does not
///   abort. Recorded as `updated`.
///
/// Byte-exact preservation of consumer formatting/comments is explicitly out of
/// scope — deterministic re-serialization is acceptable as long as keys/values
/// survive and repeat runs are stable.
///
/// # Observability (issue #4641)
///
/// Every call emits exactly one branch line through the `log` facade, tagged
/// with a greppable branch name — `fresh-write`, `merge-preserved`,
/// `template-invalid-skip`, or `invalid-JSON-fallback-overwrite` — prefixed
/// `init: config.json:`. `merge-preserved` additionally carries a leaf-level
/// diff of every key whose effective value changed, and the fallback branch
/// logs at `warn!` naming the keys it discarded plus the rescue-copy path.
///
/// This exists because `loom-daemon init` is invoked unattended from
/// provisioning scripts (`fleet::add_worker`, `install.sh` reinstalls), where a
/// bare `eprintln!` disappears into a log nobody reads: an operator-tuned
/// `autonomous.workFinder.maxConcurrent` was silently reverted on a fleet
/// worker with no trace of which process rewrote the file. `warn!`/`info!` land
/// in `daemon.log` and are greppable after the fact.
fn merge_config_file(
    defaults: &Path,
    loom_path: &Path,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join("config.json");
    let dst = loom_path.join("config.json");
    let report_name = ".loom/config.json";

    // No template shipped — nothing to do (mirrors copy_single_file's guard).
    if !src.exists() {
        return Ok(());
    }

    let template_str = fs::read_to_string(&src)
        .map_err(|e| format!("Failed to read defaults config.json: {e}"))?;

    // Fresh install: no existing consumer file → write the template through the
    // SAME canonical serialize path the reinstall merge uses (issue #3619). A
    // bare `fs::copy` of the hand-formatted template produced bytes that a later
    // merge's `to_string_pretty` output could never match, leaving config.json
    // permanently dirty after the first reinstall. Serializing here makes the
    // on-disk file canonical from the very first install, so any later merge
    // re-emits byte-identical output. With serde_json's `preserve_order`
    // feature, template key order is retained (keys are not alphabetized).
    if !dst.exists() {
        match serde_json::from_str::<Value>(&template_str) {
            Ok(template_val) => {
                let mut serialized = serde_json::to_string_pretty(&template_val)
                    .map_err(|e| format!("Failed to serialize config.json: {e}"))?;
                serialized.push('\n');
                fs::write(&dst, serialized)
                    .map_err(|e| format!("Failed to write config.json: {e}"))?;
                log::info!(
                    "init: config.json: fresh-write {} — no existing file; wrote {} key(s) from the shipped template",
                    dst.display(),
                    top_level_key_count(&template_val)
                );
            }
            Err(e) => {
                // Template is invalid JSON — fall back to a raw copy rather than
                // dropping the install. (The reinstall branch handles this too.)
                eprintln!(
                    "Warning: defaults/config.json is not valid JSON ({e}); \
                     copying it verbatim to .loom/config.json"
                );
                log::warn!(
                    "init: config.json: fresh-write {} — defaults/config.json is not valid JSON ({e}); copied verbatim",
                    dst.display()
                );
                fs::copy(&src, &dst).map_err(|e| format!("Failed to copy config.json: {e}"))?;
            }
        }
        report.added.push(report_name.to_string());
        return Ok(());
    }

    let existing_str = fs::read_to_string(&dst)
        .map_err(|e| format!("Failed to read existing config.json: {e}"))?;

    // If the shipped template is somehow invalid JSON, do NOT clobber the
    // consumer's file — leave it exactly as-is and record it as preserved.
    let template_val: Value = match serde_json::from_str(&template_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "Warning: defaults/config.json is not valid JSON ({e}); \
                 leaving existing .loom/config.json untouched"
            );
            log::warn!(
                "init: config.json: template-invalid-skip {} — defaults/config.json is not valid JSON ({e}); existing file left untouched",
                dst.display()
            );
            report.preserved.push(report_name.to_string());
            return Ok(());
        }
    };

    // If the consumer file is missing/invalid/non-object, fall back to the
    // template copy with a loud warning. This must not abort the install.
    //
    // This is the ONE branch that can silently discard operator-tuned keys
    // wholesale (#4641), so before overwriting we (a) copy the unparseable
    // bytes aside to `.loom/config.json.bak` so nothing is truly lost, and (b)
    // `warn!` with a best-effort list of the key names visible in the discarded
    // text. A torn read caused by a concurrent writer is exactly the scenario
    // this needs to leave evidence for.
    let existing_val: Value = match serde_json::from_str::<Value>(&existing_str) {
        Ok(v) if v.is_object() => v,
        parsed => {
            let reason = match parsed {
                Ok(_) => "valid JSON but not an object".to_string(),
                Err(e) => format!("not valid JSON: {e}"),
            };
            let discarded = salvage_key_names(&existing_str);
            let discarded_desc = if discarded.is_empty() {
                "none recoverable from the unparseable text".to_string()
            } else {
                summarize_list(&discarded)
            };
            let backup = dst.with_extension("json.bak");
            let backup_desc = match fs::write(&backup, &existing_str) {
                Ok(()) => format!("previous contents saved to {}", backup.display()),
                Err(e) => format!("FAILED to save previous contents to {}: {e}", backup.display()),
            };

            eprintln!(
                "Warning: existing .loom/config.json is not valid JSON; overwriting \
                 with the shipped template (previous contents were not preserved)"
            );
            log::warn!(
                "init: config.json: invalid-JSON-fallback-overwrite {} — existing file is {reason}; \
                 overwriting with the shipped template. Discarded keys: {discarded_desc}. {backup_desc}",
                dst.display()
            );

            fs::copy(&src, &dst).map_err(|e| format!("Failed to copy config.json: {e}"))?;
            report.updated.push(report_name.to_string());
            return Ok(());
        }
    };

    // Deep-merge: template is the base, existing consumer values win on conflict.
    let mut merged = template_val;
    deep_merge_existing_wins(&mut merged, &existing_val);

    // Diff BEFORE writing, so the log describes the effective config change this
    // call is about to make. With existing-wins semantics the expected shape is
    // additions only (new template keys); a `~` or `-` entry here means a
    // consumer value was overwritten or dropped and is worth investigating.
    let changes = describe_config_changes(&existing_val, &merged);

    let mut serialized = serde_json::to_string_pretty(&merged)
        .map_err(|e| format!("Failed to serialize merged config.json: {e}"))?;
    serialized.push('\n');
    fs::write(&dst, serialized).map_err(|e| format!("Failed to write merged config.json: {e}"))?;

    if changes.is_empty() {
        log::info!(
            "init: config.json: merge-preserved {} — no effective config change",
            dst.display()
        );
    } else {
        log::info!(
            "init: config.json: merge-preserved {} — {} key(s) changed: {}",
            dst.display(),
            changes.len(),
            summarize_list(&changes)
        );
    }

    report.preserved.push(report_name.to_string());
    Ok(())
}

/// Maximum number of individual entries spelled out in one log line before the
/// remainder is elided as `… and N more`. Keeps a pathological config (or a
/// fresh install merging a large template) from emitting a multi-kilobyte line.
const MAX_LOGGED_ENTRIES: usize = 20;

/// Number of top-level keys in a JSON value (0 for non-objects).
fn top_level_key_count(value: &Value) -> usize {
    value.as_object().map_or(0, |m| m.len())
}

/// Join `entries` for a log line, eliding past [`MAX_LOGGED_ENTRIES`].
fn summarize_list(entries: &[String]) -> String {
    if entries.len() <= MAX_LOGGED_ENTRIES {
        entries.join("; ")
    } else {
        format!(
            "{}; … and {} more",
            entries[..MAX_LOGGED_ENTRIES].join("; "),
            entries.len() - MAX_LOGGED_ENTRIES
        )
    }
}

/// Flatten a JSON value into `(dotted.path, compact-json-value)` leaf pairs.
///
/// Arrays are treated as leaves (compared wholesale), matching
/// [`deep_merge_existing_wins`], which also replaces arrays wholesale rather
/// than merging element-by-element.
fn flatten_json_leaves(value: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_json_leaves(child, &path, out);
            }
        }
        leaf => out.push((prefix.to_string(), leaf.to_string())),
    }
}

/// Describe the leaf-level differences between two configs as log-ready lines.
///
/// `+ path = value` (added), `- path (was value)` (dropped), and
/// `~ path: old -> new` (changed). An empty result means the write is a no-op in
/// effective-config terms.
fn describe_config_changes(before: &Value, after: &Value) -> Vec<String> {
    let mut before_leaves = Vec::new();
    flatten_json_leaves(before, "", &mut before_leaves);
    let mut after_leaves = Vec::new();
    flatten_json_leaves(after, "", &mut after_leaves);

    let before_map: std::collections::BTreeMap<&str, &str> = before_leaves
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let after_map: std::collections::BTreeMap<&str, &str> = after_leaves
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut changes = Vec::new();
    for (path, new_val) in &after_leaves {
        match before_map.get(path.as_str()) {
            Some(old_val) if *old_val == new_val.as_str() => {}
            Some(old_val) => changes.push(format!("~ {path}: {old_val} -> {new_val}")),
            None => changes.push(format!("+ {path} = {new_val}")),
        }
    }
    for (path, old_val) in &before_leaves {
        if !after_map.contains_key(path.as_str()) {
            changes.push(format!("- {path} (was {old_val})"));
        }
    }
    changes
}

/// Best-effort recovery of key names from text that failed to parse as JSON.
///
/// Used only on the invalid-JSON fallback path, where `serde_json` gives us
/// nothing to enumerate but the operator still needs to know *what* was
/// discarded. Scans for `"…"` immediately followed (modulo whitespace) by `:`,
/// deduplicating while preserving first-seen order. Deliberately naive: a key
/// name appearing inside a string *value* may be reported too. Over-reporting a
/// name in a warning is far cheaper than reporting nothing.
fn salvage_key_names(raw: &str) -> Vec<String> {
    let bytes = raw.as_bytes();
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        // Walk to the closing quote, honoring backslash escapes. Only ASCII
        // bytes are inspected, so multi-byte UTF-8 inside the string is safe.
        let start = i + 1;
        let mut end = start;
        let mut escaped = false;
        while end < bytes.len() {
            match bytes[end] {
                b'\\' if !escaped => escaped = true,
                b'"' if !escaped => break,
                _ => escaped = false,
            }
            end += 1;
        }
        if end >= bytes.len() {
            break; // unterminated string — nothing more to salvage
        }
        let mut after = end + 1;
        while after < bytes.len() && bytes[after].is_ascii_whitespace() {
            after += 1;
        }
        if after < bytes.len() && bytes[after] == b':' {
            if let Ok(name) = std::str::from_utf8(&bytes[start..end]) {
                if seen.insert(name.to_string()) {
                    keys.push(name.to_string());
                }
            }
        }
        i = end + 1;
    }
    keys
}

/// Deep-merge `overlay` into `base` with **overlay values winning** on conflict.
///
/// Used by [`merge_config_file`] with `base` = shipped template and `overlay` =
/// existing consumer config, so consumer edits are never lost while new
/// template keys are still delivered:
///
/// - Two objects are merged key-by-key, recursing into nested objects.
/// - Any non-object `overlay` value (array, scalar, null) replaces the
///   corresponding `base` value wholesale.
/// - Keys present only in `base` (new template keys) are retained.
/// - Keys present only in `overlay` (unknown consumer keys) are preserved.
///
/// `pub(crate)` (not just `fn`, issue #4390): [`crate::calibrate`] reuses this
/// same generic "overlay wins, base's untouched keys survive, recurse into
/// objects" merge for its `--write` path, with the roles inverted from this
/// module's own usage — there `overlay` is the existing consumer config
/// (wins over the shipped template); in calibrate `overlay` is the new
/// recommended knob values (wins over whatever `.loom/config.json` already
/// has at those two leaf paths). See `calibrate::merge_workfinder_values`'s
/// doc comment for the full rationale.
pub(crate) fn deep_merge_existing_wins(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_val) in overlay_map {
                match base_map.get_mut(key) {
                    Some(base_val) => deep_merge_existing_wins(base_val, overlay_val),
                    None => {
                        base_map.insert(key.clone(), overlay_val.clone());
                    }
                }
            }
        }
        (base_slot, overlay_val) => {
            *base_slot = overlay_val.clone();
        }
    }
}

/// Sync a managed directory: clean stale **Loom-owned** files on reinstall,
/// then copy fresh from defaults.
///
/// The clean step is ownership-gated (issue #5971) — see
/// [`file_ops::clean_managed_dir`]. Repo-owned content living inside a managed
/// directory (`.loom/hooks/` above all, which Loom invokes but never claims in
/// the installed-files manifest) survives the reinstall and is reported.
///
/// After copying, this function performs a fail-fast assertion that every file
/// (including those in subdirectories) present in `defaults/<dir_name>/` exists
/// in the destination. This guards against silent omissions like the regression
/// reported in issue #3220, where `scripts/lib/forge-helpers.sh` was missing
/// from installs even though `lib/loom-tools.sh` was verified by name.
fn sync_managed_dir(
    defaults: &Path,
    loom_path: &Path,
    dir_name: &str,
    is_reinstall: bool,
    ownership: &OwnershipBoundary,
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join(dir_name);
    let dst = loom_path.join(dir_name);
    let report_prefix = format!(".loom/{dir_name}");
    if src.exists() {
        if is_reinstall {
            clean_managed_dir(&src, &dst, &report_prefix, ownership, report)
                .map_err(|e| format!("Failed to clean {dir_name} directory: {e}"))?;
        }
        copy_dir_with_report(&src, &dst, &report_prefix, report)
            .map_err(|e| format!("Failed to copy {dir_name} directory: {e}"))?;

        // Fail-fast: ensure every source file (including subdirectories) reached
        // the destination. This catches bugs in copy_dir_with_report and
        // unexpected filesystem failures (permission denied on a subdir, etc.)
        // before they propagate to a broken install.
        let missing = find_missing_files(&src, &dst);
        if !missing.is_empty() {
            return Err(format!(
                "Sync of {dir_name} directory completed but {} file(s) are missing from \
                 destination (likely a copy bug or filesystem error): {}",
                missing.len(),
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

/// Recursively find files present in `src` but missing from `dst`.
///
/// Returns relative paths (from `src`) for each missing file. Used as a
/// post-copy assertion to detect partial copies. Subdirectories are walked
/// recursively so that nested files (e.g., `scripts/lib/*.sh`) are checked.
fn find_missing_files(src: &Path, dst: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    collect_missing_files(src, dst, "", &mut missing);
    missing
}

fn collect_missing_files(src: &Path, dst: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();
        let rel_path = if prefix.is_empty() {
            file_name_str.to_string()
        } else {
            format!("{prefix}/{file_name_str}")
        };
        let src_child = entry.path();
        let dst_child = dst.join(&file_name);
        if file_type.is_dir() {
            collect_missing_files(&src_child, &dst_child, &rel_path, out);
        } else if !dst_child.exists() {
            out.push(rel_path);
        }
    }
}

/// Ensure all `.sh` files in a directory (and subdirectories) are executable.
///
/// This is applied to both hooks/ and scripts/ after copying from defaults.
/// While `fs::copy` preserves permissions on Unix, some git configurations
/// or filesystem operations may strip the execute bit. This ensures all
/// shell scripts remain executable regardless of how they were copied.
fn make_shell_scripts_executable(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                make_shell_scripts_executable(&path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("sh") {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = std::fs::metadata(&path) {
                        let mut perms = metadata.permissions();
                        perms.set_mode(perms.mode() | 0o111);
                        let _ = std::fs::set_permissions(&path, perms);
                    }
                }
            }
        }
    }
}

/// Verify all copied files and scaffolding directories match their sources.
fn verify_all_copied_files(
    workspace: &Path,
    defaults: &Path,
    loom_path: &Path,
    report: &mut InitReport,
) {
    // Verify .loom managed directories (no template substitution needed)
    for dir_name in &["roles", "scripts", "hooks", "docs", "runtimes"] {
        let src = defaults.join(dir_name);
        let dst = loom_path.join(dir_name);
        let prefix = format!(".loom/{dir_name}");
        verify_copied_files(&src, &dst, &prefix, report, None);
    }

    // Verify scaffolding directories with template context for variable substitution
    let repo_info = git::extract_repo_info(workspace);
    let template_ctx = TemplateContext {
        repo_owner: repo_info.as_ref().map(|(o, _)| o.clone()),
        repo_name: repo_info.map(|(_, n)| n),
        loom_metadata: templates::LoomMetadata::from_env(),
    };
    let ctx = Some(&template_ctx);

    for dir_name in &[".claude", ".codex", ".github"] {
        let src = defaults.join(dir_name);
        let dst = workspace.join(dir_name);
        verify_copied_files(&src, &dst, dir_name, report, ctx);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
