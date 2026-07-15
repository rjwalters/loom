//! Content-gated cleanup of retired Loom files.
//!
//! Some files Loom once shipped were later RETIRED outright (deleted upstream,
//! not renamed). The daemon `init` merge/update path in [`super`] is
//! source-driven — it iterates `defaults/` files and syncs them into the
//! workspace — so a destination-only stray (a file no longer present in
//! `defaults/`) is never a candidate for removal. Only the destructive
//! `clean_managed_dir` removes dest-only files, and the `.claude` merge path
//! deliberately never calls it (it would wipe consumer-authored commands).
//!
//! The canonical example is `.claude/commands/loom/release.md`: the shipped
//! `/loom:release` skill was retired by #3563 / PR #3571, but existing consumer
//! copies were deliberately not swept. This module removes such strays narrowly
//! and safely, mirroring the shell-side block landed in
//! `scripts/install-loom.sh` (PR #3575, the `LOOM_RETIRED_FILES` allowlist).
//!
//! # Gate
//!
//! A frozen allowlist of `(retired relative path, sha256)` rows. A file is
//! removed ONLY when it exists AND its content hash matches a shipped digest —
//! i.e. it is byte-identical to something Loom shipped and therefore
//! unmodified. A consumer who customized the file (hash matches none) is left
//! in place. Absent → no-op. Idempotent by construction (once removed, the
//! existence check is false on every subsequent run).
//!
//! The empty-file sha256 (`e3b0c442...`) is deliberately NOT in the allowlist:
//! a zero-byte `release.md` is treated as consumer state and preserved.
//!
//! # Drift guard
//!
//! [`RETIRED_FILES`] is the Rust source of truth. A `#[cfg(test)]` drift guard
//! (see the `tests` module) parses the shell `LOOM_RETIRED_FILES` heredoc out
//! of `scripts/install-loom.sh` and asserts set-equality with [`RETIRED_FILES`]
//! in both directions, so neither surface can change without the other.

use std::fs;
use std::path::Path;

use crate::init::InitReport;

/// Frozen retired-file allowlist: `(relative path, sha256 hex digest)`.
///
/// One row per shipped version; multiple rows per path enumerate every version
/// Loom ever shipped at that path. Append-only — never remove a historical
/// digest. This is the Rust mirror of the `LOOM_RETIRED_FILES` heredoc in
/// `scripts/install-loom.sh`; the two are kept identical by a drift-guard test.
///
/// `.claude/commands/loom/release.md` — every version shipped in the
/// #3495 → #3563 window (retired by #3563 / PR #3571). release.md's only
/// placeholder is the Claude-Code *runtime* token `{{workspace}}`, which is NOT
/// install-time substituted, so shipped bytes are identical across consumers
/// and the content-hash gate is exact.
pub const RETIRED_FILES: &[(&str, &str)] = &[
    (
        ".claude/commands/loom/release.md",
        "11aef217942f45bd03d90a24e5efae9209041cb59f09c888df4dc7e8208910dd",
    ),
    (
        ".claude/commands/loom/release.md",
        "0df6c20846c98850413243362c80dea2fd01330c8d97033ef5f7c3989578fe8c",
    ),
    (
        ".claude/commands/loom/release.md",
        "c45841f8da42d1bda20bc180c8a93d14242238d9a2c1d9f5a1bdac32b5e9e556",
    ),
    (
        ".claude/commands/loom/release.md",
        "d91e198e977ad7799f44fa1a6827c9836bca6d31c9357ed92fc400a3c88381de",
    ),
    (
        ".claude/commands/loom/release.md",
        "0d7030dd14f32f6f382a6430cd04e5f0475825d567aaed7570b73a4c43128ad1",
    ),
    (
        ".claude/commands/loom/release.md",
        "4a077ed25cb44add0afbc4d6bda23cb372f5f3c4c2ef23b7a24b586e66e4f3e7",
    ),
    (
        ".claude/commands/loom/release.md",
        "5f9930dc72a263866122b18018a64b8fed4bd53ef623d0eef27ed1e31fa0502f",
    ),
    (
        ".claude/commands/loom/release.md",
        "b7fae9d13d2bfaee3bde514cabe44ac70b6551351a9e49357ede00f82c17cf35",
    ),
    (
        ".claude/commands/loom/release.md",
        "f6523d9be058e40397f0ce30c08a8f2b60e9b38adae04bd7c919e0cc840acfec",
    ),
    (
        ".claude/commands/loom/release.md",
        "29a845f7f8912545d23832551753304df6e72dd4a9c8082c2d8ada1f09f449e1",
    ),
    (
        ".claude/commands/loom/release.md",
        "795c1df1d3f3706ba448482b037a0c9e4eb6272a719adb2688b9ddfc91ab4de6",
    ),
];

/// Compute the lowercase-hex sha256 of `bytes`.
///
/// Mirrors the `Sha256` usage pattern at `loom-daemon/src/terminal.rs:76`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Remove retired Loom strays whose on-disk content matches a shipped digest.
///
/// For each unique retired path in [`RETIRED_FILES`]:
/// - present AND its sha256 ∈ the allowlist → `fs::remove_file`, record in
///   `report.removed`;
/// - present but hash matches none → preserve (record in `report.preserved`);
/// - absent → no-op.
///
/// Idempotent. Errors reading/removing an individual file are swallowed (best
/// effort, matching the shell block's tolerance of already-removed/untracked
/// strays) — a cleanup failure must never abort `init`.
///
/// This is called from `initialize_workspace` AFTER scaffolding and BEFORE
/// `generate_manifest`, and only on the non-self-install path (the self-install
/// short-circuit returns early, so this never mutates the Loom source tree).
pub fn cleanup_retired_files(workspace_path: &Path, report: &mut InitReport) {
    cleanup_with_allowlist(workspace_path, report, RETIRED_FILES);
}

/// Core logic, parameterized on the allowlist for testability. The public
/// [`cleanup_retired_files`] delegates here with the frozen [`RETIRED_FILES`].
fn cleanup_with_allowlist(
    workspace_path: &Path,
    report: &mut InitReport,
    allowlist: &[(&str, &str)],
) {
    // Unique retired paths — a path may have several allowed digests.
    let mut seen: Vec<&str> = Vec::new();
    for (path, _hash) in allowlist {
        if !seen.contains(path) {
            seen.push(path);
        }
    }

    for rel_path in seen {
        let target = workspace_path.join(rel_path);
        if !target.is_file() {
            continue; // absent → no-op (idempotent)
        }
        let bytes = match fs::read(&target) {
            Ok(b) => b,
            Err(_) => continue, // unreadable → leave in place
        };
        let file_hash = sha256_hex(&bytes);
        let matched = allowlist
            .iter()
            .any(|(p, h)| *p == rel_path && *h == file_hash);
        if matched {
            // Byte-identical to a shipped version → safe to remove.
            if fs::remove_file(&target).is_ok() {
                report.removed.push(rel_path.to_string());
            }
        } else {
            // Present but content matches no shipped version → consumer-customized.
            report.preserved.push(rel_path.to_string());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    const RETIRED_REL: &str = ".claude/commands/loom/release.md";

    /// Write `content` at `RETIRED_REL` under `workspace`, creating parents.
    fn write_release_md(workspace: &Path, content: &[u8]) {
        let target = workspace.join(RETIRED_REL);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, content).unwrap();
    }

    #[test]
    fn test_sha256_hex_matches_known_vectors() {
        // sha256("") == e3b0c442... (the empty-file digest, deliberately excluded).
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // sha256("abc") == ba7816bf...
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_empty_file_digest_excluded_from_allowlist() {
        let empty = sha256_hex(b"");
        assert!(
            !RETIRED_FILES.iter().any(|(_, h)| *h == empty),
            "empty-file sha256 must NOT be in the allowlist (a zero-byte file is consumer state)"
        );
    }

    #[test]
    fn test_remove_on_match() {
        // We cannot reconstruct release.md's original bytes, so drive the real
        // decision logic (`cleanup_with_allowlist`) with a synthetic allowlist
        // whose digest equals the sha256 of content we control.
        let temp = TempDir::new().unwrap();
        let workspace = temp.path();
        let content = b"a specific retired-file body\n";
        write_release_md(workspace, content);
        let digest = sha256_hex(content);
        let allow: &[(&str, &str)] = &[(RETIRED_REL, digest.as_str())];

        let mut report = InitReport::default();
        cleanup_with_allowlist(workspace, &mut report, allow);

        assert!(
            !workspace.join(RETIRED_REL).exists(),
            "content byte-identical to a shipped digest must be removed"
        );
        assert_eq!(report.removed, vec![RETIRED_REL.to_string()]);
        assert!(report.preserved.is_empty());
    }

    #[test]
    fn test_preserve_modified() {
        // Content matching no allowlist digest is a consumer customization and
        // must be preserved. Uses the real const via `cleanup_retired_files`.
        let temp = TempDir::new().unwrap();
        let workspace = temp.path();
        write_release_md(workspace, b"arbitrary consumer content\n");

        let mut report = InitReport::default();
        cleanup_retired_files(workspace, &mut report);

        assert!(workspace.join(RETIRED_REL).exists(), "unmatched content must be preserved");
        assert_eq!(report.preserved, vec![RETIRED_REL.to_string()]);
        assert!(report.removed.is_empty());
    }

    #[test]
    fn test_absent_is_noop() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path();
        let mut report = InitReport::default();
        cleanup_retired_files(workspace, &mut report);
        assert!(report.removed.is_empty());
        assert!(report.preserved.is_empty());
    }

    #[test]
    fn test_idempotent_across_runs() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path();
        let content = b"a specific retired-file body\n";
        write_release_md(workspace, content);
        let digest = sha256_hex(content);
        let allow: &[(&str, &str)] = &[(RETIRED_REL, digest.as_str())];

        // First run removes the matched file.
        let mut report = InitReport::default();
        cleanup_with_allowlist(workspace, &mut report, allow);
        assert_eq!(report.removed, vec![RETIRED_REL.to_string()]);
        assert!(!workspace.join(RETIRED_REL).exists());

        // Subsequent runs are stable no-ops (existence check is now false).
        for _ in 0..3 {
            let mut report = InitReport::default();
            cleanup_with_allowlist(workspace, &mut report, allow);
            assert!(report.removed.is_empty());
            assert!(report.preserved.is_empty());
        }
    }

    /// Parse the shell `LOOM_RETIRED_FILES` heredoc out of
    /// `scripts/install-loom.sh` into a `(path, digest)` set.
    fn parse_shell_allowlist() -> HashSet<(String, String)> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR"); // .../loom-daemon
        let repo_root = Path::new(manifest_dir).parent().unwrap();
        let script = repo_root.join("scripts").join("install-loom.sh");
        let text = fs::read_to_string(&script)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", script.display()));

        // Extract the heredoc body between `<<'RETIRED'` and the closing
        // `RETIRED` delimiter, then keep non-comment rows with >= 2 fields.
        let mut in_heredoc = false;
        let mut set = HashSet::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if !in_heredoc {
                if trimmed.contains("<<'RETIRED'") {
                    in_heredoc = true;
                }
                continue;
            }
            if trimmed == "RETIRED" {
                break; // closing delimiter
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut fields = trimmed.split_whitespace();
            if let (Some(path), Some(hash)) = (fields.next(), fields.next()) {
                set.insert((path.to_string(), hash.to_string()));
            }
        }
        set
    }

    /// DRIFT GUARD (issue #3576, curator Option B): the Rust [`RETIRED_FILES`]
    /// const and the shell `LOOM_RETIRED_FILES` heredoc must be set-equal in
    /// both directions. Fails if either side changes without the other.
    #[test]
    fn test_rust_and_shell_allowlists_in_sync() {
        let rust: HashSet<(String, String)> = RETIRED_FILES
            .iter()
            .map(|(p, h)| (p.to_string(), h.to_string()))
            .collect();
        let shell = parse_shell_allowlist();

        assert!(
            !shell.is_empty(),
            "parsed shell allowlist is empty — the LOOM_RETIRED_FILES heredoc \
             parser in this test is broken or the shell block moved"
        );

        let only_rust: Vec<_> = rust.difference(&shell).collect();
        let only_shell: Vec<_> = shell.difference(&rust).collect();
        assert!(
            only_rust.is_empty() && only_shell.is_empty(),
            "retired-file allowlist DRIFT between Rust const and \
             scripts/install-loom.sh.\n  Only in Rust: {only_rust:?}\n  Only in shell: {only_shell:?}\n\
             Update both surfaces together (append-only)."
        );
        // Cardinality sanity: both sides carry the frozen 11 release.md digests.
        assert_eq!(rust.len(), 11, "expected 11 frozen digests in the Rust const");
        assert_eq!(shell.len(), 11, "expected 11 frozen digests in the shell heredoc");
    }
}
