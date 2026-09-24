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
use super::templates::{
    assert_no_placeholders, localize_dotloom_doc_links, substitute_template_variables, LoomMetadata,
};
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

/// The `.claude`-relative path of the Claude Code settings file, special-cased
/// in the scaffolding copy so a pre-existing file (e.g. one already co-owned by
/// another tool, such as Repo Skills' PreToolUse/SessionStart hooks) is deep-
/// merged with Loom's defaults rather than overwritten (issue #5396).
const SETTINGS_JSON_REL: &str = ".claude/settings.json";

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
///
/// A missing file is the normal, silent case (fresh install — nothing to
/// merge). A *present but unparseable* file is a different, noteworthy case:
/// the caller ([`merge_settings_json`] via `setup_repository_scaffolding`)
/// treats `None` as "nothing to merge" and leaves Loom's freshly-copied
/// defaults in place untouched — silently dropping every pre-existing hook,
/// permission, and top-level key the file contained (issue #5279's
/// "Suspected Cause" #2: a non-strict-JSON file some other tool wrote — e.g.
/// containing comments or a trailing comma — would previously fail this
/// parse with zero warning, discarding its content on the very next
/// reinstall/upgrade with no diagnostic pointing at why). We now warn on that
/// specific case so the drop is visible instead of silent; the missing-file
/// case stays silent since it is not an error.
fn read_existing_settings(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&content) {
        Ok(value) if value.is_object() => Some(value),
        Ok(_) => {
            eprintln!(
                "Warning: {} is valid JSON but not a JSON object; skipping settings.json merge (Loom's own settings.json will be used as-is).",
                path.display()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "Warning: Failed to parse existing {} as JSON ({e}); skipping settings.json merge. Loom's own settings.json will be written as-is, which may drop pre-existing hooks/permissions/keys from this file. Fix the JSON syntax (comments and trailing commas are not valid JSON) and re-run install to merge them back in.",
                path.display()
            );
            None
        }
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
        // The template is authored with repo-root-relative link targets, but
        // this copy is written one directory level deeper, at
        // `.loom/CLAUDE.md` itself, so every such target needs re-basing
        // (issue #5975) — see `localize_dotloom_doc_links` for the two
        // shapes it rewrites. Only this destination needs the rewrite; the
        // short pointer written to root CLAUDE.md below carries no such
        // links.
        let loom_claude_md_localized = localize_dotloom_doc_links(&loom_substituted);
        let loom_claude_md_dst = loom_dir.join("CLAUDE.md");
        let loom_claude_md_existed = loom_claude_md_dst.exists();
        fs::write(&loom_claude_md_dst, &loom_claude_md_localized)
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

        let mut final_content = if existed {
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
                        // Issue #5384: `before` is the on-disk content that
                        // precedes the START marker, which on a real-world
                        // reinstall/upgrade already carries whatever trailing
                        // whitespace the PREVIOUS write left (typically the
                        // blank-line separator the no-markers/malformed-markers
                        // branches below insert on first install). Blindly
                        // concatenating `before` as-is would double that
                        // whitespace on every reinstall; blindly stripping it
                        // with `trim_end()` and NOT re-adding a separator (the
                        // prior bug) glues `before`'s last line directly onto
                        // `<!-- BEGIN LOOM ORCHESTRATION -->` with no newline
                        // between them, producing a Markdown run-on line.
                        // `trim_end()` + an explicit `\n\n` mirrors the
                        // no-markers/malformed-markers pattern below, which is
                        // naturally idempotent: every reinstall normalizes to
                        // exactly one blank line, regardless of how many
                        // separator newlines (if any) `before` already had.
                        format!("{}\n\n{}{}", before.trim_end(), wrapped_pointer, after)
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

        // Normalize trailing newline termination regardless of which branch
        // above assembled `final_content` (issue #6331) — every assembly path
        // above builds its pieces with `.trim()`/`.trim_end()`, which strips
        // any trailing newline the source content had, and none of them ever
        // re-add one. Doing it once here, immediately before the write,
        // avoids patching each assembly branch individually and also
        // self-heals a source file that never had a trailing newline at all.
        if !final_content.ends_with('\n') {
            final_content.push('\n');
        }

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
    // pre-#3000 layout) — but it still needs the same
    // `is_legacy_loom_managed_root` / `slice_is_discardable_legacy` heuristics
    // (issue #4888): a broken or interrupted prior install can leave a root
    // AGENTS.md carrying stale, unsubstituted Loom template text with missing
    // or malformed markers, and preserving that verbatim would reintroduce the
    // placeholders and trip `assert_no_placeholders` below. See the marker /
    // markerless branches further down.
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
        // Same rewrite as .loom/CLAUDE.md above (issue #5975) — a no-op today
        // since defaults/.loom/AGENTS.md has no `.loom/docs/...` link
        // targets, but keeps the two templates' write paths structurally
        // consistent rather than relying on that staying true by convention.
        let loom_agents_md_localized = localize_dotloom_doc_links(&agents_substituted);
        let loom_agents_md_dst = loom_dir.join("AGENTS.md");
        let loom_agents_md_existed = loom_agents_md_dst.exists();
        fs::write(&loom_agents_md_dst, &loom_agents_md_localized)
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

        let mut final_agents_content = if existed {
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
                        // Issue #5384: mirrors the CLAUDE.md fix above — see
                        // that call site's comment for the full rationale.
                        // `trim_end()` + explicit `\n\n` is naturally
                        // idempotent across repeat reinstalls.
                        format!("{}\n\n{}{}", before.trim_end(), wrapped_agents_pointer, after)
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

        // Normalize trailing newline termination — mirrors the CLAUDE.md fix
        // above (issue #6331); see that call site's comment for rationale.
        if !final_agents_content.ends_with('\n') {
            final_agents_content.push('\n');
        }

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
                    } else {
                        // Consumer-owned merge target (issue #5396): the directory
                        // copy above already recorded settings.json somewhere
                        // (added/updated) before this merge overwrote it — drop
                        // that stale entry and record the file as preserved, the
                        // same pattern install_labels_block uses for
                        // labels.yml, so the post-install byte verification
                        // (which expects installed == source) does not flag the
                        // intentional, legitimate divergence produced by the
                        // merge (e.g. a co-owner like Repo Skills' hooks).
                        report.added.retain(|f| f != SETTINGS_JSON_REL);
                        report.updated.retain(|f| f != SETTINGS_JSON_REL);
                        report.preserved.retain(|f| f != SETTINGS_JSON_REL);
                        report.preserved.push(SETTINGS_JSON_REL.to_string());
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

    // Install `.agents/skills/loom-<name>/SKILL.md` (issue #8673, contract
    // point 5) — the cross-vendor skill-discovery surface Codex, Kimi Code,
    // Mistral Vibe, and Grok read natively. Marker-gated rather than a plain
    // `copy_directory` merge: see `install_agent_skills` below for why.
    install_agent_skills(defaults_path, workspace_path, report)?;

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

/// Installs `.agents/skills/loom-<name>/SKILL.md` files generated by
/// `loom-daemon generate-agent-skills` (issue #8673, contract point 5) — the
/// cross-vendor skill-discovery surface Codex, Kimi Code, Mistral Vibe, and
/// Grok read natively, mirroring the `AGENTS.md` handling above.
///
/// Unlike [`copy_directory`]'s force-merge (which distinguishes "Loom-owned"
/// from "custom" purely by whether the relative path exists in `defaults/`),
/// this is **marker-gated per file**: a destination `SKILL.md` is only ever
/// written when it does not exist yet, OR it exists and starts with the
/// `crate::agent_skills::MARKER` line — the ownership signal every generated
/// file carries. A destination file at the same `loom-<name>/SKILL.md` path
/// that is missing the marker (consumer-authored from scratch, or a marker a
/// consumer deliberately removed to detach a file from generation) is left
/// completely alone and recorded as `preserved`, never silently overwritten
/// or reaped — the same contract `defaults/scripts/resync-installed.sh`
/// documents for this surface, applied here at install time.
fn install_agent_skills(
    defaults_path: &Path,
    workspace_path: &Path,
    report: &mut InitReport,
) -> Result<(), String> {
    let skills = match crate::agent_skills::generate_all(defaults_path) {
        Ok(skills) => skills,
        // A defaults/ tree with no roles/ directory (or malformed role
        // prompts) has nothing to install — soft no-op rather than failing
        // the whole scaffolding pass over an optional surface.
        Err(_) => return Ok(()),
    };

    let out_root = defaults_path.join(".agents").join("skills");
    for skill in &skills {
        let Ok(rel) = skill.out_path.strip_prefix(&out_root) else {
            continue;
        };
        let dst = workspace_path.join(".agents").join("skills").join(rel);
        let report_name = format!(".agents/skills/{}", rel.display());

        let existing = fs::read_to_string(&dst).ok();
        match existing {
            None => {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
                }
                fs::write(&dst, &skill.content)
                    .map_err(|e| format!("Failed to write {}: {e}", dst.display()))?;
                report.added.push(report_name);
            }
            Some(existing_content) if existing_content.contains(crate::agent_skills::MARKER) => {
                if existing_content != skill.content {
                    fs::write(&dst, &skill.content)
                        .map_err(|e| format!("Failed to write {}: {e}", dst.display()))?;
                    report.updated.push(report_name);
                } else {
                    report.preserved.push(report_name);
                }
            }
            Some(_) => {
                // No marker: consumer-authored or detached-from-generation.
                // Never overwritten — logged as preserved so an operator can
                // see it was deliberately skipped, not silently missed.
                report.preserved.push(report_name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

// A new sibling module rather than growing `scaffolding/tests.rs` in place —
// that file sits exactly at its file-size ratchet baseline
// (`.loom/docs/file-size-policy.md`), the same reason
// `defaults/scripts/tests/test-resync-installed-local-fix-guard.sh` was split
// out of `test-resync-installed.sh` rather than grown into it.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod agent_skills_tests;
