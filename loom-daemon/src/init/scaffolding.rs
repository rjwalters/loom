//! Repository scaffolding setup
//!
//! Sets up CLAUDE.md, AGENTS.md, .claude/, .codex/, and .github/ directories.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use super::file_ops::{
    copy_dir_with_report, copy_dir_with_report_filtered, force_merge_dir_with_report,
    force_merge_dir_with_report_filtered, merge_dir_with_report,
};
use super::git::extract_repo_info;
use super::templates::{assert_no_placeholders, substitute_template_variables, LoomMetadata};
use super::InitReport;

/// Name of the skip-list file under `defaults/` that lists Loom-internal
/// paths the installer must not ship to consumer repositories.
///
/// See [`load_internal_skip_list`] for the file format. Issue #3464.
pub const INTERNAL_SKIP_LIST_NAME: &str = ".loom-internal.list";

/// Prefix used to identify Loom-owned hooks in settings.json.
/// Hooks with commands starting with this prefix are managed by Loom.
///
/// Hooks are written with `${CLAUDE_PROJECT_DIR}/` prefix so that Claude Code
/// expands them at hook-invocation time to the project root, ensuring the
/// commands resolve regardless of the agent's current working directory.
#[allow(dead_code)]
pub const LOOM_HOOK_PREFIX: &str = "${CLAUDE_PROJECT_DIR}/.loom/hooks/";

/// Legacy prefix for hooks installed before the `${CLAUDE_PROJECT_DIR}` migration.
/// Used during merge/remove operations to detect and migrate stale entries.
#[allow(dead_code)]
pub const LEGACY_LOOM_HOOK_PREFIX: &str = ".loom/hooks/";

/// Substring marker identifying Loom's **machine-level** hook command form
/// (Epic #3835 Phase 5, #4262). Where [`LOOM_HOOK_PREFIX`] and
/// [`LEGACY_LOOM_HOOK_PREFIX`] are project-relative paths that start the
/// command, the machine-level form is a `bash -c '...'` wrapper (provisioned
/// into the *user-scope* `~/.claude/settings.json` by
/// `scripts/install/provision-hooks.sh`, not written by this scaffolding
/// module) that resolves and execs a hook script from the shared machine
/// checkout, e.g.:
///
/// ```text
/// bash -c 'R=$(...) || exit 0; ...; H="${LOOM_HOME:-$HOME/.local/share/loom}/defaults/hooks/guard-destructive.sh"; [ -x "$H" ] && exec "$H" || exit 0'
/// ```
///
/// The interesting path segment (`/defaults/hooks/`) appears mid-string, not
/// at the start, so recognition uses substring containment rather than
/// `starts_with`. Project-level `.claude/settings.json` never contains this
/// form today (it is user-scope only), but [`is_loom_hook_command`] still
/// recognizes it so a future merge/removal pass over project-level settings
/// (or a hand-copied entry) is not silently treated as a foreign hook.
#[allow(dead_code)]
pub const MACHINE_HOOK_MARKER: &str = "/defaults/hooks/";

/// Normalize a hook command string for semantic-duplicate comparison.
///
/// Loom-generated hook commands are a single `${CLAUDE_PROJECT_DIR}`-prefixed
/// path. Some installer generations wrapped that path in double quotes (to
/// survive a project path containing spaces); the current template emits it
/// unquoted. Byte-for-byte comparison treats
/// `"${CLAUDE_PROJECT_DIR}/.loom/hooks/foo.sh"` and
/// `${CLAUDE_PROJECT_DIR}/.loom/hooks/foo.sh` as different commands, so a
/// reinstall over a quoted-form install appended a second, functionally
/// identical hook entry on every run, and uninstall left the quoted entry
/// behind (issue #4200). Stripping quote characters and collapsing whitespace
/// before comparing treats them as the same hook without discarding either
/// side's original on-disk formatting -- this function is for comparison
/// only, never for rewriting a stored `command` value.
#[allow(dead_code)]
fn normalize_hook_command(cmd: &str) -> String {
    cmd.chars()
        .filter(|c| *c != '"' && *c != '\'')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns true if a command string belongs to Loom (matches new or legacy prefix).
///
/// The command is normalized (see [`normalize_hook_command`]) before the
/// prefix check so quoted-form entries (e.g. a path wrapped in `"..."` to
/// survive spaces) are still recognized as Loom-owned.
#[allow(dead_code)]
fn is_loom_hook_command(cmd: &str) -> bool {
    let normalized = normalize_hook_command(cmd);
    normalized.starts_with(LOOM_HOOK_PREFIX)
        || normalized.starts_with(LEGACY_LOOM_HOOK_PREFIX)
        || normalized.contains(MACHINE_HOOK_MARKER)
}

/// Loom section markers for CLAUDE.md content preservation
pub const LOOM_SECTION_START: &str = "<!-- BEGIN LOOM ORCHESTRATION -->";
pub const LOOM_SECTION_END: &str = "<!-- END LOOM ORCHESTRATION -->";

/// Loom-managed block markers for `.github/labels.yml` (issue #4187).
///
/// `.github/labels.yml` is the one scaffolding file a consumer legitimately
/// co-owns: Loom ships its 27 workflow labels, but a consumer may add their own
/// labels to the same file. Wrapping Loom's entries in these YAML-comment
/// markers lets install/upgrade/uninstall touch **only** Loom's range and never
/// clobber or orphan consumer-authored labels — the same marker-delimited
/// managed-section pattern already used for root `CLAUDE.md`
/// ([`LOOM_SECTION_START`]) and the `.gitignore` block.
///
/// The markers occupy their own comment lines in the shipped file; the line-
/// oriented `sync-labels.sh` parser treats them (like every `#` line) as inert.
pub const LOOM_LABELS_START: &str = "# BEGIN LOOM LABELS";
pub const LOOM_LABELS_END: &str = "# END LOOM LABELS";

/// The `.github`-relative path of the label registry, special-cased in the
/// scaffolding copy so its Loom-managed block is merged rather than the whole
/// file being clobbered (force) or frozen (merge). See [`install_labels_block`].
const LABELS_YML_REL: &str = ".github/labels.yml";

/// Extract the inclusive `# BEGIN LOOM LABELS` … `# END LOOM LABELS` block from
/// `content`, returning the byte range `[start, end)` that spans from the first
/// character of the BEGIN marker through the last character of the END marker
/// (the trailing newline after END, if any, is **not** included).
///
/// Returns `None` when the markers are absent or malformed (END before BEGIN),
/// so callers can fall back to append/preserve semantics rather than splicing a
/// nonsensical range.
fn labels_block_range(content: &str) -> Option<(usize, usize)> {
    let start = content.find(LOOM_LABELS_START)?;
    // Search for END only after BEGIN so a stray END above BEGIN can't invert
    // the range.
    let end_marker = content[start..].find(LOOM_LABELS_END)? + start;
    let end = end_marker + LOOM_LABELS_END.len();
    Some((start, end))
}

/// Compute the correct `.github/labels.yml` content for an install, preserving
/// all consumer-owned entries outside the Loom-managed marker block.
///
/// - **`existing` has a well-formed block** → replace only the marked range with
///   the shipped block; everything before/after is preserved byte-for-byte.
/// - **`existing` is markerless** (a legacy install, or a consumer file Loom has
///   never touched) → append the shipped block, preserving every existing entry.
/// - **`source` has no block** (defensive; the shipped file always does) →
///   return `existing` unchanged rather than risk clobbering consumer content.
///
/// The result is `None` when no change is needed (`existing` already equals the
/// computed content), letting the caller record the file as `preserved`.
fn merge_labels_block(existing: &str, source: &str) -> Option<String> {
    let Some((src_start, src_end)) = labels_block_range(source) else {
        // Shipped file unexpectedly lacks markers — never clobber the consumer.
        return None;
    };
    let source_block = &source[src_start..src_end];

    let merged = if let Some((dst_start, dst_end)) = labels_block_range(existing) {
        // Splice the shipped block over the consumer's marked range.
        format!("{}{}{}", &existing[..dst_start], source_block, &existing[dst_end..])
    } else {
        // Markerless consumer file: append the block, preserving all entries.
        let head = existing.trim_end_matches('\n');
        if head.is_empty() {
            format!("{source_block}\n")
        } else {
            format!("{head}\n\n{source_block}\n")
        }
    };

    if merged == existing {
        None
    } else {
        Some(merged)
    }
}

/// Install `.github/labels.yml` with Loom-block merge semantics (issue #4187).
///
/// `pre_existing` is the destination's content **before** the enclosing
/// `.github` directory copy ran (that copy may have clobbered it under `--force`
/// or left it frozen under merge — either way this function re-derives and writes
/// the authoritative content). Any label-registry entry the directory copy left
/// in the report is dropped and replaced with the correct one here.
fn install_labels_block(
    src: &Path,
    dst: &Path,
    pre_existing: Option<&str>,
    report: &mut InitReport,
) -> Result<(), String> {
    let source =
        fs::read_to_string(src).map_err(|e| format!("Failed to read labels.yml template: {e}"))?;

    // The directory copy above already recorded labels.yml somewhere — drop that
    // entry; this function is authoritative for the file.
    report.added.retain(|f| f != LABELS_YML_REL);
    report.updated.retain(|f| f != LABELS_YML_REL);
    report.preserved.retain(|f| f != LABELS_YML_REL);

    match pre_existing {
        None => {
            // Fresh install: ship the file verbatim (markers included), so the
            // two registry copies stay byte-identical (#3896).
            fs::write(dst, &source).map_err(|e| format!("Failed to write labels.yml: {e}"))?;
            report.added.push(LABELS_YML_REL.to_string());
        }
        Some(existing) => {
            match merge_labels_block(existing, &source) {
                Some(merged) => {
                    fs::write(dst, &merged)
                        .map_err(|e| format!("Failed to write labels.yml: {e}"))?;
                }
                None => {
                    // Content unchanged — but a `--force` directory copy may have
                    // overwritten the file, so restore the consumer's version.
                    fs::write(dst, existing)
                        .map_err(|e| format!("Failed to write labels.yml: {e}"))?;
                }
            }
            // Consumer-owned file: record as preserved so the post-install byte
            // verification (which expects installed == source) does not flag the
            // intentional divergence. See filter_preserved_from_verification_failures.
            report.preserved.push(LABELS_YML_REL.to_string());
        }
    }

    Ok(())
}

/// The short pointer injected into root CLAUDE.md (between section markers).
///
/// This block is committed to the consumer repo, so its authoritative reference
/// is the always-present Loom repository URL — never the install-generated
/// `.loom/CLAUDE.md`, which may be gitignored or absent in a fresh clone / CI
/// checkout (issue #3612). Loom additionally writes a locally-substituted copy
/// of the full guide to `.loom/CLAUDE.md` at install time; Claude Code
/// auto-discovers that local copy when agents work in `.loom/worktrees/issue-N/`
/// via ancestor directory traversal, so the auto-discovery behaviour is
/// unaffected by this wording.
pub const LOOM_ROOT_POINTER: &str = "This repository uses [Loom](https://github.com/rjwalters/loom) for AI-powered development orchestration — see the Loom repository for the full guide (roles, labels, worktrees, configuration). When installed, Loom also writes a locally-substituted copy of that guide to `.loom/CLAUDE.md`.";

/// Wrap Loom content in section markers
pub fn wrap_loom_content(content: &str) -> String {
    format!("{}\n{}\n{}", LOOM_SECTION_START, content.trim(), LOOM_SECTION_END)
}

/// Loom section markers for AGENTS.md content preservation (issue #4479,
/// epic #4167 — dual-runtime instruction anchor; seeded by gpeyton/loom fork
/// PR #8).
///
/// Deliberately a **separate** marker pair from [`LOOM_SECTION_START`] /
/// [`LOOM_SECTION_END`] (not reused) so a repo's CLAUDE.md and AGENTS.md
/// sections are independently detectable and replaceable. A repo could have
/// Loom-managed content in one and hand-authored content in the other; using
/// the same markers for both would let injection logic for one file
/// accidentally match markers belonging to the other.
pub const AGENTS_SECTION_START: &str = "<!-- BEGIN LOOM ORCHESTRATION (AGENTS) -->";
pub const AGENTS_SECTION_END: &str = "<!-- END LOOM ORCHESTRATION (AGENTS) -->";

/// The short pointer injected into root AGENTS.md (between section markers).
///
/// Like [`LOOM_ROOT_POINTER`], this block is committed to the consumer repo, so
/// its authoritative reference is the always-present Loom repository URL — never
/// the install-generated `.loom/AGENTS.md`, which may be gitignored or absent in
/// a fresh clone / CI checkout. The full runtime-neutral guide (generated from
/// `.loom/CLAUDE.md`'s `agents-md:include` ranges) is additionally written to
/// `.loom/AGENTS.md` at install time. OpenAI Codex CLI (and other AGENTS.md-aware
/// runtimes) auto-discover `AGENTS.md` via ancestor directory traversal, the
/// direct analogue of Claude Code's `CLAUDE.md` discovery.
pub const AGENTS_ROOT_POINTER: &str = "This repository uses [Loom](https://github.com/rjwalters/loom) for AI-powered development orchestration (dual-runtime: Claude Code reads `CLAUDE.md`; OpenAI Codex CLI and other AGENTS.md-aware runtimes read this file). See the Loom repository for the full guide (roles, labels, worktrees, configuration). When installed, Loom also writes a locally-substituted copy of the runtime-neutral guide to `.loom/AGENTS.md`.";

/// Wrap AGENTS.md content in its own section markers (kept separate from
/// [`wrap_loom_content`]/CLAUDE.md's markers — see [`AGENTS_SECTION_START`]).
pub fn wrap_agents_content(content: &str) -> String {
    format!("{}\n{}\n{}", AGENTS_SECTION_START, content.trim(), AGENTS_SECTION_END)
}

/// Telltale phrases that identify a root `CLAUDE.md` as Loom-managed legacy content.
///
/// Used by [`is_legacy_loom_managed_root`]. Any one of these phrases in a markerless
/// file is strong evidence that the file was generated by an older Loom installer
/// that wrote the full guide to root `CLAUDE.md` (the pre-#3000 layout), rather
/// than user-authored content.
///
/// Kept narrow on purpose: phrases that are extremely unlikely to appear in
/// hand-written project documentation. A bare mention of "loom" is not enough —
/// we require a phrase that specifically points at a Loom install.
const LEGACY_LOOM_SIGNATURES: &[&str] = &[
    // The old-layout root document header.
    "# Loom Orchestration - Repository Guide",
    // Footer line written by the old installer.
    "Generated by Loom Installation Process",
    // Metadata lines from the old layout.
    "**Loom Version**:",
    "**Loom Repository**: https://github.com/rjwalters/loom",
    // Unsubstituted template placeholders are themselves a strong signal — they
    // can only appear in a file written by the installer (real users don't type
    // `{{LOOM_VERSION}}` into their docs).
    "{{LOOM_VERSION}}",
    "{{INSTALL_DATE}}",
    "{{LOOM_COMMIT}}",
];

/// Return `true` if `existing_content` looks like a previous-generation
/// Loom-managed root `CLAUDE.md` that should be replaced on upgrade.
///
/// Heuristic (both conditions required):
///   1. The file does **not** contain the modern `LOOM_SECTION_START` marker.
///      (Modern installs wrap their content in markers; legacy installs don't.)
///   2. The file contains at least one phrase from [`LEGACY_LOOM_SIGNATURES`].
///
/// This separates three cases that the install path needs to handle differently:
///
/// | Case                                  | Markers? | Legacy signature? | Action               |
/// |---------------------------------------|----------|-------------------|----------------------|
/// | Modern marker-block install           | yes      | n/a               | replace section only |
/// | Legacy full-guide install (pre-#3000) | no       | yes               | replace entire file  |
/// | User-authored CLAUDE.md               | no       | no                | preserve, append     |
///
/// Without this heuristic, the legacy case is misclassified as user-authored,
/// leaving stale Loom-managed content (with unsubstituted `{{LOOM_VERSION}}`
/// placeholders) on disk forever. See issue #3325 for the regression that
/// prompted this.
fn is_legacy_loom_managed_root(existing_content: &str) -> bool {
    if existing_content.contains(LOOM_SECTION_START) {
        return false;
    }
    LEGACY_LOOM_SIGNATURES
        .iter()
        .any(|sig| existing_content.contains(sig))
}

/// Maximum line count for a slice to be treated as a bare legacy guide fragment
/// on the strength of a single legacy signature alone.
///
/// A genuinely legacy full-guide fragment (the pre-#3000 root `CLAUDE.md`, or a
/// #3476 hybrid where the legacy guide directly abuts the marker block) is a
/// bounded block — on the order of a few hundred lines at most, and typically
/// far fewer. This threshold is deliberately generous relative to the current
/// bare legacy guide (tens of lines) while remaining far below the size of a
/// long-lived consumer file that has accumulated hundreds of lines of real,
/// organically-added content around a surviving legacy-looking header.
const LEGACY_SLICE_MAX_LINES: usize = 200;

/// Return `true` if `slice` (the portion of a marker-bearing root `CLAUDE.md`
/// **outside** the marker block) is safe to discard as legacy Loom cruft.
///
/// This is a deliberately stricter test than [`is_legacy_loom_managed_root`].
/// The whole-file heuristic treats *any single* legacy signature as sufficient,
/// which is correct for a markerless file (there is no delimited user region to
/// lose, and a leaked `{{LOOM_VERSION}}` placeholder anywhere is decisive). But
/// in the marker-replace branch the slice may be a long-lived consumer file that
/// merely *starts* with a legacy-looking header line (e.g.
/// `# Loom Orchestration - Repository Guide`) followed by hundreds of lines of
/// real, hand-authored content. Discarding that slice deletes genuine consumer
/// content — the data-loss bug in #3527 (observed in bucket-brigade PR #480,
/// where a 1,015-line CLAUDE.md collapsed to the 3-line pointer stub).
///
/// A slice is only discardable when it is *predominantly* legacy boilerplate,
/// established by either:
///
///   1. **Multiple distinct signatures.** A genuine legacy guide carries several
///      independent markers (the header, `**Loom Version**:`, the
///      `Generated by Loom Installation Process` footer, and/or unsubstituted
///      `{{…}}` placeholders). A consumer file with one surviving header line
///      matches exactly one signature, so requiring ≥2 distinct matches cleanly
///      separates the two shapes.
///   2. **A short slice with any signature.** A bare legacy guide fragment
///      (≤ [`LEGACY_SLICE_MAX_LINES`] lines) that matches even one signature is
///      still overwhelmingly likely to be legacy cruft rather than a substantial
///      consumer document. This preserves the narrow #3476 hybrid case where the
///      legacy guide directly abuts the marker block.
///
/// When in doubt the slice is **preserved** — leftover Loom cruft is a far
/// milder failure than silently deleting consumer content.
fn slice_is_discardable_legacy(slice: &str) -> bool {
    if slice.contains(LOOM_SECTION_START) {
        return false;
    }

    let distinct_signatures = LEGACY_LOOM_SIGNATURES
        .iter()
        .filter(|sig| slice.contains(*sig))
        .count();

    if distinct_signatures == 0 {
        return false;
    }

    // Multiple independent signatures => almost certainly a genuine legacy guide.
    if distinct_signatures >= 2 {
        return true;
    }

    // A single signature is only decisive when the slice is small enough to be a
    // bare legacy fragment rather than a substantial consumer document.
    slice.lines().count() <= LEGACY_SLICE_MAX_LINES
}

/// Load the Loom-internal skip list from `<defaults>/.loom-internal.list`.
///
/// Returns a set of defaults-relative path strings (e.g.
/// `".claude/commands/loom/internal-only.md"`) that the installer must NOT
/// copy into consumer repositories.
///
/// File format:
/// - One defaults-relative path per line.
/// - Lines starting with `#` are comments and ignored.
/// - Blank lines are ignored.
/// - Leading/trailing whitespace on each entry is stripped.
/// - Paths are matched exactly against the defaults-relative path the
///   copy helpers see (e.g. `.claude/commands/loom/internal-only.md`). No
///   globbing.
///
/// Missing or unreadable files yield an empty set — the install path is
/// expected to function unchanged for repos that ship without a skip
/// list. Issue #3464.
pub fn load_internal_skip_list(defaults_path: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let path = defaults_path.join(INTERNAL_SKIP_LIST_NAME);
    let Ok(contents) = fs::read_to_string(&path) else {
        return set;
    };
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        set.insert(line.to_string());
    }
    set
}

/// Read and parse an existing settings.json file, returning None if missing or invalid.
fn read_existing_settings(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

/// Deep-merge Loom's default settings.json into an existing project settings.json.
///
/// Merge strategy:
/// - **Hooks**: For each hook type (e.g., `PreToolUse`), for each matcher entry,
///   merge Loom hooks alongside existing hooks. Deduplicates by command path.
///   Preserves all project hook types and matchers that Loom doesn't define.
/// - **Permissions**: Union of `permissions.allow` arrays (dedup exact strings).
/// - **Other keys**: Preserves all keys from the existing settings that Loom doesn't define.
pub fn merge_settings_json(existing: &Value, loom_defaults: &Value) -> Value {
    let mut result = existing.clone();
    let Some(result_obj) = result.as_object_mut() else {
        return loom_defaults.clone();
    };

    // Merge hooks
    if let Some(loom_hooks) = loom_defaults.get("hooks").and_then(|h| h.as_object()) {
        let merged_hooks =
            merge_hooks(existing.get("hooks").and_then(|h| h.as_object()), loom_hooks);
        result_obj.insert("hooks".to_string(), Value::Object(merged_hooks));
    }

    // Merge permissions
    if let Some(loom_perms) = loom_defaults.get("permissions").and_then(|p| p.as_object()) {
        let merged_perms =
            merge_permissions(existing.get("permissions").and_then(|p| p.as_object()), loom_perms);
        result_obj.insert("permissions".to_string(), Value::Object(merged_perms));
    }

    result
}

/// Merge hooks from Loom defaults into existing hooks.
///
/// For each hook type in Loom defaults:
///   - For each matcher entry, find matching entry in existing (same matcher value)
///     - If found: merge hooks arrays, deduplicating by command path
///     - If not found: add the entire matcher entry
///   - All existing hook types not in Loom defaults are preserved unchanged
fn merge_hooks(
    existing: Option<&serde_json::Map<String, Value>>,
    loom: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut result = existing.cloned().unwrap_or_default();

    for (hook_type, loom_matchers) in loom {
        let Some(loom_matchers_arr) = loom_matchers.as_array() else {
            continue;
        };

        let existing_matchers = result
            .entry(hook_type.clone())
            .or_insert_with(|| Value::Array(Vec::new()));

        let Some(existing_arr) = existing_matchers.as_array_mut() else {
            continue;
        };

        for loom_matcher_entry in loom_matchers_arr {
            let loom_matcher_val = loom_matcher_entry
                .get("matcher")
                .and_then(|m| m.as_str())
                .unwrap_or("");

            // Find matching entry in existing
            let found = existing_arr.iter_mut().find(|entry| {
                entry.get("matcher").and_then(|m| m.as_str()).unwrap_or("") == loom_matcher_val
            });

            if let Some(existing_entry) = found {
                // Merge hooks arrays within this matcher entry
                merge_hook_commands(existing_entry, loom_matcher_entry);
            } else {
                // No matching entry exists - add the entire matcher entry
                existing_arr.push(loom_matcher_entry.clone());
            }
        }
    }

    result
}

/// Merge hook commands within a single matcher entry, deduplicating by
/// semantically-normalized command (see [`normalize_hook_command`]).
///
/// Also strips legacy Loom hook entries (bare `.loom/hooks/...` paths from pre-3265
/// installs) so that re-running install does not leave duplicate hook invocations
/// alongside the new `${CLAUDE_PROJECT_DIR}/.loom/hooks/...` entries.
fn merge_hook_commands(existing_entry: &mut Value, loom_entry: &Value) {
    let Some(existing_hooks) = existing_entry
        .get_mut("hooks")
        .and_then(|h| h.as_array_mut())
    else {
        return;
    };

    let Some(loom_hooks) = loom_entry.get("hooks").and_then(|h| h.as_array()) else {
        return;
    };

    // First, strip legacy bare-relative Loom hooks so they don't coexist with the
    // new ${CLAUDE_PROJECT_DIR}-prefixed versions. We only strip the *legacy* prefix
    // here -- new-prefix entries are kept and serve as the dedup signal below.
    // The command is normalized (see [`normalize_hook_command`]) before the
    // prefix checks so a quoted legacy or current-form entry is still
    // recognized correctly.
    existing_hooks.retain(|h| {
        let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let normalized = normalize_hook_command(cmd);
        !normalized.starts_with(LEGACY_LOOM_HOOK_PREFIX) || normalized.starts_with(LOOM_HOOK_PREFIX)
    });

    // Collect existing command paths for dedup, normalized so quoted and
    // unquoted forms of the same command collide.
    let existing_commands: std::collections::HashSet<String> = existing_hooks
        .iter()
        .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
        .map(normalize_hook_command)
        .collect();

    // Add Loom hooks that aren't already present (by normalized command).
    // Note: the original `loom_hook` value (unnormalized) is what gets
    // pushed -- normalization is comparison-only and never rewrites what's
    // stored in the merged output.
    for loom_hook in loom_hooks {
        let cmd = loom_hook
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if !existing_commands.contains(&normalize_hook_command(cmd)) {
            existing_hooks.push(loom_hook.clone());
        }
    }
}

/// Merge permissions, unioning the allow arrays.
fn merge_permissions(
    existing: Option<&serde_json::Map<String, Value>>,
    loom: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut result = existing.cloned().unwrap_or_default();

    if let Some(loom_allow) = loom.get("allow").and_then(|a| a.as_array()) {
        let existing_allow = result
            .entry("allow".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));

        if let Some(existing_arr) = existing_allow.as_array_mut() {
            let existing_set: std::collections::HashSet<String> = existing_arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();

            for perm in loom_allow {
                if let Some(perm_str) = perm.as_str() {
                    if !existing_set.contains(perm_str) {
                        existing_arr.push(perm.clone());
                    }
                }
            }
        }
    }

    result
}

/// Remove Loom-owned hooks from a settings.json value.
///
/// Loom hooks are identified by command paths starting with either the new
/// `${CLAUDE_PROJECT_DIR}/.loom/hooks/` prefix or the legacy `.loom/hooks/`
/// prefix (pre-3265 installs). Both are stripped so uninstall is clean for
/// users on any prior version.
///
/// After removal, empty matcher entries and empty hook type arrays are cleaned up.
#[allow(dead_code)]
pub fn remove_loom_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };

    // Process each hook type
    let hook_types: Vec<String> = hooks.keys().cloned().collect();
    for hook_type in &hook_types {
        let Some(matchers) = hooks.get_mut(hook_type).and_then(|m| m.as_array_mut()) else {
            continue;
        };

        // For each matcher entry, remove Loom hooks from the hooks array
        for matcher_entry in matchers.iter_mut() {
            if let Some(hook_arr) = matcher_entry
                .get_mut("hooks")
                .and_then(|h| h.as_array_mut())
            {
                hook_arr.retain(|hook| {
                    let cmd = hook.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    !is_loom_hook_command(cmd)
                });
            }
        }

        // Remove matcher entries with empty hooks arrays
        matchers.retain(|entry| {
            !entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(Vec::is_empty)
        });
    }

    // Remove hook types with empty matcher arrays
    hooks.retain(|_, v| !v.as_array().is_some_and(Vec::is_empty));

    // If hooks object is now empty, remove it entirely
    if hooks.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }
}

/// Remove Loom-specific permissions from a settings.json value.
///
/// Removes permissions that match Loom's default permission list exactly.
#[allow(dead_code)]
pub fn remove_loom_permissions(settings: &mut Value, loom_defaults: &Value) {
    let Some(loom_perms) = loom_defaults
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
    else {
        return;
    };

    let loom_perm_set: std::collections::HashSet<&str> =
        loom_perms.iter().filter_map(|v| v.as_str()).collect();

    let Some(allow) = settings
        .get_mut("permissions")
        .and_then(|p| p.get_mut("allow"))
        .and_then(|a| a.as_array_mut())
    else {
        return;
    };

    allow.retain(|v| !v.as_str().is_some_and(|s| loom_perm_set.contains(s)));

    // Clean up empty permissions
    if allow.is_empty() {
        if let Some(perms) = settings
            .get_mut("permissions")
            .and_then(|p| p.as_object_mut())
        {
            perms.remove("allow");
        }
    }
    if settings
        .get("permissions")
        .and_then(|p| p.as_object())
        .is_some_and(serde_json::Map::is_empty)
    {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("permissions");
        }
    }
}

/// Setup repository scaffolding files
///
/// Copies CLAUDE.md, AGENTS.md, .claude/, .codex/, and .github/ to the workspace.
/// - Fresh install: Copies all files from defaults
/// - Reinstall without force (merge mode): Adds new files, preserves ALL existing files
/// - Reinstall with force (force-merge mode): Updates default files, preserves custom files
/// - Template variables: Substitutes variables in CLAUDE.md / AGENTS.md
///   - `{{REPO_OWNER}}`, `{{REPO_NAME}}`: Repository info from git remote
///   - `{{LOOM_VERSION}}`, `{{LOOM_COMMIT}}`, `{{INSTALL_DATE}}`: Loom installation metadata
///
/// **AGENTS.md Handling** (issue #4479): identical mechanics to CLAUDE.md below
/// (full guide in `.loom/AGENTS.md`, short pointer in root `AGENTS.md`), but with
/// its own `AGENTS_SECTION_START`/`AGENTS_SECTION_END` marker pair so the two
/// files' Loom-managed sections never cross-contaminate.
///
/// **CLAUDE.md Handling**:
/// - Full Loom guide is written to `<workspace>/.loom/CLAUDE.md` (with template substitution)
/// - Only a short pointer is injected into root `CLAUDE.md` (between Loom section markers)
/// - If existing root CLAUDE.md has Loom section markers, only the marked section is replaced
/// - If existing root CLAUDE.md has no markers, Loom pointer is appended at the end
/// - All existing root CLAUDE.md content is preserved exactly as-is
/// - Claude Code auto-discovers `.loom/CLAUDE.md` in `.loom/worktrees/issue-N/` via ancestor dirs
///
/// Custom files (files in workspace that don't exist in defaults) are always preserved.
#[allow(clippy::too_many_lines)]
pub fn setup_repository_scaffolding(
    workspace_path: &Path,
    defaults_path: &Path,
    force: bool,
    report: &mut InitReport,
) -> Result<(), String> {
    // Extract repository owner and name for template substitution
    let repo_info = extract_repo_info(workspace_path);
    let (repo_owner, repo_name) = match repo_info {
        Some((owner, name)) => (Some(owner), Some(name)),
        None => (None, None),
    };

    // Get Loom installation metadata from environment variables
    let loom_metadata = LoomMetadata::from_env();

    // Helper to copy directory with force logic and reporting
    // - Fresh install (dst doesn't exist): copy all
    // - Reinstall without force: merge (add new, preserve existing)
    // - Reinstall with force: force-merge (update defaults, preserve custom)
    let copy_directory =
        |src: &Path, dst: &Path, name: &str, report: &mut InitReport| -> Result<(), String> {
            if src.exists() {
                if !dst.exists() {
                    // Fresh install: copy all
                    copy_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to copy {name}: {e}"))?;
                } else if force {
                    // Force reinstall: update defaults, preserve custom files
                    force_merge_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to force-merge {name}: {e}"))?;
                } else {
                    // Merge reinstall: add new files only, preserve all existing
                    merge_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to merge {name}: {e}"))?;
                }
            }
            Ok(())
        };

    // Handle Loom CLAUDE.md content:
    //
    // 1. Write full Loom guide to `<workspace>/.loom/CLAUDE.md` (template substituted)
    //    - Claude Code discovers this automatically when agents work in worktrees
    //    - Always written on install/reinstall (overwrite on reinstall to get latest content)
    //
    // 2. Inject short pointer into root `CLAUDE.md` (between Loom section markers)
    //    - Keeps root CLAUDE.md minimal — saves context budget for non-Loom sessions
    //    - If existing root has markers, only the marked section is replaced
    //    - If existing root has no markers, pointer is appended at the end
    let claude_md_src = defaults_path.join(".loom").join("CLAUDE.md");

    if claude_md_src.exists() {
        // Read the Loom template content
        let loom_content = fs::read_to_string(&claude_md_src)
            .map_err(|e| format!("Failed to read CLAUDE.md template: {e}"))?;

        // Substitute template variables in Loom content
        let loom_substituted = substitute_template_variables(
            &loom_content,
            repo_owner.as_deref(),
            repo_name.as_deref(),
            &loom_metadata,
        );

        // --- Step 1: Write full guide to .loom/CLAUDE.md ---
        let loom_dir = workspace_path.join(".loom");
        // .loom/ should already exist (created earlier in initialize_workspace),
        // but create it if it doesn't to be safe.
        if !loom_dir.exists() {
            fs::create_dir_all(&loom_dir)
                .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
        }
        let loom_claude_md_dst = loom_dir.join("CLAUDE.md");
        let loom_claude_md_existed = loom_claude_md_dst.exists();
        fs::write(&loom_claude_md_dst, &loom_substituted)
            .map_err(|e| format!("Failed to write .loom/CLAUDE.md: {e}"))?;
        if loom_claude_md_existed {
            report.updated.push(".loom/CLAUDE.md".to_string());
        } else {
            report.added.push(".loom/CLAUDE.md".to_string());
        }

        // --- Step 2: Inject short pointer into root CLAUDE.md ---
        let claude_md_dst = workspace_path.join("CLAUDE.md");
        let existed = claude_md_dst.exists();

        // The pointer is a single-line description wrapped in section markers
        let wrapped_pointer = wrap_loom_content(LOOM_ROOT_POINTER);

        let final_content = if existed {
            // Read existing content
            let existing_content = fs::read_to_string(&claude_md_dst)
                .map_err(|e| format!("Failed to read existing CLAUDE.md: {e}"))?;

            // Check if existing file already has Loom section markers
            if existing_content.contains(LOOM_SECTION_START) {
                // Replace just the Loom section with the pointer, preserve everything else
                if let (Some(start_idx), Some(end_idx)) = (
                    existing_content.find(LOOM_SECTION_START),
                    existing_content.find(LOOM_SECTION_END),
                ) {
                    let before = &existing_content[..start_idx];
                    let after_end = end_idx + LOOM_SECTION_END.len();
                    let after = if after_end < existing_content.len() {
                        &existing_content[after_end..]
                    } else {
                        ""
                    };

                    // Hybrid legacy file (issue #3476): the v0.7.1 installer
                    // wrote the FULL legacy guide (with unsubstituted
                    // `{{LOOM_VERSION}}` etc.) to root CLAUDE.md AND appended
                    // the modern marker block. Preserving the before/after
                    // portions verbatim would carry the legacy content — and
                    // its leaked placeholders — forward, tripping the
                    // `assert_no_placeholders` guard below and aborting the
                    // upgrade. When a slice is *predominantly* legacy
                    // boilerplate, replace the entire file with the wrapped
                    // pointer instead of preserving the legacy text.
                    //
                    // Use `slice_is_discardable_legacy` — NOT the whole-file
                    // `is_legacy_loom_managed_root` — here (issue #3527). The
                    // whole-file heuristic discards a slice on a *single*
                    // signature match, which silently deleted ~1,000 lines of
                    // real consumer content from a long-lived CLAUDE.md whose
                    // line 1 merely matched the legacy header (bucket-brigade
                    // PR #480). The stricter slice check requires the slice to
                    // be mostly legacy (multiple signatures, or short + one
                    // signature) before discarding it, preserving genuine
                    // consumer content around the marker block.
                    if slice_is_discardable_legacy(before) || slice_is_discardable_legacy(after) {
                        wrapped_pointer.clone()
                    } else {
                        format!("{}{}{}", before.trim_end(), wrapped_pointer, after)
                    }
                } else {
                    // Malformed markers - append pointer at end
                    format!("{}\n\n{}", existing_content.trim(), wrapped_pointer)
                }
            } else if is_legacy_loom_managed_root(&existing_content) {
                // Legacy install (pre-#3000) wrote the full Loom guide to root
                // CLAUDE.md, often with unsubstituted `{{LOOM_VERSION}}` etc.
                // The "no markers" branch below would treat that as user content
                // and preserve it forever — leaving stale Loom-managed text and
                // leaked template placeholders on disk (issue #3325).
                //
                // Replace the entire file with the modern marker block. We do
                // not try to preserve fragments around the legacy content
                // because there are no markers delimiting it from anything else.
                wrapped_pointer.clone()
            } else {
                // No markers, no legacy signature — append Loom pointer at end
                // to preserve genuine user-authored content.
                format!("{}\n\n{}", existing_content.trim(), wrapped_pointer)
            }
        } else {
            // New file - just use wrapped pointer
            wrapped_pointer
        };

        // Defense-in-depth: refuse to write a root CLAUDE.md that still contains
        // unsubstituted template placeholders. This catches regressions in the
        // legacy-detection heuristic above and any future code paths that forget
        // to substitute. See issue #3325.
        assert_no_placeholders(&final_content, "CLAUDE.md")?;

        // Only write if we're creating new or content changed
        if existed {
            let current = fs::read_to_string(&claude_md_dst).unwrap_or_default();
            if final_content != current {
                fs::write(&claude_md_dst, &final_content)
                    .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
                if !report.preserved.contains(&"CLAUDE.md".to_string()) {
                    report.updated.push("CLAUDE.md".to_string());
                }
            } else if !report.preserved.contains(&"CLAUDE.md".to_string()) {
                report.preserved.push("CLAUDE.md".to_string());
            }
        } else {
            fs::write(&claude_md_dst, &final_content)
                .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
            report.added.push("CLAUDE.md".to_string());
        }
    }

    // Handle Loom AGENTS.md content (issue #4479, epic #4167 — dual-runtime
    // instruction anchor; seeded by gpeyton/loom fork PR #8).
    //
    // Mirrors the CLAUDE.md handling above, but with its own marker pair
    // (AGENTS_SECTION_START/END) so the two files' Loom-managed sections are
    // independently detectable. AGENTS.md has no historical "legacy
    // full-guide-in-root" layout to migrate away from (unlike CLAUDE.md's
    // pre-#3000 layout), so no `is_legacy_loom_managed_root`-style heuristic is
    // needed for it.
    //
    // `defaults/.loom/AGENTS.md` is itself a generated artifact
    // (defaults/scripts/generate-agents-md.sh, kept in sync by
    // scripts/check-agents-md-sync.sh); this code reads it exactly like the
    // CLAUDE.md template.
    //
    // 1. Write full Loom guide to `<workspace>/.loom/AGENTS.md` (template substituted)
    // 2. Inject short pointer into root `AGENTS.md` (between AGENTS section markers)
    let agents_md_src = defaults_path.join(".loom").join("AGENTS.md");

    if agents_md_src.exists() {
        let agents_content = fs::read_to_string(&agents_md_src)
            .map_err(|e| format!("Failed to read AGENTS.md template: {e}"))?;

        let agents_substituted = substitute_template_variables(
            &agents_content,
            repo_owner.as_deref(),
            repo_name.as_deref(),
            &loom_metadata,
        );

        // --- Step 1: Write full guide to .loom/AGENTS.md ---
        let loom_dir = workspace_path.join(".loom");
        if !loom_dir.exists() {
            fs::create_dir_all(&loom_dir)
                .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
        }
        let loom_agents_md_dst = loom_dir.join("AGENTS.md");
        let loom_agents_md_existed = loom_agents_md_dst.exists();
        fs::write(&loom_agents_md_dst, &agents_substituted)
            .map_err(|e| format!("Failed to write .loom/AGENTS.md: {e}"))?;
        if loom_agents_md_existed {
            report.updated.push(".loom/AGENTS.md".to_string());
        } else {
            report.added.push(".loom/AGENTS.md".to_string());
        }

        // --- Step 2: Inject short pointer into root AGENTS.md ---
        let agents_md_dst = workspace_path.join("AGENTS.md");
        let existed = agents_md_dst.exists();

        let wrapped_agents_pointer = wrap_agents_content(AGENTS_ROOT_POINTER);

        let final_agents_content = if existed {
            let existing_content = fs::read_to_string(&agents_md_dst)
                .map_err(|e| format!("Failed to read existing AGENTS.md: {e}"))?;

            if existing_content.contains(AGENTS_SECTION_START) {
                // Replace just the Loom section with the pointer, preserve everything else.
                if let (Some(start_idx), Some(end_idx)) = (
                    existing_content.find(AGENTS_SECTION_START),
                    existing_content.find(AGENTS_SECTION_END),
                ) {
                    let before = &existing_content[..start_idx];
                    let after_end = end_idx + AGENTS_SECTION_END.len();
                    let after = if after_end < existing_content.len() {
                        &existing_content[after_end..]
                    } else {
                        ""
                    };
                    // Same hybrid-legacy hazard CLAUDE.md guards against
                    // (issue #3476/#3527): if the slice outside the marker
                    // block is itself leftover Loom-managed cruft — e.g. from
                    // a previously interrupted install that left unsubstituted
                    // `{{LOOM_VERSION}}` text lying around — preserving it
                    // verbatim reintroduces the placeholders and trips the
                    // `assert_no_placeholders` guard below (issue #4888).
                    if slice_is_discardable_legacy(before) || slice_is_discardable_legacy(after) {
                        wrapped_agents_pointer.clone()
                    } else {
                        format!("{}{}{}", before.trim_end(), wrapped_agents_pointer, after)
                    }
                } else {
                    // Malformed markers (only one of START/END present) - this
                    // is exactly the shape a broken/interrupted prior install
                    // can leave behind. Treat it the same as the no-markers
                    // case below rather than blindly preserving it (issue
                    // #4888): discard if it looks like leftover Loom-managed
                    // content, otherwise preserve and append.
                    if is_legacy_loom_managed_root(&existing_content) {
                        wrapped_agents_pointer.clone()
                    } else {
                        format!("{}\n\n{}", existing_content.trim(), wrapped_agents_pointer)
                    }
                }
            } else if is_legacy_loom_managed_root(&existing_content) {
                // No markers, but the content matches a known Loom-managed
                // legacy/leftover signature (most tellingly, unsubstituted
                // `{{LOOM_VERSION}}`-style placeholders — real users don't
                // type those into hand-authored docs). This can happen even
                // though AGENTS.md itself has no historical full-guide-in-root
                // layout: a broken or interrupted prior install can leave a
                // markerless root AGENTS.md carrying stale Loom template text
                // (issue #4888). Discard rather than preserve-and-leak.
                wrapped_agents_pointer.clone()
            } else {
                // No markers, no legacy signature — preserve genuine
                // user-authored content, append at end.
                format!("{}\n\n{}", existing_content.trim(), wrapped_agents_pointer)
            }
        } else {
            // New file - just use wrapped pointer.
            wrapped_agents_pointer
        };

        // Defense-in-depth: refuse to write a root AGENTS.md that still
        // contains unsubstituted template placeholders (mirrors the CLAUDE.md
        // guard above; see issue #3325 for the original rationale).
        assert_no_placeholders(&final_agents_content, "AGENTS.md")?;

        if existed {
            let current = fs::read_to_string(&agents_md_dst).unwrap_or_default();
            if final_agents_content != current {
                fs::write(&agents_md_dst, &final_agents_content)
                    .map_err(|e| format!("Failed to write AGENTS.md: {e}"))?;
                if !report.preserved.contains(&"AGENTS.md".to_string()) {
                    report.updated.push("AGENTS.md".to_string());
                }
            } else if !report.preserved.contains(&"AGENTS.md".to_string()) {
                report.preserved.push("AGENTS.md".to_string());
            }
        } else {
            fs::write(&agents_md_dst, &final_agents_content)
                .map_err(|e| format!("Failed to write AGENTS.md: {e}"))?;
            report.added.push("AGENTS.md".to_string());
        }
    }

    // Copy .claude/ directory - always update default commands, preserve custom commands
    // - Fresh install: copy all from defaults
    // - Reinstall: always force-merge (update defaults, preserve custom)
    //
    // This ensures command updates from loom propagate to target repos while
    // preserving any custom commands the project has added.
    // Consistent with .loom/roles/ and .loom/scripts/ behavior.
    //
    // Special handling for settings.json: deep-merge hooks and permissions
    // instead of overwriting, so project-specific hooks are preserved.
    //
    // Issue #3464: skip files listed in `defaults/.loom-internal.list` so
    // Loom-internal skills (e.g. `.claude/commands/loom/internal-only.md`) are
    // not shipped to consumer repositories. The skip list is loaded once and the
    // closure does a HashSet lookup per file. An empty list (or missing file)
    // is a no-op.
    let skip_list = load_internal_skip_list(defaults_path);
    let skip_predicate = |rel_path: &str| -> bool { skip_list.contains(rel_path) };
    let claude_src = defaults_path.join(".claude");
    let claude_dst = workspace_path.join(".claude");
    if claude_src.exists() {
        // Save existing settings.json before directory copy overwrites it
        let existing_settings = read_existing_settings(&claude_dst.join("settings.json"));

        if claude_dst.exists() {
            // Reinstall: always force-merge to update default commands
            // Custom commands (files not in defaults) are preserved
            force_merge_dir_with_report_filtered(
                &claude_src,
                &claude_dst,
                ".claude",
                report,
                &skip_predicate,
            )
            .map_err(|e| format!("Failed to force-merge .claude directory: {e}"))?;
        } else {
            // Fresh install: copy all
            copy_dir_with_report_filtered(
                &claude_src,
                &claude_dst,
                ".claude",
                report,
                &skip_predicate,
            )
            .map_err(|e| format!("Failed to copy .claude directory: {e}"))?;
        }

        // If there was an existing settings.json, merge Loom's defaults into it
        // instead of using the overwritten copy
        if let Some(existing) = existing_settings {
            let settings_path = claude_dst.join("settings.json");
            let loom_defaults = read_existing_settings(&settings_path);
            if let Some(loom) = loom_defaults {
                let merged = merge_settings_json(&existing, &loom);
                if let Ok(pretty) = serde_json::to_string_pretty(&merged) {
                    if let Err(e) = fs::write(&settings_path, pretty) {
                        eprintln!("Warning: Failed to write merged settings.json: {e}");
                    }
                }
            }
        }
    }

    // Copy .codex/ directory
    copy_directory(
        &defaults_path.join(".codex"),
        &workspace_path.join(".codex"),
        ".codex",
        report,
    )?;

    // Copy .github/ directory.
    //
    // `.github/labels.yml` is special-cased (issue #4187): capture its
    // destination content BEFORE the directory copy (which clobbers it under
    // --force / freezes it under merge), then re-derive the authoritative
    // content via install_labels_block so only Loom's BEGIN/END LOOM LABELS
    // block is (re)written and consumer-authored labels outside it survive.
    let labels_src = defaults_path.join(LABELS_YML_REL);
    let labels_dst = workspace_path.join(LABELS_YML_REL);
    let pre_existing_labels = fs::read_to_string(&labels_dst).ok();

    copy_directory(
        &defaults_path.join(".github"),
        &workspace_path.join(".github"),
        ".github",
        report,
    )?;

    if labels_src.exists() {
        install_labels_block(&labels_src, &labels_dst, pre_existing_labels.as_deref(), report)?;
    }

    // Note: The label-external-issues.yml workflow is no longer installed by default.
    // It generated spammy "No jobs were run" emails in single-contributor repos.
    // The workflow is available in defaults/optional/github-workflows/ for manual installation.

    // Note: scripts/ is now copied earlier in initialize_workspace()
    // to .loom/scripts/ along with other .loom-specific files

    // Copy package.json ONLY if workspace doesn't have one
    // (never overwrite existing package.json, even in force mode)
    // This provides stub scripts for pnpm commands referenced in roles
    let package_json_src = defaults_path.join("package.json");
    let package_json_dst = workspace_path.join("package.json");
    if package_json_src.exists() && !package_json_dst.exists() {
        fs::copy(&package_json_src, &package_json_dst)
            .map_err(|e| format!("Failed to copy package.json: {e}"))?;
    }

    // Install loom.sh convenience wrapper at repo root (always update from defaults)
    // This is a thin wrapper around .loom/scripts/start-daemon.sh (the tmux
    // agent-pool path) that lets users run `./loom.sh` from the repo root
    // instead of the full script path.
    let loom_sh_src = defaults_path.join("loom.sh");
    let loom_sh_dst = workspace_path.join("loom.sh");
    if loom_sh_src.exists() {
        fs::copy(&loom_sh_src, &loom_sh_dst).map_err(|e| format!("Failed to copy loom.sh: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&loom_sh_dst)
                .map_err(|e| format!("Failed to read loom.sh metadata: {e}"))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&loom_sh_dst, perms)
                .map_err(|e| format!("Failed to make loom.sh executable: {e}"))?;
        }
        report.updated.push("loom.sh".to_string());
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_wrap_loom_content() {
        let content = "# Loom Orchestration\n\nLoom content here.";
        let wrapped = wrap_loom_content(content);

        assert!(wrapped.starts_with(LOOM_SECTION_START));
        assert!(wrapped.ends_with(LOOM_SECTION_END));
        assert!(wrapped.contains("Loom content here"));
    }

    #[test]
    fn test_load_internal_skip_list_missing_file() {
        // Missing skip-list file yields an empty set so existing repos that
        // ship without one keep their current behavior.
        let temp_dir = TempDir::new().unwrap();
        let set = load_internal_skip_list(temp_dir.path());
        assert!(set.is_empty());
    }

    #[test]
    fn test_load_internal_skip_list_parses_entries() {
        // Issue #3464: confirm comment lines, blank lines, and surrounding
        // whitespace are handled correctly so the file is operator-editable
        // without surprise behavior.
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join(INTERNAL_SKIP_LIST_NAME),
            "# Loom-internal files\n\
             \n\
             .claude/commands/loom/release.md\n\
             \n\
             # second-section comment\n\
             .claude/commands/loom/some-other.md  \n",
        )
        .unwrap();

        let set = load_internal_skip_list(temp_dir.path());
        assert_eq!(set.len(), 2);
        assert!(set.contains(".claude/commands/loom/release.md"));
        assert!(set.contains(".claude/commands/loom/some-other.md"));
    }

    #[test]
    fn test_setup_repository_scaffolding_skips_internal_files() {
        // End-to-end coverage of issue #3464: when defaults/.loom-internal.list
        // lists a path under .claude/, the installer must skip it on both
        // fresh install and reinstall, while sibling commands continue to
        // ship.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();

        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("builder.md"),
            "builder command",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("judge.md"),
            "judge command",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("release.md"),
            "loom-internal release skill",
        )
        .unwrap();

        // Skip-list excludes release.md.
        fs::write(
            defaults.join(INTERNAL_SKIP_LIST_NAME),
            "# header\n.claude/commands/loom/release.md\n",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // release.md must NOT be in the consumer's installed tree.
        assert!(
            !workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("release.md")
                .exists(),
            "issue #3464: .claude/commands/loom/release.md must be skipped on install"
        );
        // The siblings must still be installed verbatim.
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md")
            .exists());
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("judge.md")
            .exists());

        // Reinstall: same outcome, plus a stale local copy (if one exists)
        // is left in place rather than being overwritten or deleted.
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("release.md"),
            "stale local copy",
        )
        .unwrap();
        let mut report2 = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report2).unwrap();
        let local = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("release.md"),
        )
        .unwrap();
        assert_eq!(
            local, "stale local copy",
            "skip rule must leave pre-existing local copies untouched"
        );
        // And the report must not list release.md as added/updated.
        assert!(!report2
            .added
            .iter()
            .chain(report2.updated.iter())
            .any(|p| p == ".claude/commands/loom/release.md"));
    }

    #[test]
    fn test_setup_repository_scaffolding_force_mode() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults directory with .claude commands in loom/ subdirectory
        fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
            "loom command from defaults",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("builder.md"),
            "builder command from defaults",
        )
        .unwrap();

        // Create existing .claude directory in workspace with custom commands
        fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("custom.md"),
            "my custom command",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
            "old loom command",
        )
        .unwrap();

        // Run setup with force=true (force-merge mode)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify custom.md was PRESERVED (custom file not in defaults)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("custom.md")
            .exists());
        let custom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md"))
                .unwrap();
        assert_eq!(custom_content, "my custom command");

        // Verify loom.md was UPDATED with new content (default file)
        let loom_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
        )
        .unwrap();
        assert_eq!(loom_content, "loom command from defaults");

        // Verify builder.md was ADDED (new file from defaults)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md")
            .exists());
    }

    #[test]
    fn test_setup_repository_scaffolding_merge_mode() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults directory with .claude commands in loom/ subdirectory
        fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
            "loom command from defaults",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("builder.md"),
            "builder command from defaults",
        )
        .unwrap();

        // Create existing .claude directory in workspace with custom commands
        fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("custom.md"),
            "my custom command",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
            "custom loom command",
        )
        .unwrap();

        // Run setup with force=false (merge mode for .codex/.github, but .claude/ always force-merges)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify custom.md still exists (preserved)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("custom.md")
            .exists());
        let custom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md"))
                .unwrap();
        assert_eq!(custom_content, "my custom command");

        // Verify loom.md was UPDATED with new content (default file)
        // .claude/ always force-merges on reinstall to propagate command updates
        let loom_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
        )
        .unwrap();
        assert_eq!(loom_content, "loom command from defaults");

        // Verify builder.md was added (new file)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("builder.md")
            .exists());
        let builder_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("builder.md"),
        )
        .unwrap();
        assert_eq!(builder_content, "builder command from defaults");
    }

    #[test]
    fn test_package_json_copied_when_missing() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with package.json
        fs::create_dir_all(&defaults).unwrap();
        fs::write(
            defaults.join("package.json"),
            r#"{"name": "loom-workspace", "scripts": {"test": "echo test"}}"#,
        )
        .unwrap();

        // Workspace has no package.json initially
        assert!(!workspace.join("package.json").exists());

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify package.json was copied
        assert!(workspace.join("package.json").exists());
        let content = fs::read_to_string(workspace.join("package.json")).unwrap();
        assert!(content.contains("loom-workspace"));
    }

    #[test]
    fn test_package_json_preserved_when_exists() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with package.json
        fs::create_dir_all(&defaults).unwrap();
        fs::write(
            defaults.join("package.json"),
            r#"{"name": "loom-workspace", "scripts": {"test": "echo test"}}"#,
        )
        .unwrap();

        // Create existing package.json in workspace (project-specific)
        fs::write(
            workspace.join("package.json"),
            r#"{"name": "my-rust-project", "scripts": {"build": "cargo build"}}"#,
        )
        .unwrap();

        // Run setup with force=true (should STILL preserve package.json)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify package.json was NOT overwritten
        let content = fs::read_to_string(workspace.join("package.json")).unwrap();
        assert!(content.contains("my-rust-project"));
        assert!(!content.contains("loom-workspace"));
    }

    /// Helper to create a standard test setup with a CLAUDE.md template in defaults
    fn setup_test_with_claude_template(
        temp_dir: &TempDir,
        template_content: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let workspace = temp_dir.path().to_path_buf();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(defaults.join(".loom").join("CLAUDE.md"), template_content).unwrap();

        (workspace, defaults)
    }

    #[test]
    fn test_loom_claude_md_written_to_loom_dir() {
        // Verifies full content goes to .loom/CLAUDE.md on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content here.",
        );

        // Pre-create .loom/ dir (as initialize_workspace normally does)
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        // Verify .loom/CLAUDE.md was created with full guide content
        assert!(workspace.join(".loom").join("CLAUDE.md").exists());
        let loom_claude_content =
            fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_claude_content.contains("Loom Orchestration - Repository Guide"));
        assert!(loom_claude_content.contains("Full guide content here"));
        assert!(report.added.contains(&".loom/CLAUDE.md".to_string()));
    }

    #[test]
    fn test_root_claude_md_contains_only_pointer() {
        // Verifies root CLAUDE.md has short pointer, not full guide, on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content here.",
        );

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // No existing root CLAUDE.md
        assert!(!workspace.join("CLAUDE.md").exists());

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        // Verify root CLAUDE.md has only the pointer, not the full guide
        assert!(workspace.join("CLAUDE.md").exists());
        let root_content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(root_content.contains(LOOM_SECTION_START));
        assert!(root_content.contains(LOOM_SECTION_END));
        assert!(root_content.contains(LOOM_ROOT_POINTER));
        // Full guide content must NOT be in root CLAUDE.md
        assert!(!root_content.contains("Full guide content here"));
        assert!(report.added.contains(&"CLAUDE.md".to_string()));
    }

    #[test]
    fn test_claude_md_preservation_new_install() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nLoom content here.",
        )
        .unwrap();

        // Pre-create .loom/ dir
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // No existing root CLAUDE.md in workspace
        assert!(!workspace.join("CLAUDE.md").exists());

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify root CLAUDE.md was created with section markers and short pointer only
        assert!(workspace.join("CLAUDE.md").exists());
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full content must be absent from root
        assert!(!content.contains("Loom content here"));
        assert!(report.added.contains(&"CLAUDE.md".to_string()));

        // Verify .loom/CLAUDE.md was created with full content
        assert!(workspace.join(".loom").join("CLAUDE.md").exists());
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("Loom content here"));
    }

    #[test]
    fn test_claude_md_preservation_existing_project_content() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nNew Loom content.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md with project-specific content (no markers)
        fs::write(
            workspace.join("CLAUDE.md"),
            r"# My Awesome Project

This project does amazing things with Rust.

## Getting Started

Run `cargo run` to start.",
        )
        .unwrap();

        // Run setup - Loom pointer should be appended at end
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify existing content was preserved and Loom pointer appended
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Awesome Project"));
        assert!(content.contains("amazing things with Rust"));
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full Loom guide must NOT be in root
        assert!(!content.contains("New Loom content"));

        // Project content should come BEFORE Loom section (appended at end)
        let project_pos = content.find("My Awesome Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // No duplicate content
        assert_eq!(content.matches("My Awesome Project").count(), 1);
    }

    #[test]
    fn test_claude_md_append_when_no_markers() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nLoom content here.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md WITHOUT markers (e.g., from previous install or manual creation)
        fs::write(
            workspace.join("CLAUDE.md"),
            r"# Lean Genius Project

Formal mathematics in Lean 4.

## Docker Build Safety

WARNING: Never run `lake build` inside Docker - causes memory corruption.

## Custom Agents

- Erdos: Mathematical proof orchestrator
- Aristotle: Automated theorem prover",
        )
        .unwrap();

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify existing content was preserved at top
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("Lean Genius Project"));
        assert!(content.contains("Docker Build Safety"));
        assert!(content.contains("Custom Agents"));

        // Verify Loom pointer was appended at end with markers
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full guide must NOT be in root
        assert!(!content.contains("Loom content here"));

        // Verify order: project content comes BEFORE Loom section
        let project_pos = content.find("Lean Genius Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // Verify no duplicate content or mangling
        assert_eq!(content.matches("Lean Genius Project").count(), 1);
    }

    #[test]
    fn test_claude_md_preservation_update_loom_section_only() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template (simulating upgrade)
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nUPDATED Loom content v2.0.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md with markers (previous install had full guide in root)
        // This simulates upgrading from old install where full guide was in root CLAUDE.md
        let existing = format!(
            "# My Project\n\nProject docs here.\n\n{LOOM_SECTION_START}\n# Loom Orchestration - Repository Guide\n\nOld Loom content v1.0.\n{LOOM_SECTION_END}"
        );
        fs::write(workspace.join("CLAUDE.md"), existing).unwrap();

        // Run setup with force=true
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify project content was preserved, Loom section was replaced with short pointer
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Project"));
        assert!(content.contains("Project docs here"));
        // Old full guide content must be gone from root
        assert!(!content.contains("Old Loom content v1.0"));
        // Updated full guide must also NOT be in root
        assert!(!content.contains("UPDATED Loom content v2.0"));
        // Root should now have the short pointer
        assert!(content.contains(LOOM_ROOT_POINTER));

        // Should only have ONE set of markers
        assert_eq!(
            content.matches(LOOM_SECTION_START).count(),
            1,
            "Should have exactly one start marker"
        );
        assert_eq!(
            content.matches(LOOM_SECTION_END).count(),
            1,
            "Should have exactly one end marker"
        );

        // Updated full guide content must be in .loom/CLAUDE.md
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("UPDATED Loom content v2.0"));
    }

    #[test]
    fn test_loom_claude_md_updated_on_reinstall() {
        // Verifies .loom/CLAUDE.md is overwritten on reinstall with new template content
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration\n\nUpdated content v2.",
        )
        .unwrap();

        // Pre-existing .loom/CLAUDE.md from previous install
        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(
            workspace.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration\n\nOld content v1.",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify .loom/CLAUDE.md was updated with new content
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("Updated content v2"));
        assert!(!loom_content.contains("Old content v1"));
        assert!(report.updated.contains(&".loom/CLAUDE.md".to_string()));
    }

    // =========================================================================
    // AGENTS.md tests (issue #4479, epic #4167 — dual-runtime instruction
    // anchor; seeded by gpeyton/loom fork PR #8). These mirror the CLAUDE.md
    // marker-injection tests above, exercised against `defaults/.loom/AGENTS.md`
    // and the AGENTS-specific marker pair. AGENTS.md has no historical
    // full-guide-in-root layout of its own to migrate away from, but issue
    // #4888 showed a broken/interrupted prior install can still leave a root
    // AGENTS.md carrying leaked, unsubstituted `{{LOOM_VERSION}}`-style
    // placeholder text (with or without markers) — the legacy-migration tests
    // below (reusing the same `is_legacy_loom_managed_root` /
    // `slice_is_discardable_legacy` heuristics as CLAUDE.md) cover that case.
    // =========================================================================

    /// Helper to create a standard test setup with an AGENTS.md template in defaults.
    fn setup_test_with_agents_template(
        temp_dir: &TempDir,
        template_content: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let workspace = temp_dir.path().to_path_buf();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(defaults.join(".loom").join("AGENTS.md"), template_content).unwrap();

        (workspace, defaults)
    }

    #[test]
    fn test_loom_agents_md_written_to_loom_dir() {
        // Verifies full content goes to .loom/AGENTS.md on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_agents_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content here.",
        );

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        assert!(workspace.join(".loom").join("AGENTS.md").exists());
        let loom_agents_content =
            fs::read_to_string(workspace.join(".loom").join("AGENTS.md")).unwrap();
        assert!(loom_agents_content.contains("Full guide content here"));
        assert!(report.added.contains(&".loom/AGENTS.md".to_string()));
    }

    #[test]
    fn test_root_agents_md_contains_only_pointer() {
        // Verifies root AGENTS.md has short pointer, not full guide, on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_agents_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content here.",
        );

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        assert!(!workspace.join("AGENTS.md").exists());

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        assert!(workspace.join("AGENTS.md").exists());
        let root_content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(root_content.contains(AGENTS_SECTION_START));
        assert!(root_content.contains(AGENTS_SECTION_END));
        assert!(root_content.contains(AGENTS_ROOT_POINTER));
        // Full guide content must NOT be in root AGENTS.md
        assert!(!root_content.contains("Full guide content here"));
        assert!(report.added.contains(&"AGENTS.md".to_string()));

        // AGENTS.md markers must be independent from CLAUDE.md's markers —
        // the root AGENTS.md must not contain the CLAUDE.md marker pair.
        assert!(!root_content.contains(LOOM_SECTION_START));
        assert!(!root_content.contains(LOOM_SECTION_END));
    }

    #[test]
    fn test_agents_md_preservation_new_install() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("AGENTS.md"),
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nLoom content here.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        assert!(!workspace.join("AGENTS.md").exists());

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        assert!(workspace.join("AGENTS.md").exists());
        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains(AGENTS_SECTION_START));
        assert!(content.contains(AGENTS_SECTION_END));
        assert!(content.contains(AGENTS_ROOT_POINTER));
        assert!(!content.contains("Loom content here"));
        assert!(report.added.contains(&"AGENTS.md".to_string()));

        assert!(workspace.join(".loom").join("AGENTS.md").exists());
        let loom_content = fs::read_to_string(workspace.join(".loom").join("AGENTS.md")).unwrap();
        assert!(loom_content.contains("Loom content here"));
    }

    #[test]
    fn test_agents_md_preservation_existing_project_content() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("AGENTS.md"),
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nNew Loom content.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Existing AGENTS.md with project-specific content (no markers)
        fs::write(
            workspace.join("AGENTS.md"),
            r"# My Awesome Project (Codex instructions)

This project does amazing things with Rust.

## Getting Started

Run `cargo run` to start.",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains("My Awesome Project (Codex instructions)"));
        assert!(content.contains("amazing things with Rust"));
        assert!(content.contains(AGENTS_SECTION_START));
        assert!(content.contains(AGENTS_SECTION_END));
        assert!(content.contains(AGENTS_ROOT_POINTER));
        assert!(!content.contains("New Loom content"));

        let project_pos = content
            .find("My Awesome Project (Codex instructions)")
            .unwrap();
        let loom_pos = content.find(AGENTS_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        assert_eq!(
            content
                .matches("My Awesome Project (Codex instructions)")
                .count(),
            1
        );
    }

    #[test]
    fn test_agents_md_append_when_no_markers() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("AGENTS.md"),
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nLoom content here.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Existing AGENTS.md WITHOUT markers
        fs::write(
            workspace.join("AGENTS.md"),
            r"# Lean Genius Project

Formal mathematics in Lean 4.

## Docker Build Safety

WARNING: Never run `lake build` inside Docker - causes memory corruption.",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains("Lean Genius Project"));
        assert!(content.contains("Docker Build Safety"));

        assert!(content.contains(AGENTS_SECTION_START));
        assert!(content.contains(AGENTS_SECTION_END));
        assert!(content.contains(AGENTS_ROOT_POINTER));
        assert!(!content.contains("Loom content here"));

        let project_pos = content.find("Lean Genius Project").unwrap();
        let loom_pos = content.find(AGENTS_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        assert_eq!(content.matches("Lean Genius Project").count(), 1);
    }

    #[test]
    fn test_agents_md_preservation_update_loom_section_only() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("AGENTS.md"),
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nUPDATED Loom content v2.0.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Existing AGENTS.md with markers already present (simulating a prior install)
        let existing = format!(
            "# My Project\n\nProject docs here.\n\n{AGENTS_SECTION_START}\nOld pointer text.\n{AGENTS_SECTION_END}"
        );
        fs::write(workspace.join("AGENTS.md"), existing).unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains("My Project"));
        assert!(content.contains("Project docs here"));
        assert!(!content.contains("Old pointer text"));
        assert!(!content.contains("UPDATED Loom content v2.0"));
        assert!(content.contains(AGENTS_ROOT_POINTER));

        assert_eq!(
            content.matches(AGENTS_SECTION_START).count(),
            1,
            "Should have exactly one AGENTS start marker"
        );
        assert_eq!(
            content.matches(AGENTS_SECTION_END).count(),
            1,
            "Should have exactly one AGENTS end marker"
        );

        let loom_content = fs::read_to_string(workspace.join(".loom").join("AGENTS.md")).unwrap();
        assert!(loom_content.contains("UPDATED Loom content v2.0"));
    }

    #[test]
    fn test_loom_agents_md_updated_on_reinstall() {
        // Verifies .loom/AGENTS.md is overwritten on reinstall with new template content
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("AGENTS.md"),
            "# Loom Orchestration (AGENTS.md)\n\nUpdated content v2.",
        )
        .unwrap();

        // Pre-existing .loom/AGENTS.md from previous install
        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(
            workspace.join(".loom").join("AGENTS.md"),
            "# Loom Orchestration (AGENTS.md)\n\nOld content v1.",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        let loom_content = fs::read_to_string(workspace.join(".loom").join("AGENTS.md")).unwrap();
        assert!(loom_content.contains("Updated content v2"));
        assert!(!loom_content.contains("Old content v1"));
        assert!(report.updated.contains(&".loom/AGENTS.md".to_string()));
    }

    // ---------- #4888 AGENTS.md legacy-placeholder migration tests ----------

    #[test]
    fn test_setup_scaffolding_discards_markerless_legacy_root_agents_md() {
        // Regression test for #4888 defect 1. A broken/interrupted prior
        // install (or an old pre-marker layout) can leave a markerless root
        // AGENTS.md carrying leaked, unsubstituted `{{LOOM_VERSION}}` text.
        // Before the fix, the "no markers" branch preserved this verbatim,
        // which reintroduced the placeholders and tripped
        // `assert_no_placeholders`, aborting the whole install.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_agents_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Markerless legacy content with leaked template placeholders.
        fs::write(
            workspace.join("AGENTS.md"),
            "# Loom Orchestration - Repository Guide\n\n\
             **Loom Version**: {{LOOM_VERSION}}\n\
             **Installation Date**: {{INSTALL_DATE}}\n\n\
             Generated by Loom Installation Process\n",
        )
        .unwrap();

        let mut report = InitReport::default();
        let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
        assert!(result.is_ok(), "install must not fail on legacy AGENTS.md: {result:?}");

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains(AGENTS_SECTION_START));
        assert!(content.contains(AGENTS_SECTION_END));
        assert!(content.contains(AGENTS_ROOT_POINTER));
        assert!(!content.contains("{{LOOM_VERSION}}"), "leaked placeholder: {content}");
        assert!(!content.contains("{{INSTALL_DATE}}"));
        assert!(!content.contains("Generated by Loom Installation Process"));
    }

    #[test]
    fn test_setup_scaffolding_discards_hybrid_legacy_root_agents_md() {
        // Regression test for #4888 defect 1, hybrid variant (mirrors
        // CLAUDE.md's #3476 hybrid test): a markered root AGENTS.md whose
        // slice OUTSIDE the marker block is itself leftover legacy content
        // with unsubstituted placeholders. Before the fix the marker-replace
        // branch preserved `before`/`after` verbatim regardless of content,
        // leaking the placeholders through and tripping the guard.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_agents_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        let legacy_fragment = "# Loom Orchestration - Repository Guide\n\n\
             **Loom Version**: {{LOOM_VERSION}}\n\
             **Installation Date**: {{INSTALL_DATE}}\n\n\
             Generated by Loom Installation Process\n";
        let hybrid = format!("{}\n{}\n", legacy_fragment, wrap_agents_content("Old pointer text"));
        fs::write(workspace.join("AGENTS.md"), &hybrid).unwrap();

        let mut report = InitReport::default();
        let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
        assert!(result.is_ok(), "install must not fail on hybrid legacy AGENTS.md: {result:?}");

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert_eq!(
            content,
            wrap_agents_content(AGENTS_ROOT_POINTER),
            "hybrid legacy AGENTS.md should be fully replaced with the wrapped pointer"
        );
        assert!(!content.contains("{{LOOM_VERSION}}"));
        assert!(!content.contains("{{INSTALL_DATE}}"));
        assert!(!content.contains("Generated by Loom Installation Process"));
        assert!(!content.contains("Old pointer text"));
    }

    #[test]
    fn test_setup_scaffolding_discards_malformed_marker_legacy_root_agents_md() {
        // Regression test for #4888 defect 1, malformed-marker variant: only
        // the START marker is present (no END), which used to fall into the
        // "append pointer at end" branch and preserve the entire file
        // (including leaked placeholders) verbatim.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_agents_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        let malformed = format!(
            "{AGENTS_SECTION_START}\n**Loom Version**: {{{{LOOM_VERSION}}}}\nno end marker here\n"
        );
        fs::write(workspace.join("AGENTS.md"), &malformed).unwrap();

        let mut report = InitReport::default();
        let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
        assert!(
            result.is_ok(),
            "install must not fail on malformed-marker AGENTS.md: {result:?}"
        );

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains(AGENTS_SECTION_START));
        assert!(content.contains(AGENTS_SECTION_END));
        assert!(content.contains(AGENTS_ROOT_POINTER));
        assert!(!content.contains("{{LOOM_VERSION}}"), "leaked placeholder: {content}");
    }

    #[test]
    fn test_setup_scaffolding_preserves_markerless_user_root_agents_md() {
        // Negative control: markerless content with NO legacy signature must
        // still be preserved and appended-to, not discarded. Guards against
        // the new legacy check being overly aggressive.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_agents_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide (AGENTS.md)\n\nFull guide content (new).",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        fs::write(
            workspace.join("AGENTS.md"),
            "# My Project\n\nHand-written Codex instructions, no Loom signatures here.",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains("Hand-written Codex instructions"));
        assert!(content.contains(AGENTS_SECTION_START));
        assert!(content.contains(AGENTS_ROOT_POINTER));
    }

    #[test]
    fn test_codex_directory_copy_is_silent_noop_when_absent() {
        // `defaults/.codex/` does not currently ship in this repo. Verify that
        // running scaffolding setup with no `defaults/.codex/` present does not
        // error, does not create `<workspace>/.codex/`, and does not add any
        // report entries referencing `.codex`.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(&defaults).unwrap();
        // Deliberately do NOT create defaults/.codex/.
        assert!(!defaults.join(".codex").exists());

        let mut report = InitReport::default();
        let result = setup_repository_scaffolding(workspace, &defaults, false, &mut report);
        assert!(result.is_ok(), ".codex/ absence must not error: {result:?}");

        assert!(
            !workspace.join(".codex").exists(),
            ".codex/ must not be created in the workspace when defaults/.codex/ is absent"
        );
        assert!(
            !report
                .added
                .iter()
                .chain(report.updated.iter())
                .chain(report.preserved.iter())
                .any(|p| p.contains(".codex")),
            "no report entries should reference .codex when the source directory is absent"
        );
    }

    #[test]
    fn test_claude_commands_always_updated_on_reinstall() {
        // .claude/ commands should always be force-merged on reinstall (without --force flag)
        // This ensures command updates propagate while custom commands are preserved.
        // Issue #3310: also covers `.claude/agents/` (subagent definitions),
        // which live alongside `.claude/commands/` and must propagate the same way.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with .claude commands AND agents
        fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
        fs::create_dir_all(defaults.join(".claude").join("agents")).unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
            "loom command v2 with bug fix",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("builder.md"),
            "builder command v2",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("agents")
                .join("loom-builder.md"),
            "loom-builder subagent v2",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("agents")
                .join("loom-judge.md"),
            "loom-judge subagent v1",
        )
        .unwrap();

        // Create existing .claude directory in workspace (simulates previous install)
        fs::create_dir_all(workspace.join(".claude").join("commands").join("loom")).unwrap();
        fs::create_dir_all(workspace.join(".claude").join("agents")).unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
            "loom command v1 with bug",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("my-custom.md"),
            "my project-specific command",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("agents")
                .join("loom-builder.md"),
            "loom-builder subagent v1 with bug",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("agents")
                .join("my-custom-agent.md"),
            "project-specific custom subagent",
        )
        .unwrap();

        // Run setup WITHOUT force flag (simulates normal reinstall)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify: loom.md was UPDATED (default command updated with bug fix)
        let loom_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("loom.md"),
        )
        .unwrap();
        assert_eq!(loom_content, "loom command v2 with bug fix");

        // Verify: builder.md was ADDED (new default command)
        let builder_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("builder.md"),
        )
        .unwrap();
        assert_eq!(builder_content, "builder command v2");

        // Verify: my-custom.md was PRESERVED (custom command not in defaults)
        let custom_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("my-custom.md"),
        )
        .unwrap();
        assert_eq!(custom_content, "my project-specific command");

        // Verify report reflects the changes
        assert!(report
            .updated
            .contains(&".claude/commands/loom/loom.md".to_string()));
        assert!(report
            .added
            .contains(&".claude/commands/loom/builder.md".to_string()));
        assert!(report
            .preserved
            .contains(&".claude/commands/my-custom.md".to_string()));

        // Issue #3310: verify .claude/agents/ propagates identically.
        // Default subagent updated:
        let agent_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("agents")
                .join("loom-builder.md"),
        )
        .unwrap();
        assert_eq!(agent_content, "loom-builder subagent v2");
        // New default subagent added:
        let new_agent_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("agents")
                .join("loom-judge.md"),
        )
        .unwrap();
        assert_eq!(new_agent_content, "loom-judge subagent v1");
        // Project-specific custom subagent preserved:
        let custom_agent_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("agents")
                .join("my-custom-agent.md"),
        )
        .unwrap();
        assert_eq!(custom_agent_content, "project-specific custom subagent");

        assert!(report
            .updated
            .contains(&".claude/agents/loom-builder.md".to_string()));
        assert!(report
            .added
            .contains(&".claude/agents/loom-judge.md".to_string()));
        assert!(report
            .preserved
            .contains(&".claude/agents/my-custom-agent.md".to_string()));
    }

    #[test]
    fn test_fresh_install_copies_claude_agents() {
        // Issue #3310: a fresh install (no existing .claude/) must copy the
        // full `.claude/agents/` tree from defaults so native subagent
        // dispatch (subagent_type="loom-builder", etc.) works out of the
        // box. Previously the .claude/ directory copy happened only via
        // copy_dir_with_report — this test pins the behavior so the
        // installer cannot silently regress agents/ in the future.
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with .claude/agents/ (and a commands/ stub so the
        // .claude/ src directory is non-empty, mirroring real defaults).
        fs::create_dir_all(defaults.join(".claude").join("commands").join("loom")).unwrap();
        fs::create_dir_all(defaults.join(".claude").join("agents")).unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("commands")
                .join("loom")
                .join("builder.md"),
            "builder command",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("agents")
                .join("loom-builder.md"),
            "loom-builder subagent body",
        )
        .unwrap();
        fs::write(
            defaults
                .join(".claude")
                .join("agents")
                .join("loom-judge.md"),
            "loom-judge subagent body",
        )
        .unwrap();

        // Workspace starts with NO .claude/ — pure fresh install
        assert!(!workspace.join(".claude").exists());

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // The fresh-install path must produce the full agents tree.
        let installed_builder = workspace
            .join(".claude")
            .join("agents")
            .join("loom-builder.md");
        assert!(
            installed_builder.exists(),
            "fresh install must copy .claude/agents/loom-builder.md (see #3310)"
        );
        assert_eq!(fs::read_to_string(&installed_builder).unwrap(), "loom-builder subagent body");
        assert!(workspace
            .join(".claude")
            .join("agents")
            .join("loom-judge.md")
            .exists());

        // Report should track every agent file as "added".
        assert!(report
            .added
            .contains(&".claude/agents/loom-builder.md".to_string()));
        assert!(report
            .added
            .contains(&".claude/agents/loom-judge.md".to_string()));
    }

    // =========================================================================
    // settings.json merge tests
    // =========================================================================

    #[test]
    fn test_merge_settings_fresh_install_no_existing() {
        // When no existing settings.json, Loom defaults are used as-is
        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)"]
            }
        }"#,
        )
        .unwrap();

        // Empty existing
        let existing: serde_json::Value = serde_json::from_str("{}").unwrap();
        let merged = merge_settings_json(&existing, &loom_defaults);

        // Should have Loom's hooks
        let hooks = merged.get("hooks").unwrap();
        let pre_tool = hooks.get("PreToolUse").unwrap().as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["matcher"], "Bash");

        // Should have Loom's permissions
        let perms = merged
            .get("permissions")
            .unwrap()
            .get("allow")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(perms.len(), 2);
    }

    #[test]
    fn test_merge_settings_preserves_project_hooks() {
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Edit",
                    "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk-files.sh"}]
                }],
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [{"type": "command", "command": "skill-router.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "CustomPermission"]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)"]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        // PreToolUse should have both Edit (project) and Bash (Loom) matchers
        let pre_tool = merged["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2, "Should have both Edit and Bash matchers");

        // Edit matcher should be preserved
        let edit_matcher = pre_tool.iter().find(|m| m["matcher"] == "Edit").unwrap();
        assert_eq!(edit_matcher["hooks"][0]["command"], ".claude/hooks/guard-pdk-files.sh");

        // Bash matcher should be added from Loom
        let bash_matcher = pre_tool.iter().find(|m| m["matcher"] == "Bash").unwrap();
        assert_eq!(bash_matcher["hooks"][0]["command"], ".loom/hooks/guard-destructive.sh");

        // UserPromptSubmit (project-only) should be preserved
        let user_prompt = merged["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(user_prompt.len(), 1);
        assert_eq!(user_prompt[0]["hooks"][0]["command"], "skill-router.sh");

        // Permissions should be unioned (3 unique: gh, git, CustomPermission)
        let perms = merged["permissions"]["allow"].as_array().unwrap();
        assert_eq!(perms.len(), 3, "Should have 3 unique permissions");
        let perm_strs: Vec<&str> = perms.iter().map(|p| p.as_str().unwrap()).collect();
        assert!(perm_strs.contains(&"Bash(gh:*)"));
        assert!(perm_strs.contains(&"Bash(git:*)"));
        assert!(perm_strs.contains(&"CustomPermission"));
    }

    #[test]
    fn test_merge_settings_deduplicates_hooks() {
        // When project already has the new-prefix Loom hook, don't add it again
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": ".claude/hooks/custom-bash-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        // Should not duplicate the Loom hook
        let bash_hooks = &merged["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();
        assert_eq!(hooks_arr.len(), 2, "Should not duplicate existing Loom hook");

        // Both hooks should still be present
        let commands: Vec<&str> = hooks_arr
            .iter()
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert!(commands.contains(&"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"));
        assert!(commands.contains(&".claude/hooks/custom-bash-guard.sh"));
    }

    #[test]
    fn test_merge_settings_deduplicates_hooks_with_quoted_paths() {
        // Issue #4200: a prior installer generation wrote the Loom hook
        // command wrapped in double quotes (to survive a project path
        // containing spaces). Reinstalling with the current, unquoted
        // template must recognize this as the SAME hook and not append a
        // second, functionally identical entry.
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""},
                        {"type": "command", "command": ".claude/hooks/custom-bash-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        let bash_hooks = &merged["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();

        // Exactly 2 entries: the original quoted Loom hook (preserved as-is,
        // not rewritten) + the custom project hook. No unquoted duplicate.
        assert_eq!(
            hooks_arr.len(),
            2,
            "Should not append an unquoted duplicate of an existing quoted Loom hook: {hooks_arr:?}"
        );

        let commands: Vec<&str> = hooks_arr
            .iter()
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert!(
            commands.contains(&"\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""),
            "Original quoted entry should be preserved unchanged (comparison-only normalization), got: {commands:?}"
        );
        assert!(commands.contains(&".claude/hooks/custom-bash-guard.sh"));
    }

    #[test]
    fn test_merge_settings_migrates_legacy_hooks() {
        // Pre-3265 installs have bare-relative `.loom/hooks/...` entries.
        // On re-install, the merge must strip the legacy entry and add the new
        // `${CLAUDE_PROJECT_DIR}/.loom/hooks/...` entry so the result has no
        // duplicate invocations.
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": ".loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": ".claude/hooks/custom-bash-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        let bash_hooks = &merged["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();

        let commands: Vec<&str> = hooks_arr
            .iter()
            .map(|h| h["command"].as_str().unwrap())
            .collect();

        // Legacy bare-relative entry must be stripped
        assert!(
            !commands.contains(&".loom/hooks/guard-destructive.sh"),
            "Legacy bare-relative hook should be removed during merge"
        );
        // New prefix entry must be present
        assert!(
            commands.contains(&"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"),
            "New ${{CLAUDE_PROJECT_DIR}}-prefixed hook must be added"
        );
        // Custom project hook must be preserved
        assert!(commands.contains(&".claude/hooks/custom-bash-guard.sh"));

        // Exactly 2 entries: new Loom hook + custom project hook (no duplicate)
        assert_eq!(
            hooks_arr.len(),
            2,
            "Should have exactly 2 hooks: new Loom hook + custom project hook"
        );
    }

    #[test]
    fn test_merge_settings_preserves_other_keys() {
        // Keys like enabledPlugins, MCP config, etc. should be preserved
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "enabledPlugins": {"some-plugin": true},
            "model": "opus",
            "permissions": {
                "allow": ["CustomPermission"]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)"]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        // enabledPlugins and model should be preserved
        assert_eq!(merged["enabledPlugins"]["some-plugin"], true);
        assert_eq!(merged["model"], "opus");

        // Hooks should be added
        assert!(merged.get("hooks").is_some());

        // Permissions should be merged
        let perms = merged["permissions"]["allow"].as_array().unwrap();
        assert_eq!(perms.len(), 2);
    }

    #[test]
    fn test_remove_loom_hooks() {
        let mut settings: serde_json::Value = serde_json::from_str(r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {"type": "command", "command": ".loom/hooks/guard-destructive.sh"},
                            {"type": "command", "command": ".claude/hooks/custom-guard.sh"}
                        ]
                    },
                    {
                        "matcher": "Edit",
                        "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk-files.sh"}]
                    }
                ],
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [{"type": "command", "command": "skill-router.sh"}]
                }]
            }
        }"#).unwrap();

        remove_loom_hooks(&mut settings);

        // Loom hook should be removed from PreToolUse/Bash
        let bash_hooks = &settings["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();
        assert_eq!(hooks_arr.len(), 1);
        assert_eq!(hooks_arr[0]["command"], ".claude/hooks/custom-guard.sh");

        // Edit matcher should be untouched
        let edit_hooks = &settings["hooks"]["PreToolUse"][1]["hooks"];
        assert_eq!(edit_hooks.as_array().unwrap().len(), 1);

        // UserPromptSubmit should be untouched
        let user_prompt = &settings["hooks"]["UserPromptSubmit"];
        assert_eq!(user_prompt.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_remove_loom_hooks_removes_quoted_form() {
        // Issue #4200: a quoted-form Loom hook command (e.g.
        // `"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"`) begins
        // with `\"`, so it matches neither the legacy nor the new-prefix
        // `starts_with` check without normalization -- it must still be
        // recognized and removed on uninstall.
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""},
                        {"type": "command", "command": ".claude/hooks/custom-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

        remove_loom_hooks(&mut settings);

        let bash_hooks = &settings["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();
        assert_eq!(
            hooks_arr.len(),
            1,
            "Quoted-form Loom hook should be removed, got: {hooks_arr:?}"
        );
        assert_eq!(hooks_arr[0]["command"], ".claude/hooks/custom-guard.sh");
    }

    #[test]
    fn test_remove_loom_hooks_removes_machine_level_form() {
        // Epic #3835 Phase 5 (#4262): the machine-level `bash -c '...'`
        // wrapper command (provisioned at user-scope, but exercised here
        // against a project-level settings.json to prove recognition is not
        // scope-specific) must be recognized and removed alongside the
        // legacy/current project-relative prefixes.
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "bash -c 'H=\"${LOOM_HOME:-$HOME/.local/share/loom}/defaults/hooks/guard-destructive.sh\"; [ -x \"$H\" ] && exec \"$H\" || exit 0'"},
                        {"type": "command", "command": ".claude/hooks/custom-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

        remove_loom_hooks(&mut settings);

        let bash_hooks = &settings["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();
        assert_eq!(
            hooks_arr.len(),
            1,
            "machine-level hook command should be removed, got: {hooks_arr:?}"
        );
        assert_eq!(hooks_arr[0]["command"], ".claude/hooks/custom-guard.sh");
    }

    #[test]
    fn test_is_loom_hook_command_recognizes_all_three_forms() {
        // Exercised indirectly through remove_loom_hooks above (the function
        // itself is private); this test locks in the three recognized
        // command shapes side-by-side so a future edit to the marker/prefix
        // constants can't silently narrow recognition.
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": ".loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": "bash -c 'H=\"${LOOM_HOME:-$HOME/.local/share/loom}/defaults/hooks/guard-loom-workflow.sh\"; [ -x \"$H\" ] && exec \"$H\" || exit 0'"},
                        {"type": "command", "command": ".claude/hooks/custom-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

        remove_loom_hooks(&mut settings);

        let bash_hooks = &settings["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();
        assert_eq!(
            hooks_arr.len(),
            1,
            "only the non-Loom custom guard should remain, got: {hooks_arr:?}"
        );
        assert_eq!(hooks_arr[0]["command"], ".claude/hooks/custom-guard.sh");
    }

    #[test]
    fn test_remove_loom_hooks_cleans_empty_matchers() {
        // When removing Loom hook leaves a matcher with no hooks, remove the matcher
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

        remove_loom_hooks(&mut settings);

        // hooks key should be removed entirely since nothing remains
        assert!(
            settings.get("hooks").is_none(),
            "hooks key should be removed when empty, got: {settings:?}"
        );
    }

    #[test]
    fn test_remove_loom_permissions() {
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)", "CustomPermission", "WebSearch"]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)", "WebSearch"]
            }
        }"#,
        )
        .unwrap();

        remove_loom_permissions(&mut settings, &loom_defaults);

        let perms = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0], "CustomPermission");
    }

    #[test]
    fn test_remove_loom_permissions_cleans_empty() {
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)"]
            },
            "model": "opus"
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)"]
            }
        }"#,
        )
        .unwrap();

        remove_loom_permissions(&mut settings, &loom_defaults);

        // permissions key should be removed entirely
        assert!(settings.get("permissions").is_none());
        // other keys should be preserved
        assert_eq!(settings["model"], "opus");
    }

    #[test]
    fn test_merge_settings_in_scaffolding_reinstall() {
        // Integration test: verify settings.json is merged during reinstall
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with .claude/commands and settings.json
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(defaults.join(".claude").join("commands").join("loom.md"), "loom command")
            .unwrap();
        fs::write(
            defaults.join(".claude").join("settings.json"),
            r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                    }]
                },
                "permissions": {
                    "allow": ["Bash(gh:*)", "Bash(git:*)"]
                }
            }"#,
        ).unwrap();

        // Create existing .claude directory with project settings
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("settings.json"),
            r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Edit",
                        "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk.sh"}]
                    }],
                    "UserPromptSubmit": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "skill-router.sh"}]
                    }]
                },
                "permissions": {
                    "allow": ["Bash(gh:*)", "CustomPermission"]
                },
                "enabledPlugins": {"my-plugin": true}
            }"#,
        )
        .unwrap();

        // Run setup (reinstall)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Read the resulting settings.json
        let result_content =
            fs::read_to_string(workspace.join(".claude").join("settings.json")).unwrap();
        let result: serde_json::Value = serde_json::from_str(&result_content).unwrap();

        // PreToolUse should have both matchers (Edit from project, Bash from Loom)
        let pre_tool = result["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2, "Should have Edit and Bash matchers");

        // Project's Edit matcher should be preserved
        let has_edit = pre_tool.iter().any(|m| m["matcher"] == "Edit");
        assert!(has_edit, "Project's Edit matcher should be preserved");

        // Loom's Bash matcher should be added
        let has_bash = pre_tool.iter().any(|m| m["matcher"] == "Bash");
        assert!(has_bash, "Loom's Bash matcher should be added");

        // Project's UserPromptSubmit should be preserved
        assert!(
            result["hooks"].get("UserPromptSubmit").is_some(),
            "Project's UserPromptSubmit hooks should be preserved"
        );

        // Permissions should be unioned
        let perms = result["permissions"]["allow"].as_array().unwrap();
        let perm_strs: Vec<&str> = perms.iter().map(|p| p.as_str().unwrap()).collect();
        assert!(perm_strs.contains(&"Bash(gh:*)"));
        assert!(perm_strs.contains(&"Bash(git:*)"));
        assert!(perm_strs.contains(&"CustomPermission"));

        // enabledPlugins should be preserved
        assert_eq!(result["enabledPlugins"]["my-plugin"], true);
    }

    // ---------- #3325 legacy-layout migration tests ----------

    /// Snippet of the legacy (pre-#3000) root CLAUDE.md content, including
    /// unsubstituted template placeholders. Real-world example: strata-fdtd at
    /// commit `e1776eed` had this exact shape.
    const LEGACY_ROOT_CLAUDE_MD: &str = "\
# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Some stale guide content here that nobody should preserve on upgrade.

---

**Generated by Loom Installation Process**
";

    #[test]
    fn test_is_legacy_loom_managed_root_detects_old_layout() {
        // Old-layout content with the title header is legacy.
        assert!(is_legacy_loom_managed_root(LEGACY_ROOT_CLAUDE_MD));

        // Bare `{{LOOM_VERSION}}` is also a signature on its own.
        assert!(is_legacy_loom_managed_root("Loom Version: {{LOOM_VERSION}}"));

        // "Generated by Loom Installation Process" footer alone is enough.
        assert!(is_legacy_loom_managed_root(
            "Some content.\n\n**Generated by Loom Installation Process**"
        ));
    }

    #[test]
    fn test_is_legacy_loom_managed_root_rejects_user_content() {
        // Pure user content with no Loom signatures.
        assert!(!is_legacy_loom_managed_root(
            "# My Project\n\nThis is hand-written documentation."
        ));

        // Empty file.
        assert!(!is_legacy_loom_managed_root(""));

        // Mentioning "loom" in passing isn't enough — we require a specific
        // installer-generated phrase.
        assert!(!is_legacy_loom_managed_root(
            "# My Project\n\nWe use loom for some stuff but wrote this ourselves."
        ));
    }

    #[test]
    fn test_is_legacy_loom_managed_root_skips_marker_block() {
        // Modern marker block must short-circuit to "not legacy" regardless of
        // signature phrases inside the block — that branch is handled
        // separately by the section-replace logic upstream.
        let modern = format!(
            "{LOOM_SECTION_START}\nThis repository uses [Loom](...). **Loom Version**: 0.8.0\n{LOOM_SECTION_END}"
        );
        assert!(!is_legacy_loom_managed_root(&modern));
    }

    #[test]
    fn test_setup_scaffolding_upgrades_legacy_root_claude_md() {
        // Regression test for #3325. Pre-create a workspace root CLAUDE.md
        // with the legacy full-guide layout (including unsubstituted
        // placeholders), run scaffolding, assert the result is the modern
        // marker block — no leftover placeholders, no leftover legacy content.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content (new).",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Pre-existing legacy root CLAUDE.md (pre-#3000 layout).
        fs::write(workspace.join("CLAUDE.md"), LEGACY_ROOT_CLAUDE_MD).unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

        // Result must be the bare marker block — legacy content is gone.
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // No leaked placeholders.
        assert!(
            !content.contains("{{LOOM_VERSION}}"),
            "leaked {{{{LOOM_VERSION}}}} placeholder: {content}"
        );
        assert!(!content.contains("{{LOOM_COMMIT}}"));
        assert!(!content.contains("{{INSTALL_DATE}}"));
        // No leftover legacy content from the title-header block.
        assert!(
            !content.contains("# Loom Orchestration - Repository Guide"),
            "legacy header should be replaced, got: {content}"
        );
        assert!(!content.contains("Some stale guide content"));
        assert!(!content.contains("Generated by Loom Installation Process"));
    }

    #[test]
    fn test_setup_scaffolding_upgrades_hybrid_legacy_root_claude_md() {
        // Regression test for #3476 Bug 1. The v0.7.1 installer wrote the
        // full legacy guide (with unsubstituted placeholders) to root
        // CLAUDE.md AND appended the modern marker block — a hybrid file.
        // The marker-replacement branch used to preserve the legacy "before"
        // portion verbatim, so assert_no_placeholders refused the write and
        // the upgrade aborted. The fix detects the legacy slice and replaces
        // the entire file with the wrapped pointer.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content (new).",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Hybrid shape: legacy full guide followed by a modern marker block.
        let hybrid =
            format!("{}\n{}\n", LEGACY_ROOT_CLAUDE_MD, wrap_loom_content("Old pointer text"));
        fs::write(workspace.join("CLAUDE.md"), &hybrid).unwrap();

        let mut report = InitReport::default();
        let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
        assert!(result.is_ok(), "upgrade of hybrid legacy CLAUDE.md failed: {result:?}");

        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

        // Result must be ONLY the wrapped pointer block.
        assert_eq!(
            content,
            wrap_loom_content(LOOM_ROOT_POINTER),
            "hybrid legacy file should be fully replaced with the wrapped pointer"
        );
        // Exactly one marker block — the legacy portion is gone, not duplicated.
        assert_eq!(content.matches(LOOM_SECTION_START).count(), 1);
        assert_eq!(content.matches(LOOM_SECTION_END).count(), 1);
        // No leaked placeholders or legacy content.
        assert!(!content.contains("{{LOOM_VERSION}}"));
        assert!(!content.contains("{{LOOM_COMMIT}}"));
        assert!(!content.contains("{{INSTALL_DATE}}"));
        assert!(!content.contains("# Loom Orchestration - Repository Guide"));
        assert!(!content.contains("Some stale guide content"));
        assert!(!content.contains("Generated by Loom Installation Process"));
        assert!(!content.contains("Old pointer text"));
    }

    #[test]
    fn test_setup_scaffolding_upgrades_marker_first_hybrid_root_claude_md() {
        // Robustness variant of the #3476 Bug 1 fix: legacy content AFTER the
        // marker block (marker-block-first hybrid) must also trigger full
        // replacement — the `after` slice is checked too.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content (new).",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        let hybrid =
            format!("{}\n\n{}\n", wrap_loom_content("Old pointer text"), LEGACY_ROOT_CLAUDE_MD);
        fs::write(workspace.join("CLAUDE.md"), &hybrid).unwrap();

        let mut report = InitReport::default();
        let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
        assert!(result.is_ok(), "upgrade of marker-first hybrid failed: {result:?}");

        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert_eq!(content, wrap_loom_content(LOOM_ROOT_POINTER));
        assert!(!content.contains("{{LOOM_VERSION}}"));
        assert!(!content.contains("Generated by Loom Installation Process"));
    }

    #[test]
    fn test_slice_is_discardable_legacy_distinguishes_shapes() {
        // Regression unit test for #3527. The slice-discard predicate must
        // separate a genuine legacy guide fragment from a long-lived consumer
        // file that merely starts with a legacy-looking header line.

        // A genuine legacy guide fragment carries multiple signatures — discard.
        assert!(slice_is_discardable_legacy(LEGACY_ROOT_CLAUDE_MD));

        // A short slice with a single signature is still legacy cruft — discard.
        assert!(slice_is_discardable_legacy(
            "# Loom Orchestration - Repository Guide\n\nA few lines of stale guide text.\n"
        ));

        // The bucket-brigade shape: one legacy-looking header line followed by
        // hundreds of lines of real consumer content. Exactly ONE signature in a
        // large slice must be PRESERVED, not discarded.
        let bulk: String = (0..1000)
            .map(|i| format!("Real consumer line {i} with unique content.\n"))
            .collect();
        let consumer = format!("# Loom Orchestration - Repository Guide\n\n{bulk}");
        assert!(
            !slice_is_discardable_legacy(&consumer),
            "large consumer slice with one surviving header line must be preserved"
        );

        // No signatures at all => always preserve.
        assert!(!slice_is_discardable_legacy(
            "# My Project\n\nHand-written docs with no Loom signatures.\n"
        ));

        // Empty slice => preserve (nothing to discard).
        assert!(!slice_is_discardable_legacy(""));
    }

    #[test]
    fn test_setup_scaffolding_preserves_large_consumer_content_with_legacy_header() {
        // Regression test for #3527 (bucket-brigade PR #480 data loss).
        //
        // Shape: a root CLAUDE.md that (a) starts with the legacy-looking header
        // `# Loom Orchestration - Repository Guide` as a plain heading, (b) has
        // ~1000 lines of unrelated real consumer content between that heading and
        // the marker block, and (c) has a valid modern marker block near the end.
        //
        // Before the fix, `is_legacy_loom_managed_root(before)` returned true
        // because the slice contained the header signature, collapsing the entire
        // file to the 3-line pointer stub and deleting ~1000 lines of consumer
        // content. After the fix, the outside-marker content must be preserved
        // byte-for-byte and only the marker-delimited section may change.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nNew full guide content.",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Build the consumer content: legacy-looking header + ~1000 lines of real,
        // organically-added content that must survive verbatim.
        let bulk: String = (0..1000)
            .map(|i| {
                format!(
                    "Remote-dev guide line {i}: Anvil integration reference detail number {i}.\n"
                )
            })
            .collect();
        let consumer_before = format!(
            "# Loom Orchestration - Repository Guide\n\n\
             ## Compute Resource Guidelines\n\n\
             NEVER train models locally. Use the remote host inventory.\n\n\
             {bulk}"
        );

        // Assemble the full file: consumer content, then an OLD marker block
        // (pre-existing pointer) that the installer will refresh in place.
        let pre_existing = format!(
            "{}\n{}\n\n## Trailing Consumer Section\n\nMore real content after the markers.\n",
            consumer_before,
            wrap_loom_content("Old pointer text"),
        );
        fs::write(workspace.join("CLAUDE.md"), &pre_existing).unwrap();

        let mut report = InitReport::default();
        let result = setup_repository_scaffolding(&workspace, &defaults, false, &mut report);
        assert!(result.is_ok(), "upgrade of large consumer CLAUDE.md failed: {result:?}");

        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

        // The consumer content outside the markers must survive byte-for-byte.
        assert!(
            content.contains("## Compute Resource Guidelines"),
            "consumer section header lost: data loss regression"
        );
        assert!(content.contains("NEVER train models locally. Use the remote host inventory."));
        assert!(content.contains("## Trailing Consumer Section"));
        assert!(content.contains("More real content after the markers."));
        // Spot-check the bulk lines survived.
        assert!(content.contains("Remote-dev guide line 0:"));
        assert!(content.contains("Remote-dev guide line 999:"));
        for i in [0usize, 250, 500, 750, 999] {
            assert!(
                content.contains(&format!(
                    "Remote-dev guide line {i}: Anvil integration reference detail number {i}."
                )),
                "consumer line {i} was deleted"
            );
        }

        // The marker section was refreshed to the new pointer, exactly once.
        assert!(content.contains(LOOM_ROOT_POINTER));
        assert!(!content.contains("Old pointer text"));
        assert_eq!(content.matches(LOOM_SECTION_START).count(), 1);
        assert_eq!(content.matches(LOOM_SECTION_END).count(), 1);

        // The file did NOT collapse to the bare stub.
        assert_ne!(
            content,
            wrap_loom_content(LOOM_ROOT_POINTER),
            "file collapsed to bare pointer stub — consumer content was deleted"
        );
    }

    #[test]
    fn test_setup_scaffolding_preserves_modern_marker_block() {
        // Regression guard for #3000 behavior: an existing modern marker block
        // must update the wrapped pointer in place, not replace the whole file.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nNew full guide content.",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Pre-existing modern root with markers + project content above and below.
        let pre_existing = format!(
            "# My Project\n\nIntro paragraph.\n\n{}\n{}\n{}\n\n## Project Notes\n\nMore stuff.\n",
            LOOM_SECTION_START, "Old pointer text", LOOM_SECTION_END
        );
        fs::write(workspace.join("CLAUDE.md"), &pre_existing).unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

        // User content above and below the marker block must survive.
        assert!(content.contains("# My Project"));
        assert!(content.contains("Intro paragraph"));
        assert!(content.contains("## Project Notes"));
        assert!(content.contains("More stuff"));
        // The Loom section is updated to the new pointer.
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Old marker contents are gone.
        assert!(!content.contains("Old pointer text"));
    }

    #[test]
    fn test_setup_scaffolding_preserves_user_content_without_legacy_signature() {
        // Genuine user-authored root CLAUDE.md (no markers, no Loom signatures)
        // must be preserved with the marker block appended at the end.
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nNew full guide content.",
        );
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        let user_content = "\
# My Awesome Project

This project does amazing things.

## Getting Started

Run `cargo run` to start.";
        fs::write(workspace.join("CLAUDE.md"), user_content).unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();

        // User content survives.
        assert!(content.contains("My Awesome Project"));
        assert!(content.contains("amazing things"));
        assert!(content.contains("Getting Started"));
        assert!(content.contains("cargo run"));
        // Loom marker block is appended.
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // User content comes BEFORE the marker block.
        let user_pos = content.find("My Awesome Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(user_pos < loom_pos, "user content must precede Loom block");
    }

    #[test]
    fn test_assert_no_placeholders_catches_corrupt_template() {
        // Defense-in-depth: if a future code path slips literal `{{LOOM_VERSION}}`
        // past the substitution step, the post-write assertion must reject it.
        // We exercise the assertion directly here; the install path itself can't
        // currently produce a leaked-placeholder file (the legacy branch
        // replaces with a hardcoded pointer; the marker branch reuses the same
        // string), but the guard is the safety net.
        let leaky = format!("{LOOM_SECTION_START}\n{{{{LOOM_VERSION}}}}\n{LOOM_SECTION_END}");
        let err = assert_no_placeholders(&leaky, "CLAUDE.md").unwrap_err();
        assert!(err.contains("CLAUDE.md"));
        assert!(err.contains("{{LOOM_VERSION}}"));

        // Sanity check: the normal install output (just the wrapped pointer)
        // must pass.
        let clean = wrap_loom_content(LOOM_ROOT_POINTER);
        assert!(assert_no_placeholders(&clean, "CLAUDE.md").is_ok());
    }

    // ── .github/labels.yml Loom-block merge (issue #4187) ──────────────────

    const SHIPPED_LABELS: &str = "# BEGIN LOOM LABELS\n# managed by Loom\n- name: loom:issue\n  color: \"3B82F6\"\n- name: loom:building\n  color: \"F59E0B\"\n# END LOOM LABELS\n";

    #[test]
    fn test_labels_block_range_well_formed() {
        let (start, end) = labels_block_range(SHIPPED_LABELS).unwrap();
        assert_eq!(&SHIPPED_LABELS[start..start + LOOM_LABELS_START.len()], LOOM_LABELS_START);
        assert!(SHIPPED_LABELS[..end].ends_with(LOOM_LABELS_END));
    }

    #[test]
    fn test_labels_block_range_absent_or_malformed() {
        // No markers at all.
        assert!(labels_block_range("- name: team:foo\n  color: abcdef\n").is_none());
        // END before BEGIN is not a valid block.
        let inverted = "# END LOOM LABELS\n- name: x\n# BEGIN LOOM LABELS\n";
        // BEGIN is found, but END only searched after BEGIN -> none.
        assert!(labels_block_range(inverted).is_none());
    }

    #[test]
    fn test_merge_labels_block_replaces_marked_range_preserving_outside() {
        // Consumer file: labels above and below a stale Loom block.
        let existing = "- name: team:above\n  color: \"111111\"\n\n# BEGIN LOOM LABELS\n- name: loom:issue\n  color: \"000000\"\n# END LOOM LABELS\n\n- name: team:below\n  color: \"222222\"\n";
        let merged = merge_labels_block(existing, SHIPPED_LABELS).unwrap();

        // Consumer entries outside the block are byte-preserved.
        assert!(merged.contains("- name: team:above"));
        assert!(merged.contains("- name: team:below"));
        // The Loom block is refreshed to the shipped content.
        assert!(merged.contains("color: \"3B82F6\""));
        assert!(merged.contains("- name: loom:building"));
        assert!(!merged.contains("color: \"000000\""), "stale Loom color must be gone");
        // Exactly one marker pair.
        assert_eq!(merged.matches(LOOM_LABELS_START).count(), 1);
        assert_eq!(merged.matches(LOOM_LABELS_END).count(), 1);
    }

    #[test]
    fn test_merge_labels_block_appends_to_markerless_file() {
        let existing =
            "- name: team:frontend\n  color: \"00ff00\"\n  description: consumer label\n";
        let merged = merge_labels_block(existing, SHIPPED_LABELS).unwrap();

        // Every existing entry survives.
        assert!(merged.contains("- name: team:frontend"));
        assert!(merged.contains("description: consumer label"));
        // The Loom block is appended.
        assert!(merged.contains(LOOM_LABELS_START));
        assert!(merged.contains("- name: loom:issue"));
        // Consumer content precedes the appended block.
        assert!(merged.find("team:frontend").unwrap() < merged.find(LOOM_LABELS_START).unwrap());
    }

    #[test]
    fn test_merge_labels_block_noop_when_block_matches() {
        // A file that already equals a plain copy of the shipped file needs no change.
        assert!(merge_labels_block(SHIPPED_LABELS, SHIPPED_LABELS).is_none());
    }

    #[test]
    fn test_merge_labels_block_preserves_when_source_markerless() {
        // Defensive: a shipped file without markers must never clobber consumer content.
        let existing = "- name: team:only\n  color: \"abcdef\"\n";
        assert!(merge_labels_block(existing, "- name: loom:issue\n  color: fff\n").is_none());
    }

    #[test]
    fn test_install_labels_block_fresh_install_is_verbatim_copy() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("labels.src.yml");
        let dst = temp.path().join("labels.yml");
        fs::write(&src, SHIPPED_LABELS).unwrap();

        let mut report = InitReport::default();
        install_labels_block(&src, &dst, None, &mut report).unwrap();

        // Fresh install ships the file byte-for-byte (keeps registry parity).
        assert_eq!(fs::read_to_string(&dst).unwrap(), SHIPPED_LABELS);
        assert!(report.added.contains(&LABELS_YML_REL.to_string()));
        assert!(!report.preserved.contains(&LABELS_YML_REL.to_string()));
    }

    #[test]
    fn test_install_labels_block_force_restores_consumer_content() {
        // Simulate a --force directory copy having clobbered the dst with the
        // shipped file, while pre_existing captured the consumer's real content.
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("labels.src.yml");
        let dst = temp.path().join("labels.yml");
        fs::write(&src, SHIPPED_LABELS).unwrap();
        // Post-clobber on-disk state.
        fs::write(&dst, SHIPPED_LABELS).unwrap();

        let pre_existing = "# BEGIN LOOM LABELS\n- name: loom:issue\n  color: \"000000\"\n# END LOOM LABELS\n\n- name: team:below\n  color: \"222222\"\n";

        let mut report = InitReport::default();
        install_labels_block(&src, &dst, Some(pre_existing), &mut report).unwrap();

        let result = fs::read_to_string(&dst).unwrap();
        // Consumer label survives the force reinstall.
        assert!(result.contains("- name: team:below"));
        // Loom block refreshed.
        assert!(result.contains("color: \"3B82F6\""));
        // Recorded as preserved (consumer-owned) — dropped any copy-side entry.
        assert!(report.preserved.contains(&LABELS_YML_REL.to_string()));
        assert_eq!(report.added.iter().filter(|f| *f == LABELS_YML_REL).count(), 0);
        assert_eq!(
            report
                .updated
                .iter()
                .filter(|f| *f == LABELS_YML_REL)
                .count(),
            0
        );
    }
}
