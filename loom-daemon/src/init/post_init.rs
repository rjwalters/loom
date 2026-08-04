//! Post-initialization operations
//!
//! Operations performed after file copying: manifest generation and gitignore updates.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::json;

use super::templates::LoomMetadata;

/// Gitignore patterns that would shadow installed Loom files like
/// `.loom/scripts/lib/*.sh`. If a user's gitignore contains any of these, the
/// installer must fail loudly: the files will exist on disk after install but
/// will not be committed to the repo, producing a "successful" install that
/// breaks on the next worktree (see issue #3287).
///
/// Note: `.loom/` is intentionally listed even though some users may *want*
/// to ignore the entire `.loom/` directory. In that mode they should not be
/// running `install-loom.sh` at all — the installer's job is to commit Loom
/// files into the target repo. Hard-failing here surfaces that mismatch
/// instead of producing a silently broken install.
const OVERBROAD_LOOM_PATTERNS: &[&str] = &[
    ".loom/",
    ".loom",
    ".loom/*",
    ".loom/**",
    ".loom/scripts/",
    ".loom/scripts",
    ".loom/scripts/*",
    ".loom/scripts/**",
    ".loom/scripts/lib/",
    ".loom/scripts/lib",
    ".loom/scripts/lib/*",
    ".loom/scripts/lib/*.sh",
];

/// Scan `.gitignore` for patterns that would block installed Loom files
/// (specifically `.loom/scripts/lib/*.sh`) from being committed.
///
/// Returns a sorted list of offending pattern lines (trimmed of whitespace).
/// Empty result means the gitignore is safe.
///
/// This is the detection half of the issue #3287 fix. The installer treats a
/// non-empty result as a hard error: the user must remove the broad pattern
/// (or scope it more narrowly) before installation can proceed.
pub fn find_overbroad_loom_patterns(workspace_path: &Path) -> Vec<String> {
    let gitignore_path = workspace_path.join(".gitignore");
    let Ok(contents) = fs::read_to_string(&gitignore_path) else {
        return Vec::new();
    };

    let mut found: Vec<String> = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        // Skip blanks, comments, and negation entries (a negation can't shadow files)
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if OVERBROAD_LOOM_PATTERNS.contains(&line) {
            found.push(line.to_string());
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Sentinel that opens the Loom-managed `.gitignore` block.
///
/// The managed block is delimited by [`GITIGNORE_BEGIN_MARKER`] and
/// [`GITIGNORE_END_MARKER`] so that both `update_gitignore` (add/refresh) and
/// `scripts/uninstall-loom.sh` (remove) operate on a single, self-locating,
/// contiguous region. This mirrors the `<!-- BEGIN/END LOOM ORCHESTRATION -->`
/// convention already used for CLAUDE.md. See issue #3590.
pub const GITIGNORE_BEGIN_MARKER: &str = "# >>> loom-managed (do not edit) >>>";

/// Sentinel that closes the Loom-managed `.gitignore` block.
pub const GITIGNORE_END_MARKER: &str = "# <<< loom-managed <<<";

/// Human-readable header emitted inside the managed block. Pre-#3590 installs
/// wrote this same line as a bare (markerless) header; detecting it lets us
/// migrate those installs to the marked form in place.
const GITIGNORE_BLOCK_HEADER: &str = "# Loom runtime state (don't commit these)";

/// Ephemeral/runtime files that should be ignored — the single source of truth
/// for the Loom-managed `.gitignore` block.
///
/// Keep in sync with the Loom source repo's `.gitignore`.
///
/// Phase 3.5 (#3402, epic #3372): removed patterns for retired daemon-brain
/// state files (`daemon-state.json`, archived `[0-9][0-9]-daemon-state.json`,
/// `progress/`, `stuck-history.json`, `alerts.json`, `health-metrics.json`)
/// and added `spawn-loop-state.json` for the Phase 1 spawn loop (#3374).
///
/// #3778: closed the drift where this installer-managed list had fallen behind
/// the source `.gitignore`, so a consumer repo's re-synced loom-managed block
/// still omitted several Loom-owned transient paths — added `.loom-managed`,
/// `.loom/exit-codes/`, `.loom/sweep-run/`, `.loom/stats/`, `.loom/spawn-loop.pid`,
/// and `.loom/stop-spawn-loop`. The marker-delimited block is refreshed in place
/// on every `update_gitignore`, so re-running the installer re-syncs consumers.
///
/// #4280: added `.loom/worktrees-local/` (machine-local worktree state observed
/// untracked-and-unignored in a consumer repo). Existing consumer installs
/// converge on this list via the new `loom-daemon update-gitignore` subcommand,
/// which `resync-installed.sh` invokes — the block was previously refreshed only
/// during a full `init`, so a fix here never reached repos between installs.
///
/// #5014: added `.loom/account-health.json` + `.loom/account-health.lock` (the
/// per-repo token-pool health cache and its sibling `mkdir` lock, written by the
/// daemon token pool) — Loom-owned runtime state that surfaced as untracked dirt
/// in 0.17.0. Also re-synced the committed source `.gitignore`, whose block had
/// drifted behind this list (was missing `.no-changes-needed` and `.loom/*.bak`).
/// `.loom/accounts.json` is intentionally left tracked: it is an optional,
/// committable per-repo profile allowlist, not runtime state.
///
/// #5267: added `.claude/worktrees/` — the agent harness's own
/// `isolation: worktree` checkouts, which land *inside* the main checkout just
/// like `.loom/worktrees/` but were never covered here. Unignored, each one is
/// a nested git repo, so a `git add -A` silently stages an embedded-repo
/// gitlink (a near-miss in gf180-trng, 2026-08-04, caught only by hand).
pub const EPHEMERAL_PATTERNS: &[&str] = &[
    ".loom-in-use",
    // Per-worktree builder progress checkpoint. Its WRITER moved from Python to
    // Rust in #4275 (`loom_tools.checkpoints` -> `loom-daemon checkpoint`,
    // behind `checkpoint.sh`), but the file, its path and this pattern are
    // unchanged — checkpoints remain live, and a builder's `git add -A` must
    // still never sweep one into a commit.
    ".loom-checkpoint",
    // Worktree sentinel dropped by worktree.sh into each issue worktree; must be
    // ignored so a builder's `git add -A` doesn't sweep it into a commit (#3778).
    ".loom-managed",
    // Builder "no changes needed" marker (builder.md § "Signaling No Changes
    // Needed"). Must stay untracked/gitignored so it is born absent in every
    // fresh worktree and a deliberate write doesn't get swept into a commit by
    // `git add -A` — a tracked copy on main defeated the signal entirely (#4635).
    ".no-changes-needed",
    ".loom/.daemon.pid",
    ".loom/.daemon.log",
    ".loom/daemon.sock",
    ".loom/daemon-loop.pid",
    ".loom/daemon-metrics.json",
    ".loom/loom-source-path",
    ".loom/spawn-loop-state.json",
    ".loom/spawn-loop.pid",
    ".loom/stop-spawn-loop",
    ".loom/issue-failures.json",
    ".loom/interventions/",
    ".loom/worktrees/",
    // Local-mode / machine-local worktree state observed untracked-and-unignored
    // in a consumer repo (anvil, 2026-07-28): `.loom/worktrees-local/<repo>/issue-N`
    // (#4280). Runtime worktree state, same class as `.loom/worktrees/`.
    ".loom/worktrees-local/",
    // Worktrees created inside the main checkout by the agent harness's
    // `isolation: worktree` mechanism — NOT Loom's own worktree state (that is
    // `.loom/worktrees/` above). Each is a nested git repo, so leaving it
    // unignored lets a `git add -A` stage an embedded-repo gitlink behind
    // nothing but an easily-missed advice block (#5267).
    ".claude/worktrees/",
    ".loom/state.json",
    ".loom/mcp-command.json",
    ".loom/activity.db",
    ".loom/claims/",
    ".loom/locks/",
    ".loom/signals/",
    ".loom/status/",
    ".loom/retry-state/",
    ".loom/exit-codes/",
    ".loom/sweep-checkpoint/",
    ".loom/sweep-run/",
    ".loom/stats/",
    ".loom/diagnostics/",
    ".loom/guide-docs-state.json",
    ".loom/metrics_state.json",
    ".loom/manifest.json",
    ".loom/stuck-config.json",
    ".loom/metrics/",
    ".loom/usage-cache.json",
    ".loom/claude-config/",
    // Secret-bearing token pool + repo-local account source (#3695). These
    // hold OAuth keys and must never be committed.
    ".loom/tokens/",
    ".loom/accounts.env",
    // Codex/token-pool per-repo health cache + its sibling `mkdir` lock, written
    // atomically by the daemon token pool (#5014). Machine-local runtime state
    // that surfaced as untracked dirt in 0.17.0. Not secret-bearing (account
    // names + reason categories only — never auth.json or child output), but
    // still never something to commit. NB: `.loom/accounts.json` is deliberately
    // NOT ignored — it is an optional, committable per-repo profile allowlist.
    ".loom/account-health.json",
    ".loom/account-health.lock",
    // Uncommitted canary confirmation sentinel (#3731). Its guardrail power comes
    // from being uncommitted, so it must never be tracked.
    ".loom/CANARY",
    ".loom/*.log",
    ".loom/*.sock",
    // Interrupted atomic writes under .loom/ (#4401). Several Loom writers use
    // the write-to-`<path>.tmp`-then-`mv` idiom — most visibly
    // `defaults/scripts/verify-install.sh generate`, which builds
    // `.loom/manifest.json.tmp` before renaming it over `.loom/manifest.json`.
    // The destination is ignored above, but the tmp sidecar was not, so a failed
    // or interrupted `mv` (a killed installer, a full disk) left a 1000+-line
    // untracked file that a consumer's `git add -A` swept into a commit. Defense
    // in depth: ignore the whole class rather than one filename.
    ".loom/*.tmp",
    // Rescue copy written by `init::merge_config_file` before it overwrites an
    // unparseable `.loom/config.json` with the shipped template (#4641). It
    // exists so an operator can recover hand-tuned keys after a torn write, but
    // it is machine-local salvage — never something to commit.
    ".loom/*.bak",
    ".loom/logs/",
];

/// Build the Loom-managed `.gitignore` block (marker lines + header + patterns),
/// with no leading or trailing newline. Callers add surrounding newlines.
fn managed_gitignore_block() -> String {
    let mut block = String::new();
    block.push_str(GITIGNORE_BEGIN_MARKER);
    block.push('\n');
    block.push_str(GITIGNORE_BLOCK_HEADER);
    block.push('\n');
    for pattern in EPHEMERAL_PATTERNS {
        block.push_str(pattern);
        block.push('\n');
    }
    block.push_str(GITIGNORE_END_MARKER);
    block
}

/// Derive the Loom source-checkout root from the resolved defaults directory.
///
/// The wrappers and daemon both point `init` at a `<root>/defaults` directory,
/// so the source root is that directory's parent. Returns `None` when the
/// directory is not named `defaults` (a bundled/embedded layout) or has no
/// parent — the caller then omits `loom_source` rather than recording a wrong
/// path. The result is canonicalized to an absolute path when possible so the
/// gitignored `.loom/loom-source-path` sidecar and `loom_source` key match what
/// the shell installer writes.
fn derive_loom_source(defaults_dir: &Path) -> Option<String> {
    if defaults_dir.file_name().and_then(|n| n.to_str()) != Some("defaults") {
        return None;
    }
    let root = defaults_dir.parent()?;
    // Prefer an absolute, symlink-resolved path (matches install.sh's
    // `loom_root`); fall back to the lexical parent if canonicalization fails
    // (e.g. the directory was removed between resolve and write).
    let resolved = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Some(resolved.to_string_lossy().into_owned())
}

/// Write `.loom/install-metadata.json` (and, when derivable, the gitignored
/// `.loom/loom-source-path` sidecar) so a direct `loom-daemon init` produces the
/// same version metadata the shell installers do (#4050).
///
/// Before this, only `install.sh`/`install-loom.sh` wrote these artifacts, so a
/// consumer that ran `loom-daemon init` directly (a supported entry point since
/// the machine-level binary of #3922) got a `.loom/` with no version metadata:
/// `/repo:update-tools` reported UNKNOWN and `manifest.json` carried empty
/// `loom_version`/`loom_commit`.
///
/// Schema-compatible with `finalize_quick_install` (at least `loom_version`,
/// `loom_commit`, `install_date`; `installed_files` may be empty —
/// `verify-install.sh` warns-and-falls-back on an empty list). The wrappers run
/// `finalize_quick_install` *after* `init`, overwriting this file with the
/// richer version (populated `loom_source` + `installed_files`), so both paths
/// converge and this write never regresses a wrapper install.
///
/// Non-fatal: a write failure warns but does not abort the install (mirrors
/// [`generate_manifest`]).
pub fn write_install_metadata(workspace_path: &Path, metadata: &LoomMetadata, defaults_dir: &Path) {
    let loom_path = workspace_path.join(".loom");
    if !loom_path.exists() {
        return;
    }

    let version = metadata.version.as_deref().unwrap_or("unknown");
    let commit = metadata.commit.as_deref().unwrap_or("unknown");
    let source = derive_loom_source(defaults_dir);

    let mut obj = json!({
        "loom_version": version,
        "loom_commit": commit,
        "install_date": metadata.install_date,
        "installed_files": [],
    });
    // Only record loom_source when it can be derived — never a wrong path.
    if let Some(src) = source.as_deref() {
        obj["loom_source"] = json!(src);
    }

    match serde_json::to_string_pretty(&obj) {
        Ok(mut contents) => {
            contents.push('\n');
            if let Err(e) = fs::write(loom_path.join("install-metadata.json"), contents) {
                eprintln!("Warning: Could not write .loom/install-metadata.json: {e}");
            }
        }
        Err(e) => {
            eprintln!("Warning: Could not serialize install metadata: {e}");
        }
    }

    // Machine-local source sidecar (gitignored via EPHEMERAL_PATTERNS). Only
    // written when the source root is known; consumers have a fallback chain
    // that recreates it from `install-metadata.json`'s `loom_source` otherwise.
    if let Some(src) = source {
        if let Err(e) = fs::write(loom_path.join("loom-source-path"), format!("{src}\n")) {
            eprintln!("Warning: Could not write .loom/loom-source-path: {e}");
        }
    }
}

/// Generate installation manifest by running verify-install.sh
///
/// Attempts to run `.loom/scripts/verify-install.sh generate --quiet` to create
/// `.loom/manifest.json` with SHA-256 checksums of all installed files.
/// This is non-fatal - manifest generation failure doesn't prevent installation.
pub fn generate_manifest(workspace_path: &Path) {
    let script = workspace_path
        .join(".loom")
        .join("scripts")
        .join("verify-install.sh");

    if !script.exists() {
        return;
    }

    let result = Command::new("bash")
        .arg(&script)
        .arg("generate")
        .arg("--quiet")
        .current_dir(workspace_path)
        .output();

    match result {
        Ok(output) => {
            if !output.status.success() {
                eprintln!(
                    "Warning: Manifest generation failed (exit {})",
                    output.status.code().unwrap_or(-1)
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: Could not run verify-install.sh: {e}");
        }
    }
}

/// Update .gitignore with Loom ephemeral patterns
///
/// Adds patterns for ephemeral Loom files that shouldn't be committed.
/// Creates .gitignore if it doesn't exist.
pub fn update_gitignore(workspace_path: &Path) -> Result<(), String> {
    let gitignore_path = workspace_path.join(".gitignore");
    let block = managed_gitignore_block();

    // Create a fresh .gitignore containing only the managed block.
    if !gitignore_path.exists() {
        fs::write(&gitignore_path, format!("{block}\n"))
            .map_err(|e| format!("Failed to create .gitignore: {e}"))?;
        return Ok(());
    }

    let contents = fs::read_to_string(&gitignore_path)
        .map_err(|e| format!("Failed to read .gitignore: {e}"))?;

    // Split on '\n'. When the file ends with '\n', the trailing element is an
    // empty string; joining back with '\n' reproduces the original bytes
    // exactly, so line-vector edits are byte-preserving for untouched regions.
    let mut lines: Vec<String> = contents.split('\n').map(str::to_string).collect();

    // Remove legacy over-broad patterns that would shadow *any* installed
    // `.loom/*.json` file (older installs and /imagine used `.loom/*.json`),
    // plus the negation that was paired with that glob. These are genuinely
    // dangerous — they block files this installer never asked the user to
    // ignore (`.loom/install-metadata.json`, `.loom/config/skill-routes.json`,
    // …) — so they are always removed regardless of who wrote them.
    //
    // #5242: `.loom/config.json` itself is deliberately NOT in this list.
    // It used to be here (see the historical note below), on the theory that
    // any occurrence was a leftover from the pre-#2278 bug where `.loom/config.json`
    // was gitignored by mistake, blocking the merge-aware tracked-config design
    // (#3598) from working. That theory stopped holding once fleet hosts started
    // adding this exact line *on purpose*: `.loom/config.json` is committed
    // team config by default, but a host that keeps genuinely host-local runtime
    // state there (e.g. a `worktree.root` override) has a legitimate reason to
    // gitignore it — and this installer never adds that rule itself (it is not
    // in EPHEMERAL_PATTERNS), so per the "never remove ignore rules we didn't
    // add" rule it must not strip it either. Removing it here fought that
    // documented, intentional divergence on every subsequent install/update
    // (rjwalters/lean-genius#43683). A single scoped `.loom/config.json` line
    // does not shadow any other installed file, so leaving it alone is safe.
    let legacy_overbroad = [".loom/*.json", "!.loom/roles/*.json"];
    lines.retain(|line| !legacy_overbroad.contains(&line.trim()));

    let begin = lines
        .iter()
        .position(|l| l.trim() == GITIGNORE_BEGIN_MARKER);
    let end = lines.iter().position(|l| l.trim() == GITIGNORE_END_MARKER);

    let block_lines: Vec<String> = block.split('\n').map(str::to_string).collect();

    match (begin, end) {
        (Some(b), Some(e)) if b <= e => {
            // Marked block already present: refresh it in place (patterns may
            // have changed between versions) without moving it.
            lines.splice(b..=e, block_lines.iter().cloned());
        }
        _ => {
            // No well-formed BEGIN <= END pair. This covers a legacy markerless
            // block, a fresh consumer file, and corrupted single/misordered
            // markers. Migrate in place rather than relocating to EOF (#3592).

            // 1. Normalize orphan/corrupted markers. Because we did not match the
            //    in-place arm, any marker line present here is stray (an END-only
            //    orphan, a BEGIN-only orphan, or an END-before-BEGIN pair). Drop
            //    them so they can neither accumulate every run (orphan-END
            //    unbounded growth) nor drive a destructive splice that swallows
            //    user content (orphan-BEGIN). #3592
            lines.retain(|line| {
                let t = line.trim();
                t != GITIGNORE_BEGIN_MARKER && t != GITIGNORE_END_MARKER
            });

            // 2. Locate the legacy markerless block: the bare header and/or bare
            //    ephemeral pattern lines. Remember where the first such line sits
            //    so the marked block can be spliced back into that same span.
            let is_legacy = |line: &str| {
                let t = line.trim();
                t == GITIGNORE_BLOCK_HEADER || EPHEMERAL_PATTERNS.contains(&t)
            };
            let first_legacy = lines.iter().position(|l| is_legacy(l));

            match first_legacy {
                Some(start) => {
                    // Remove every legacy line, then splice the marked block into
                    // the original position. Because we replace in place instead
                    // of removing-then-appending, flanking blank lines stay put:
                    // the block does not relocate to EOF and no double-blank
                    // artifact is left behind where the old block used to sit
                    // (#3592). Any user content that followed the legacy block
                    // still follows the managed block.
                    lines.retain(|l| !is_legacy(l));
                    lines.splice(start..start, block_lines.iter().cloned());
                }
                None => {
                    // Genuinely no Loom header/patterns/markers: create the block
                    // at EOF. Drop exactly one trailing empty element (the file's
                    // final '\n') so the block is appended directly after the
                    // existing content with no spurious blank line. This makes the
                    // append the exact inverse of uninstall's marker-span
                    // deletion, so an install -> uninstall -> install round-trip
                    // is byte-identical (issue #3590).
                    if lines.last().is_some_and(String::is_empty) {
                        lines.pop();
                    }
                    for bl in &block_lines {
                        lines.push(bl.clone());
                    }
                    // Re-add exactly one trailing empty element => single '\n'.
                    lines.push(String::new());
                }
            }
        }
    }

    let new_contents = lines.join("\n");
    if new_contents != contents {
        fs::write(&gitignore_path, &new_contents)
            .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn metadata(version: &str, commit: &str) -> LoomMetadata {
        LoomMetadata {
            version: Some(version.to_string()),
            commit: Some(commit.to_string()),
            install_date: "2026-07-27".to_string(),
        }
    }

    #[test]
    fn write_install_metadata_writes_version_and_commit() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir(workspace.join(".loom")).unwrap();
        // A `<root>/defaults` dir so loom_source is derivable.
        let defaults = workspace.join("srcroot").join("defaults");
        fs::create_dir_all(&defaults).unwrap();

        write_install_metadata(workspace, &metadata("0.15.0", "ebf4fc55"), &defaults);

        let raw =
            fs::read_to_string(workspace.join(".loom").join("install-metadata.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["loom_version"], "0.15.0");
        assert_eq!(v["loom_commit"], "ebf4fc55");
        assert_eq!(v["install_date"], "2026-07-27");
        assert!(v["installed_files"].is_array());
        // loom_source is the parent of the `defaults` dir, canonicalized.
        let src = v["loom_source"].as_str().unwrap();
        assert!(src.ends_with("srcroot"), "loom_source should be the defaults parent, got {src}");

        // The gitignored sidecar mirrors loom_source.
        let sidecar = fs::read_to_string(workspace.join(".loom").join("loom-source-path")).unwrap();
        assert_eq!(sidecar.trim(), src);
    }

    #[test]
    fn write_install_metadata_omits_source_when_not_defaults() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir(workspace.join(".loom")).unwrap();
        // Not named `defaults` → loom_source cannot be derived and must be omitted.
        let bundled = workspace.join("resources");
        fs::create_dir_all(&bundled).unwrap();

        write_install_metadata(workspace, &metadata("0.15.0", "abc1234"), &bundled);

        let raw =
            fs::read_to_string(workspace.join(".loom").join("install-metadata.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["loom_version"], "0.15.0");
        assert!(v.get("loom_source").is_none(), "loom_source must be omitted, not wrong");
        // No sidecar written when the source root is unknown.
        assert!(!workspace.join(".loom").join("loom-source-path").exists());
    }

    #[test]
    fn write_install_metadata_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir(workspace.join(".loom")).unwrap();
        let defaults = workspace.join("srcroot").join("defaults");
        fs::create_dir_all(&defaults).unwrap();

        write_install_metadata(workspace, &metadata("0.15.0", "abc1234"), &defaults);
        let first =
            fs::read_to_string(workspace.join(".loom").join("install-metadata.json")).unwrap();
        write_install_metadata(workspace, &metadata("0.15.0", "abc1234"), &defaults);
        let second =
            fs::read_to_string(workspace.join(".loom").join("install-metadata.json")).unwrap();

        assert_eq!(first, second, "re-running init must not garble the JSON");
        // Parses cleanly (no duplicate/concatenated objects).
        serde_json::from_str::<serde_json::Value>(&second).unwrap();
    }

    #[test]
    fn write_install_metadata_falls_back_to_unknown_when_absent() {
        // A LoomMetadata with no version/commit (both env AND compiled empty)
        // still produces valid JSON with the "unknown" placeholder rather than
        // panicking or writing null.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir(workspace.join(".loom")).unwrap();
        let defaults = workspace.join("srcroot").join("defaults");
        fs::create_dir_all(&defaults).unwrap();

        let meta = LoomMetadata {
            version: None,
            commit: None,
            install_date: "2026-07-27".to_string(),
        };
        write_install_metadata(workspace, &meta, &defaults);

        let raw =
            fs::read_to_string(workspace.join(".loom").join("install-metadata.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["loom_version"], "unknown");
        assert_eq!(v["loom_commit"], "unknown");
    }

    #[test]
    fn derive_loom_source_handles_defaults_and_non_defaults() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("checkout");
        let defaults = root.join("defaults");
        fs::create_dir_all(&defaults).unwrap();

        let src = derive_loom_source(&defaults).unwrap();
        assert!(src.ends_with("checkout"));

        // A directory not named `defaults` yields None.
        assert!(derive_loom_source(&root).is_none());
    }

    #[test]
    fn creates_gitignore_with_all_patterns_when_none_exists() {
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        // Spot-check key runtime patterns
        assert!(contents.contains(".loom-in-use"));
        assert!(contents.contains(".loom-checkpoint"));
        assert!(contents.contains(".loom/spawn-loop-state.json"));
        assert!(contents.contains(".loom/worktrees/"));
        assert!(contents.contains(".loom/*.log"));
        assert!(contents.contains(".loom/logs/"));
        assert!(contents.contains(".loom/daemon-metrics.json"));
        assert!(contents.contains(".loom/activity.db"));
        assert!(contents.contains(".loom/issue-failures.json"));
        assert!(contents.contains(".loom/usage-cache.json"));
        // Runtime dirs added in #3635 — must appear exactly once
        assert_eq!(contents.matches(".loom/sweep-checkpoint/").count(), 1);
        assert_eq!(contents.matches(".loom/locks/").count(), 1);
        assert!(contents.contains("# Loom runtime state"));

        // #4280: machine-local worktree state must be ignored and appear once.
        assert_eq!(contents.matches(".loom/worktrees-local/").count(), 1);

        // #5267: harness `isolation: worktree` checkouts must be ignored and
        // appear once, else `git add -A` stages an embedded-repo gitlink.
        assert_eq!(contents.matches(".claude/worktrees/").count(), 1);

        // #3778: patterns that had drifted out of this installer-managed list
        // relative to the source .gitignore — a consumer re-sync must now emit
        // them so Loom-owned transient state never surfaces as untracked dirt.
        assert!(contents.contains(".loom-managed"));
        assert!(contents.contains(".loom/exit-codes/"));
        assert!(contents.contains(".loom/sweep-run/"));
        assert!(contents.contains(".loom/stats/"));
        assert!(contents.contains(".loom/spawn-loop.pid"));
        assert!(contents.contains(".loom/stop-spawn-loop"));

        // #4635: the builder "no changes needed" marker must be ignored so it
        // is born absent in every fresh worktree.
        assert!(contents.contains(".no-changes-needed"));

        // #5014: the per-repo token-pool health cache + its sibling mkdir lock
        // must be ignored so 0.17.0 runtime state never surfaces as untracked
        // dirt. The optional, committable `.loom/accounts.json` must NOT be.
        assert!(contents.contains(".loom/account-health.json"));
        assert!(contents.contains(".loom/account-health.lock"));
        assert!(!contents.contains(".loom/accounts.json"));

        // Retired daemon-brain patterns must NOT be emitted (Phase 3.5, #3402)
        assert!(!contents.contains(".loom/daemon-state.json"));
        assert!(!contents.contains(".loom/[0-9][0-9]-daemon-state.json"));
        assert!(!contents.contains(".loom/progress/"));
        assert!(!contents.contains(".loom/stuck-history.json"));
        assert!(!contents.contains(".loom/alerts.json"));
        assert!(!contents.contains(".loom/health-metrics.json"));

        // config.json must NOT be gitignored
        assert!(!contents.contains(".loom/config.json"));
        assert!(!contents.contains(".loom/*.json"));
    }

    #[test]
    fn appends_missing_patterns_to_existing_gitignore() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // Pre-existing gitignore with only old patterns
        fs::write(&gitignore, "node_modules/\n.loom/state.json\n.loom/worktrees/\n").unwrap();

        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(&gitignore).unwrap();

        // Original content preserved
        assert!(contents.contains("node_modules/"));
        // Pre-existing patterns not duplicated
        assert_eq!(contents.matches(".loom/state.json").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);
        // New patterns added
        assert!(contents.contains(".loom-in-use"));
        assert!(contents.contains(".loom/spawn-loop-state.json"));
        assert!(contents.contains(".loom/daemon-metrics.json"));
        assert!(contents.contains(".loom/activity.db"));
        // #5267: the harness's `isolation: worktree` directory is added exactly
        // once on a fresh install over an existing (unmarked) .gitignore.
        assert_eq!(contents.matches(".claude/worktrees/").count(), 1);
    }

    #[test]
    fn does_not_duplicate_patterns_on_repeated_runs() {
        let tmp = TempDir::new().unwrap();

        update_gitignore(tmp.path()).unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        assert_eq!(contents.matches(".loom/spawn-loop-state.json").count(), 1);
        assert_eq!(contents.matches(".loom-in-use").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);
        assert_eq!(contents.matches(".loom/sweep-checkpoint/").count(), 1);
        assert_eq!(contents.matches(".loom/locks/").count(), 1);
        // #4280: `.loom/worktrees-local/` must not duplicate across runs.
        assert_eq!(contents.matches(".loom/worktrees-local/").count(), 1);
        // #5267: `.claude/worktrees/` must not duplicate across runs either.
        assert_eq!(contents.matches(".claude/worktrees/").count(), 1);
        // #4635: the builder "no changes needed" marker must not duplicate
        // across runs either.
        assert_eq!(contents.matches(".no-changes-needed").count(), 1);
    }

    #[test]
    fn covers_all_source_repo_gitignore_patterns() {
        // Verify that every Loom runtime pattern from the source .gitignore
        // is present in the ephemeral_patterns list by running update_gitignore
        // and checking the output.
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        let expected = [
            ".loom-in-use",
            ".loom-checkpoint",
            // #3838: the worktree sentinel worktree.sh drops into each issue
            // worktree must be ignored, else a builder's `git add -A` commits it.
            ".loom-managed",
            // #4635: builder "no changes needed" marker (builder.md § "Signaling
            // No Changes Needed") must stay untracked/gitignored.
            ".no-changes-needed",
            ".loom/.daemon.pid",
            ".loom/.daemon.log",
            ".loom/daemon.sock",
            ".loom/daemon-loop.pid",
            ".loom/daemon-metrics.json",
            ".loom/loom-source-path",
            ".loom/spawn-loop-state.json",
            ".loom/issue-failures.json",
            ".loom/interventions/",
            ".loom/worktrees/",
            ".loom/worktrees-local/",
            // #5267: harness `isolation: worktree` checkouts inside the main
            // checkout — nested git repos a `git add -A` would otherwise stage
            // as an embedded-repo gitlink.
            ".claude/worktrees/",
            ".loom/state.json",
            ".loom/mcp-command.json",
            ".loom/activity.db",
            ".loom/claims/",
            ".loom/locks/",
            ".loom/signals/",
            ".loom/status/",
            ".loom/retry-state/",
            ".loom/sweep-checkpoint/",
            ".loom/diagnostics/",
            ".loom/guide-docs-state.json",
            ".loom/metrics_state.json",
            ".loom/manifest.json",
            ".loom/stuck-config.json",
            ".loom/metrics/",
            ".loom/usage-cache.json",
            ".loom/claude-config/",
            // #5014: per-repo token-pool health cache + its sibling mkdir lock.
            ".loom/account-health.json",
            ".loom/account-health.lock",
            ".loom/*.log",
            ".loom/*.sock",
            // #4401: tmp sidecars from interrupted atomic writes (e.g.
            // `.loom/manifest.json.tmp` from verify-install.sh's `generate`).
            ".loom/*.tmp",
            // #4641/#5014: salvage/backup sidecars from torn atomic writes.
            ".loom/*.bak",
            ".loom/logs/",
        ];

        for pattern in &expected {
            assert!(
                contents.contains(pattern),
                "Missing pattern in generated .gitignore: {pattern}"
            );
        }

        // Retired patterns (Phase 3.5, #3402) must no longer appear
        let retired = [
            ".loom/daemon-state.json",
            ".loom/[0-9][0-9]-daemon-state.json",
            ".loom/progress/",
            ".loom/stuck-history.json",
            ".loom/alerts.json",
            ".loom/health-metrics.json",
        ];
        for pattern in &retired {
            assert!(
                !contents.contains(pattern),
                "Retired pattern should not be in generated .gitignore: {pattern}"
            );
        }
    }

    #[test]
    fn find_overbroad_loom_patterns_empty_when_no_gitignore() {
        let tmp = TempDir::new().unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert!(
            found.is_empty(),
            "Expected empty result when .gitignore is absent, got {found:?}"
        );
    }

    #[test]
    fn find_overbroad_loom_patterns_detects_dot_loom_slash() {
        // Issue #3287: a target repo with `.loom/` in .gitignore would have all
        // installed Loom files (including `.loom/scripts/lib/*.sh`) silently
        // dropped at commit time.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "node_modules/\n.loom/\nsome-other-pattern\n")
            .unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/".to_string()]);
    }

    #[test]
    fn find_overbroad_loom_patterns_detects_dot_loom_scripts() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), ".loom/scripts/\n# comment\n").unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/scripts/".to_string()]);
    }

    #[test]
    fn find_overbroad_loom_patterns_detects_lib_specifically() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), ".loom/scripts/lib/*.sh\n").unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/scripts/lib/*.sh".to_string()]);
    }

    #[test]
    fn find_overbroad_loom_patterns_ignores_safe_runtime_patterns() {
        // The patterns written by update_gitignore() are runtime-only and
        // must not be flagged as over-broad.
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert!(found.is_empty(), "update_gitignore output should be safe, got: {found:?}");
    }

    #[test]
    fn find_overbroad_loom_patterns_ignores_comments_and_negations() {
        let tmp = TempDir::new().unwrap();
        // Negations and comments mentioning `.loom/` must not be flagged
        fs::write(
            tmp.path().join(".gitignore"),
            "# .loom/ — see docs\n!.loom/scripts/lib/\n.loom/worktrees/\n",
        )
        .unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert!(found.is_empty(), "Expected no flags, got: {found:?}");
    }

    #[test]
    fn find_overbroad_loom_patterns_dedups_and_sorts() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), ".loom/scripts/\n.loom/\n.loom/scripts/\n")
            .unwrap();
        let found = find_overbroad_loom_patterns(tmp.path());
        assert_eq!(found, vec![".loom/".to_string(), ".loom/scripts/".to_string()]);
    }

    #[test]
    fn initialize_workspace_rejects_overbroad_gitignore() {
        // End-to-end: an install against a target with `.loom/` in .gitignore
        // must fail with a clear error message (issue #3287). Without this
        // check, files copy successfully on disk but never make it into the
        // commit, producing a "successful" install that is silently broken.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let defaults = tmp.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join("roles")).unwrap();
        fs::create_dir_all(defaults.join("scripts").join("lib")).unwrap();
        fs::write(defaults.join("config.json"), "{}").unwrap();
        fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
        fs::write(
            defaults
                .join("scripts")
                .join("lib")
                .join("forge-helpers.sh"),
            "#!/bin/bash",
        )
        .unwrap();

        // Hostile .gitignore: `.loom/` would prevent the lib files from ever
        // being committed (the file copy would succeed, but git would silently
        // drop them at commit time).
        fs::write(workspace.join(".gitignore"), ".loom/\n").unwrap();

        let result = crate::init::initialize_workspace(
            workspace.to_str().unwrap(),
            defaults.to_str().unwrap(),
            false,
        );
        assert!(result.is_err(), "install should refuse hostile .gitignore but returned Ok");
        if let Err(err) = result {
            assert!(
                err.contains(".loom/") && err.contains("Refusing to install"),
                "error message should call out the offending pattern; got: {err}"
            );
        }
    }

    #[test]
    fn removes_legacy_broad_json_pattern_but_preserves_config_json_rule() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // Simulate a gitignore from an older install or /imagine with the legacy patterns
        fs::write(
            &gitignore,
            "node_modules/\n\n# Loom\n.loom/config.json\n.loom/state.json\n.loom/*.json\n!.loom/roles/*.json\n.loom/worktrees/\n",
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(&gitignore).unwrap();

        // The genuinely over-broad patterns (which would shadow *any* installed
        // `.loom/*.json` file, not just config.json) must still be removed.
        assert!(
            !contents.contains(".loom/*.json"),
            "Legacy .loom/*.json pattern should have been removed"
        );
        assert!(
            !contents.contains("!.loom/roles/*.json"),
            "Legacy negation pattern should have been removed"
        );

        // #5242: a narrowly-scoped `.loom/config.json` ignore rule must survive.
        // This installer never adds this line itself (it is not in
        // EPHEMERAL_PATTERNS), so it must not remove it either — some hosts add
        // it deliberately to keep host-local runtime state (e.g. worktree.root
        // overrides) out of a shared repo's tracked config.
        assert!(
            contents.contains(".loom/config.json"),
            "A pre-existing, narrowly-scoped .loom/config.json ignore rule must be preserved"
        );

        // Specific ephemeral patterns should be added instead
        assert!(contents.contains(".loom/spawn-loop-state.json"));
        assert!(contents.contains(".loom/state.json"));
        assert!(contents.contains(".loom/worktrees/"));

        // Non-Loom content preserved
        assert!(contents.contains("node_modules/"));
    }

    #[test]
    fn preserves_standalone_config_json_ignore_rule_across_repeated_updates() {
        // #5242 regression: a repo that deliberately keeps `.loom/config.json`
        // gitignored (documented host-local runtime state, e.g. a fleet host's
        // `worktree.root` override) must not have that rule stripped again on
        // every subsequent `update_gitignore` run (install/update/resync).
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        fs::write(
            &gitignore,
            "node_modules/\n\n# Host-local divergence: worktree.root is per-machine\n.loom/config.json\n",
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        update_gitignore(tmp.path()).unwrap();

        let contents = fs::read_to_string(&gitignore).unwrap();

        assert!(
            contents.contains(".loom/config.json"),
            "a standalone, user-added .loom/config.json ignore rule must survive repeated \
             update_gitignore runs, got: {contents}"
        );
        assert!(contents.contains("node_modules/"));
    }

    /// Mimic `scripts/uninstall-loom.sh`'s marker-span deletion: drop every
    /// line from the BEGIN marker through the END marker (inclusive).
    fn remove_managed_block(contents: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        let mut in_block = false;
        for line in contents.split('\n') {
            if line.trim() == GITIGNORE_BEGIN_MARKER {
                in_block = true;
                continue;
            }
            if in_block {
                if line.trim() == GITIGNORE_END_MARKER {
                    in_block = false;
                }
                continue;
            }
            out.push(line);
        }
        out.join("\n")
    }

    #[test]
    fn create_wraps_patterns_in_markers() {
        let tmp = TempDir::new().unwrap();
        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();

        assert!(contents.contains(GITIGNORE_BEGIN_MARKER), "missing BEGIN marker");
        assert!(contents.contains(GITIGNORE_END_MARKER), "missing END marker");
        // BEGIN must precede END, and each appears exactly once.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);
        let b = contents.find(GITIGNORE_BEGIN_MARKER).unwrap();
        let e = contents.find(GITIGNORE_END_MARKER).unwrap();
        assert!(b < e, "BEGIN marker must precede END marker");
        // Patterns live inside the block.
        assert!(contents.contains(".loom/worktrees/"));
    }

    #[test]
    fn in_place_update_does_not_move_or_duplicate_block() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // User content that already carries a marked block, followed by more
        // user content AFTER the block (so an EOF re-append would reorder it).
        fs::write(
            &gitignore,
            format!(
                "node_modules/\n{block}\n# trailing user content\ndist/\n",
                block = managed_gitignore_block()
            ),
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // Exactly one block, refreshed in place.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);
        // Content after the block is untouched and still after the block.
        let end_pos = contents.find(GITIGNORE_END_MARKER).unwrap();
        let trailing_pos = contents.find("# trailing user content").unwrap();
        assert!(trailing_pos > end_pos, "trailing user content must not move");
        assert!(contents.contains("node_modules/"));
        assert!(contents.contains("dist/"));
    }

    #[test]
    fn roundtrip_is_byte_identical() {
        // install -> uninstall -> install on a committed, customized .gitignore
        // (whose Loom block is exactly what install produces) must be a no-op at
        // the byte level. This is the core acceptance criterion of issue #3590.
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // A realistic consumer .gitignore.
        fs::write(&gitignore, "# consumer rules\nnode_modules/\n__pycache__/\n.venv/\n*.log\n")
            .unwrap();

        // First install writes the managed block.
        update_gitignore(tmp.path()).unwrap();
        let after_install = fs::read_to_string(&gitignore).unwrap();
        assert!(after_install.contains(GITIGNORE_BEGIN_MARKER));

        // Simulate `uninstall-loom.sh` removing the marked span, then reinstall.
        let after_uninstall = remove_managed_block(&after_install);
        fs::write(&gitignore, &after_uninstall).unwrap();
        update_gitignore(tmp.path()).unwrap();
        let after_reinstall = fs::read_to_string(&gitignore).unwrap();

        assert_eq!(
            after_install, after_reinstall,
            "install -> uninstall -> install must be byte-identical"
        );

        // And uninstall fully removes the Loom block (no residual patterns).
        assert!(!after_uninstall.contains(GITIGNORE_BEGIN_MARKER));
        assert!(!after_uninstall.contains(GITIGNORE_END_MARKER));
        assert!(!after_uninstall.contains(".loom/worktrees/"));
        assert!(!after_uninstall.contains("# Loom runtime state"));
        // User content survives the uninstall verbatim.
        assert_eq!(
            after_uninstall,
            "# consumer rules\nnode_modules/\n__pycache__/\n.venv/\n*.log\n"
        );
    }

    #[test]
    fn migrates_legacy_markerless_block_in_place() {
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // Pre-#3590 install: bare header + bare patterns, no markers.
        fs::write(
            &gitignore,
            "node_modules/\n# Loom runtime state (don't commit these)\n\
             .loom/state.json\n.loom/worktrees/\n.loom/*.log\n",
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // Migrated to the marked form.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);
        // No duplicate bare patterns outside the block.
        assert_eq!(contents.matches(".loom/state.json").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);
        // Now idempotent: a second run is a no-op.
        update_gitignore(tmp.path()).unwrap();
        let again = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(contents, again, "post-migration update must be idempotent");
    }

    #[test]
    fn migrates_mid_file_legacy_block_in_place_without_double_blank() {
        // Regression for #3592: a legacy markerless block sitting mid-file,
        // flanked by a blank line on each side with user content AFTER it, must
        // migrate to the marked form (a) in place (not relocated to EOF),
        // (b) without leaving a double-blank artifact, and (c) idempotently.
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        fs::write(
            &gitignore,
            "node_modules/\n\n# Loom runtime state (don't commit these)\n\
             .loom-in-use\n.loom/state.json\n.loom/worktrees/\n.loom/logs/\n\
             \ndist/\n",
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // (a) Exactly one BEGIN and one END marker.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);

        // (b) The block stayed in place: user content that followed the legacy
        // block still follows the managed block, and content that preceded it
        // still precedes it.
        let begin_pos = contents.find(GITIGNORE_BEGIN_MARKER).unwrap();
        let end_pos = contents.find(GITIGNORE_END_MARKER).unwrap();
        let node_pos = contents.find("node_modules/").unwrap();
        let dist_pos = contents.find("dist/").unwrap();
        assert!(node_pos < begin_pos, "node_modules/ must stay before the block");
        assert!(dist_pos > end_pos, "dist/ must stay after the block");

        // (c) No double-blank artifact anywhere.
        assert!(
            !contents.contains("\n\n\n"),
            "migration must not leave a double-blank artifact: {contents:?}"
        );

        // No duplicate patterns outside the block.
        assert_eq!(contents.matches(".loom/state.json").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees/").count(), 1);

        // (d) A second run is a byte-identical no-op.
        update_gitignore(tmp.path()).unwrap();
        let again = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(contents, again, "second migration run must be a byte-identical no-op");
    }

    #[test]
    fn refreshes_stale_marked_block_to_include_new_runtime_patterns() {
        // #4280: a consumer whose marker-delimited block was written by a
        // pre-#3642 binary lacks `.loom/sweep-checkpoint/` (and `.loom/worktrees-local/`).
        // A single `update_gitignore` must refresh the block in place so those
        // runtime paths become ignored — this is what the resync entry point drives.
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        // Seed a well-formed but stale managed block (a realistic pre-#3642 subset),
        // flanked by user content on both sides.
        let stale_block = format!(
            "{begin}\n{header}\n.loom-in-use\n.loom/state.json\n.loom/worktrees/\n.loom/logs/\n{end}",
            begin = GITIGNORE_BEGIN_MARKER,
            header = GITIGNORE_BLOCK_HEADER,
            end = GITIGNORE_END_MARKER,
        );
        fs::write(&gitignore, format!("node_modules/\n{stale_block}\ndist/\n")).unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // Exactly one block, markers preserved.
        assert_eq!(contents.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(GITIGNORE_END_MARKER).count(), 1);
        // The previously-absent runtime patterns are now present exactly once.
        assert_eq!(contents.matches(".loom/sweep-checkpoint/").count(), 1);
        assert_eq!(contents.matches(".loom/worktrees-local/").count(), 1);
        // #5267: the harness `isolation: worktree` directory is backfilled by
        // the same in-place refresh — this is exactly what resync-installed.sh's
        // `loom-daemon update-gitignore` invocation drives for existing installs.
        assert_eq!(contents.matches(".claude/worktrees/").count(), 1);
        // User content on both sides survived and kept its ordering.
        let begin_pos = contents.find(GITIGNORE_BEGIN_MARKER).unwrap();
        let end_pos = contents.find(GITIGNORE_END_MARKER).unwrap();
        assert!(contents.find("node_modules/").unwrap() < begin_pos);
        assert!(contents.find("dist/").unwrap() > end_pos);

        // Idempotent: a second run is a byte-identical no-op.
        update_gitignore(tmp.path()).unwrap();
        let again = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(contents, again, "second refresh must be a byte-identical no-op");
    }

    #[test]
    fn hand_added_claude_worktrees_rule_survives_block_refresh_without_block_duplication() {
        // #5267, the gf180-bandgap case: before `.claude/worktrees/` was managed,
        // some repos hand-added the rule *above* the loom-managed block. When the
        // managed block then gains the same pattern, the block itself must still
        // carry it exactly once, and the operator's own line must survive — Loom
        // only rewrites the span between its markers and never strips ignore rules
        // it did not write. The resulting extra line is a semantic no-op for git
        // (the same behavior any hand-added `.loom/worktrees/` already gets).
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        let stale_block = format!(
            "{begin}\n{header}\n.loom-in-use\n.loom/worktrees/\n{end}",
            begin = GITIGNORE_BEGIN_MARKER,
            header = GITIGNORE_BLOCK_HEADER,
            end = GITIGNORE_END_MARKER,
        );
        fs::write(
            &gitignore,
            format!("# harness worktree isolation\n.claude/worktrees/\n\n{stale_block}\n"),
        )
        .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let contents = fs::read_to_string(&gitignore).unwrap();

        // The operator's pre-existing rule is untouched, above the block.
        let begin_pos = contents.find(GITIGNORE_BEGIN_MARKER).unwrap();
        assert!(contents.find("# harness worktree isolation").unwrap() < begin_pos);
        assert!(contents.find(".claude/worktrees/").unwrap() < begin_pos);

        // The managed block carries the pattern exactly once.
        let end_pos = contents.find(GITIGNORE_END_MARKER).unwrap();
        let block = &contents[begin_pos..end_pos];
        assert_eq!(block.matches(".claude/worktrees/").count(), 1);

        // Idempotent: a second run is a byte-identical no-op (no growth per run).
        update_gitignore(tmp.path()).unwrap();
        let again = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(contents, again, "second refresh must be a byte-identical no-op");
    }

    #[test]
    fn orphan_end_marker_converges_without_growth() {
        // Regression for #3592: a stray END marker above user content (a
        // corrupted / hand-edited .gitignore) previously drove unbounded marker
        // growth (+1 block per run) because `position()` found the orphan END,
        // `begin > end`, and the legacy arm re-appended a fresh block forever.
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        fs::write(&gitignore, "# <<< loom-managed <<<\nnode_modules/\ndist/\n").unwrap();

        update_gitignore(tmp.path()).unwrap();
        let after_one = fs::read_to_string(&gitignore).unwrap();

        // Converges to exactly one well-formed marked block.
        assert_eq!(after_one.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(after_one.matches(GITIGNORE_END_MARKER).count(), 1);
        // User content survives.
        assert!(after_one.contains("node_modules/"));
        assert!(after_one.contains("dist/"));

        // A second run is a byte-identical no-op (no growth).
        update_gitignore(tmp.path()).unwrap();
        let after_two = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(after_one, after_two, "orphan-END migration must converge, not grow");
        assert_eq!(after_two.matches(GITIGNORE_END_MARKER).count(), 1);
    }

    #[test]
    fn orphan_begin_marker_converges_without_eating_user_content() {
        // Regression for #3592: a stray BEGIN marker above user content
        // previously converged only by having the in-place splice swallow every
        // line from the orphan BEGIN through the real END, silently deleting the
        // intervening user line (e.g. `dist/`).
        let tmp = TempDir::new().unwrap();
        let gitignore = tmp.path().join(".gitignore");

        fs::write(&gitignore, "# >>> loom-managed (do not edit) >>>\nnode_modules/\ndist/\n")
            .unwrap();

        update_gitignore(tmp.path()).unwrap();
        let after_one = fs::read_to_string(&gitignore).unwrap();

        // Converges to exactly one well-formed marked block.
        assert_eq!(after_one.matches(GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(after_one.matches(GITIGNORE_END_MARKER).count(), 1);
        // The user line between the orphan marker and EOF must survive.
        assert!(after_one.contains("dist/"), "user content must not be eaten");
        assert!(after_one.contains("node_modules/"));

        // A second run is a byte-identical no-op.
        update_gitignore(tmp.path()).unwrap();
        let after_two = fs::read_to_string(&gitignore).unwrap();
        assert_eq!(after_one, after_two, "orphan-BEGIN migration must be idempotent");
    }
}
