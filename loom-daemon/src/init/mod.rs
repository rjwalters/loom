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

mod file_ops;
mod git;
mod post_init;
mod retired;
mod scaffolding;
mod templates;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use file_ops::{clean_managed_dir, copy_dir_with_report, verify_copied_files, TemplateContext};
use post_init::{find_overbroad_loom_patterns, generate_manifest, write_install_metadata};
use retired::cleanup_retired_files;
use scaffolding::setup_repository_scaffolding;

// Re-export public types and functions
pub use git::is_loom_source_repo;
// Re-exported so the `loom-daemon update-gitignore` subcommand (#4280) can
// rewrite the marker-delimited managed block on its own, without running a full
// `init`. The pattern list stays single-sourced in `post_init::EPHEMERAL_PATTERNS`.
pub use post_init::update_gitignore;

// Import the rest for internal use
use git::{resolve_defaults_path, validate_git_repository, validate_loom_source_repo};

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

    // Sync managed directories (clean stale files on reinstall, then copy fresh)
    sync_managed_dir(&defaults, &loom_path, "roles", is_reinstall, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "scripts", is_reinstall, &mut report)?;
    sync_managed_dir(&defaults, &loom_path, "hooks", is_reinstall, &mut report)?;
    // `docs` ships static reference documentation (e.g. ci-integration.md
    // from issue #3333). Sync alongside other managed dirs so installed
    // repos always carry the latest copy.
    sync_managed_dir(&defaults, &loom_path, "docs", is_reinstall, &mut report)?;
    // `runtimes` ships the per-runtime capability manifests consumed by
    // `runtime_admission::roots()` (#4688). This directory was declared in
    // the install manifest (scripts/install/manifest.sh) since #4183 but
    // never actually synced by this Rust-native path — every fresh install
    // and Rust-native reinstall left `.loom/runtimes/` unpopulated, which
    // made the admission gate fall through to a nonexistent
    // `defaults/runtimes/...` on every consumer dispatch.
    sync_managed_dir(&defaults, &loom_path, "runtimes", is_reinstall, &mut report)?;

    // Sync `.loom/bin/` from `defaults/.loom/bin/`. The manifest generator
    // (scripts/install/manifest.sh) walks `defaults/.loom/` and registers
    // every file under it as Loom-installed, so the bin/ subdirectory must
    // be copied here or the post-install metadata-vs-disk verification
    // fails fast on missing `.loom/bin/loom`. Pass `defaults/.loom` as the
    // helper's `defaults` arg so src=`defaults/.loom/bin` and dst=`.loom/bin`.
    sync_managed_dir(&defaults.join(".loom"), &loom_path, "bin", is_reinstall, &mut report)?;

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

/// Sync a managed directory: clean stale files on reinstall, then copy fresh from defaults.
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
    report: &mut InitReport,
) -> Result<(), String> {
    let src = defaults.join(dir_name);
    let dst = loom_path.join(dir_name);
    let report_prefix = format!(".loom/{dir_name}");
    if src.exists() {
        if is_reinstall {
            clean_managed_dir(&dst, &report_prefix, report)
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
mod tests {
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
        let sidecar_src =
            fs::read_to_string(workspace.join(".loom").join("loom-source-path")).unwrap();
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

        let result =
            initialize_workspace(workspace.to_str().unwrap(), "nonexistent-defaults", false);
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

        let result =
            initialize_workspace(workspace.to_str().unwrap(), "nonexistent-defaults", false);
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
        fs::write(workspace.join(".loom").join("roles").join("stale-role.md"), "stale role")
            .unwrap();

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
        let judge =
            fs::read_to_string(workspace.join(".loom").join("roles").join("judge.md")).unwrap();
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
        fs::write(workspace.join(".loom").join("roles").join("obsolete.md"), "removed role")
            .unwrap();

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
            fs::read_to_string(workspace.join(".loom").join("scripts").join("worktree.sh"))
                .unwrap();
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
        let installed =
            fs::read_to_string(workspace.join(".claude").join("settings.json")).unwrap();
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
        fs::write(workspace.join(".loom").join("config.json"), "{ this is not valid json ,,,")
            .unwrap();

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
        fs::write(workspace.join(".loom").join("config.json"), r#"[{"maxConcurrent": 10}]"#)
            .unwrap();

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
        let keys = salvage_key_names(
            r#"{"version": "2", "autonomous": {"workFinder": {"maxConcurrent": 10"#,
        );
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
}
