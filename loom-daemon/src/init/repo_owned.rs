//! Ownership boundary for the managed-directory clean sweep (issue #5971).
//!
//! On reinstall, `sync_managed_dir` cleans each managed `.loom/` directory
//! before re-copying from `defaults/`. That sweep used to delete **every**
//! destination-only file it found, with no ownership check — so a repo-owned
//! file living inside a Loom-managed directory was silently destroyed. The
//! reported incident: a consumer repo's own `.loom/hooks/post-worktree.sh`
//! (a documented extension point Loom itself invokes from `worktree.sh`)
//! disappeared on an `install.sh --quick --yes --confirm-reinstall` upgrade,
//! so the hook simply stopped firing.
//!
//! This module answers one question for a single destination path: **is Loom
//! entitled to delete it?** Three signals, checked in this order by
//! [`OwnershipBoundary::classify`]:
//!
//! 1. **Loom ships it right now** (the caller passes `shipped_now`, derived
//!    from the corresponding `defaults/` source tree). Removable — the sweep
//!    deletes it and the copy step immediately re-writes it, so the net
//!    effect on disk is unchanged from before this module existed.
//! 2. **The repo declared it repo-owned** by listing its `.loom/`-relative
//!    path in `.loom/resync-ignore`. Never removable. This reuses the
//!    existing, already-documented pin convention that
//!    `resync-installed.sh` honors ("never overwrite this file"), extended
//!    to also mean "never delete this file".
//! 3. **A previous install recorded it** in `.loom/install-metadata.json`'s
//!    `installed_files`. Removable — Loom wrote it, so Loom may retire it.
//!
//! Anything else has **no ownership evidence at all** and is preserved
//! (and reported), because the failure modes are not symmetric: a
//! wrongly-kept stale Loom file is cosmetic drift that the manifest-driven
//! sweeps in `scripts/install-loom.sh` / `scripts/uninstall-loom.sh` still
//! clean up, whereas a wrongly-deleted repo file is unrecoverable data loss.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// What the clean sweep is allowed to do with one destination file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// Loom owns this path — the sweep may delete it.
    Loom,
    /// The repo declared this path repo-owned in `.loom/resync-ignore`.
    DeclaredRepoOwned,
    /// No ownership evidence either way — preserve conservatively.
    Unknown,
}

/// Ownership evidence gathered from a workspace, built once per init run.
#[derive(Debug, Default)]
pub struct OwnershipBoundary {
    /// `.loom/`-relative paths pinned in `.loom/resync-ignore`
    /// (e.g. `hooks/post-worktree.sh`).
    pinned: HashSet<String>,
    /// Repo-relative paths from `.loom/install-metadata.json`'s
    /// `installed_files` (e.g. `.loom/scripts/worktree.sh`).
    installed: HashSet<String>,
}

impl OwnershipBoundary {
    /// Read `.loom/resync-ignore` and `.loom/install-metadata.json` from
    /// `workspace`. Missing or unparseable files simply contribute no
    /// evidence — this never fails, and never blocks an install.
    pub fn load(workspace: &Path) -> Self {
        let loom = workspace.join(".loom");
        Self {
            pinned: parse_resync_ignore(&loom.join("resync-ignore")),
            installed: parse_installed_files(&loom.join("install-metadata.json")),
        }
    }

    /// Classify one repo-relative destination path (e.g.
    /// `.loom/hooks/post-worktree.sh`).
    ///
    /// `shipped_now` is `true` when the current `defaults/` tree ships a file
    /// at the corresponding source path; such a file is Loom's regardless of
    /// any pin, because the copy step re-writes it moments later either way
    /// (a pin on a shipped path is a *resync* concern, handled by
    /// `resync-installed.sh`, not a deletion concern).
    pub fn classify(&self, rel_path: &str, shipped_now: bool) -> Ownership {
        if shipped_now {
            return Ownership::Loom;
        }
        if self.is_declared_repo_owned(rel_path) {
            return Ownership::DeclaredRepoOwned;
        }
        if self.installed.contains(rel_path) {
            return Ownership::Loom;
        }
        Ownership::Unknown
    }

    /// True when `rel_path` (repo-relative, e.g. `.loom/hooks/foo.sh`) is
    /// pinned in `.loom/resync-ignore`.
    pub fn is_declared_repo_owned(&self, rel_path: &str) -> bool {
        let key = rel_path.strip_prefix(".loom/").unwrap_or(rel_path);
        self.pinned.contains(key)
    }
}

/// Parse `.loom/resync-ignore`: one `.loom/`-relative path per line, `#`
/// comments and blank lines ignored. Mirrors `is_ignored()` in
/// `defaults/scripts/resync-installed.sh` (exact match, no globbing) so the
/// two readers can never disagree about what a line means. A `.loom/` prefix
/// is tolerated and stripped, since that is the form operators see in
/// installer output.
fn parse_resync_ignore(path: &Path) -> HashSet<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    contents
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.strip_prefix("./")
                .unwrap_or(line)
                .strip_prefix(".loom/")
                .unwrap_or(line)
                .to_string()
        })
        .collect()
}

/// Parse `installed_files` out of `.loom/install-metadata.json`.
///
/// An absent file, unreadable JSON, or an **empty** array all yield an empty
/// set — "no record", not "Loom owns nothing". `write_install_metadata`
/// writes a stub with an empty `installed_files` (the shell installer later
/// overwrites it with the real list), so an empty array genuinely carries no
/// information and must not be read as an ownership claim.
fn parse_installed_files(path: &Path) -> HashSet<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return HashSet::new();
    };
    value
        .get("installed_files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workspace_with(resync_ignore: Option<&str>, metadata: Option<&str>) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let loom = tmp.path().join(".loom");
        fs::create_dir_all(&loom).unwrap();
        if let Some(body) = resync_ignore {
            fs::write(loom.join("resync-ignore"), body).unwrap();
        }
        if let Some(body) = metadata {
            fs::write(loom.join("install-metadata.json"), body).unwrap();
        }
        tmp
    }

    #[test]
    fn missing_files_yield_no_evidence() {
        let tmp = TempDir::new().unwrap();
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/hooks/post-worktree.sh", false), Ownership::Unknown);
    }

    #[test]
    fn resync_ignore_pin_declares_repo_ownership() {
        let tmp = workspace_with(Some("# a repo-owned hook\nhooks/post-worktree.sh\n\n"), None);
        let boundary = OwnershipBoundary::load(tmp.path());
        assert!(boundary.is_declared_repo_owned(".loom/hooks/post-worktree.sh"));
        assert_eq!(
            boundary.classify(".loom/hooks/post-worktree.sh", false),
            Ownership::DeclaredRepoOwned
        );
        // A different path in the same directory is unaffected.
        assert_eq!(boundary.classify(".loom/hooks/other.sh", false), Ownership::Unknown);
    }

    #[test]
    fn resync_ignore_tolerates_a_loom_prefix() {
        let tmp = workspace_with(Some(".loom/hooks/post-worktree.sh\n"), None);
        let boundary = OwnershipBoundary::load(tmp.path());
        assert!(boundary.is_declared_repo_owned(".loom/hooks/post-worktree.sh"));
    }

    #[test]
    fn installed_files_record_makes_a_path_loom_owned() {
        let tmp =
            workspace_with(None, Some(r#"{"installed_files": [".loom/scripts/retired.sh"]}"#));
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/scripts/retired.sh", false), Ownership::Loom);
        assert_eq!(
            boundary.classify(".loom/scripts/never-installed.sh", false),
            Ownership::Unknown
        );
    }

    #[test]
    fn empty_installed_files_is_no_record_not_an_ownership_claim() {
        let tmp = workspace_with(None, Some(r#"{"installed_files": []}"#));
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/scripts/anything.sh", false), Ownership::Unknown);
    }

    #[test]
    fn malformed_metadata_is_treated_as_no_record() {
        let tmp = workspace_with(None, Some("{not json"));
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/scripts/anything.sh", false), Ownership::Unknown);
    }

    #[test]
    fn a_currently_shipped_path_is_always_loom_owned() {
        // Even a pinned path: the copy step rewrites it moments later, so the
        // pin cannot protect it from the clean-then-copy cycle and pretending
        // otherwise would only make the report lie.
        let tmp = workspace_with(Some("hooks/guard-destructive.sh\n"), None);
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/hooks/guard-destructive.sh", true), Ownership::Loom);
    }

    // ---- Rust/shell `resync-ignore` parser parity (issue #6161, AC #3) ----
    //
    // `parse_resync_ignore()`'s own doc comment claims it "mirrors `is_ignored()`
    // in `defaults/scripts/resync-installed.sh` ... so the two readers can never
    // disagree". This is a DRIFT GUARD in the same spirit as
    // `init::retired::tests::test_rust_and_shell_allowlists_in_sync` (which
    // cross-checks a different Rust/shell pair) — except `is_ignored()` is a
    // *function*, not static data, so instead of parsing it into a comparable
    // set this test runs the REAL shell function (extracted verbatim from the
    // production script, not reimplemented) against the same fixture and
    // compares its exit status to the Rust classifier's answer.

    use std::path::PathBuf;
    use std::process::Command;

    /// Absolute path to the real `defaults/scripts/resync-installed.sh` in
    /// this checkout, or `None` when this is not a source checkout (e.g. a
    /// vendored copy of just the `loom-daemon` crate with no sibling
    /// `defaults/` tree) — mirrors `build_slot::tests::bash_helper()`.
    fn resync_installed_sh() -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .join("defaults/scripts/resync-installed.sh");
        p.is_file().then_some(p)
    }

    /// Run the ACTUAL `is_ignored()` function body extracted out of
    /// `resync-installed.sh` (never a reimplementation) against `ignore_file`,
    /// checking `rel`. Returns `true` when the shell function reports the
    /// path ignored (its exit status 0).
    fn shell_is_ignored(script: &Path, ignore_file: &Path, rel: &str) -> bool {
        // `is_ignored()` reads the global `$IGNORE_FILE`, so this driver sets
        // it directly rather than reconstructing resync-installed.sh's own
        // `WRITE_ROOT`-derived computation of that path — the function under
        // test never notices the difference.
        let driver = r#"
            set -euo pipefail
            SCRIPT="$1"; IGNORE_FILE="$2"; REL="$3"
            # Extract only the is_ignored() function body from the real
            # script (first line matching its signature through the next
            # line that is exactly a closing brace) and source it — this
            # exercises production code, not a copy of it.
            FN="$(sed -n '/^is_ignored() {/,/^}/p' "$SCRIPT")"
            if [[ -z "$FN" ]]; then
                echo "is_ignored() not found in $SCRIPT" >&2
                exit 2
            fi
            eval "$FN"
            is_ignored "$REL"
        "#;
        let status = Command::new("bash")
            .arg("-c")
            .arg(driver)
            .arg("bash") // $0
            .arg(script)
            .arg(ignore_file)
            .arg(rel)
            .status()
            .expect("run the extracted shell is_ignored()");
        match status.code() {
            Some(0) => true,
            Some(1) => false,
            other => panic!("shell_is_ignored driver failed unexpectedly: {other:?}"),
        }
    }

    fn rust_is_ignored(resync_ignore_contents: &str, rel: &str) -> bool {
        let tmp = workspace_with(Some(resync_ignore_contents), None);
        let boundary = OwnershipBoundary::load(tmp.path());
        boundary.is_declared_repo_owned(rel)
    }

    /// One fixture case: a `.loom/resync-ignore` body, a checked path, and
    /// whether the two parsers are expected to currently AGREE it is ignored.
    struct Case {
        label: &'static str,
        resync_ignore: &'static str,
        rel: &'static str,
        expect_ignored: bool,
    }

    /// Cases both parsers are expected to agree on today: exact match with no
    /// globbing, `#`-comment stripping, leading/trailing whitespace trimming,
    /// blank lines skipped, and a non-matching path never ignored. These use
    /// the "natural" un-prefixed label form that `resync-installed.sh` itself
    /// passes as `rel` for the hooks/scripts/roles/docs/bin surfaces (see
    /// `sync_one` call sites), which is also the form
    /// `is_declared_repo_owned`'s doc comment documents.
    fn agreement_cases() -> Vec<Case> {
        vec![
            Case {
                label: "exact match, single entry",
                resync_ignore: "hooks/post-worktree.sh\n",
                rel: "hooks/post-worktree.sh",
                expect_ignored: true,
            },
            Case {
                label: "no match against an unrelated path",
                resync_ignore: "hooks/post-worktree.sh\n",
                rel: "hooks/other.sh",
                expect_ignored: false,
            },
            Case {
                label: "comment-only line contributes no pin",
                resync_ignore: "# hooks/post-worktree.sh\n",
                rel: "hooks/post-worktree.sh",
                expect_ignored: false,
            },
            Case {
                label: "trailing comment stripped from a real pin",
                resync_ignore: "hooks/post-worktree.sh  # local override\n",
                rel: "hooks/post-worktree.sh",
                expect_ignored: true,
            },
            Case {
                label: "leading/trailing whitespace trimmed",
                resync_ignore: "   hooks/post-worktree.sh   \n",
                rel: "hooks/post-worktree.sh",
                expect_ignored: true,
            },
            Case {
                label: "blank lines are skipped, later pin still matches",
                resync_ignore: "\n\n  \nscripts/claude-wrapper.sh\n",
                rel: "scripts/claude-wrapper.sh",
                expect_ignored: true,
            },
            Case {
                label: "no globbing: a wildcard pin does not match a similarly-named path",
                resync_ignore: "hooks/*.sh\n",
                rel: "hooks/post-worktree.sh",
                expect_ignored: false,
            },
            Case {
                label: "empty ignore file has no pins",
                resync_ignore: "",
                rel: "hooks/post-worktree.sh",
                expect_ignored: false,
            },
            Case {
                label: "a same-named pin in a different directory does not match",
                resync_ignore: "scripts/post-worktree.sh\n",
                rel: "hooks/post-worktree.sh",
                expect_ignored: false,
            },
            Case {
                // `.loom/CLAUDE.md` and `.loom/biome.jsonc` are the two
                // surfaces `resync-installed.sh` itself passes to
                // `is_ignored()` WITH the `.loom/` prefix already baked into
                // `rel` (see the `sync_one` call sites at the `.loom/CLAUDE.md`
                // restamp and the `.loom/biome.jsonc` single-file sync) — so a
                // pin written with that same prefix is the documented,
                // already-agreeing case, not the drifted one below.
                label:
                    "a prefixed pin matches the two shell call sites that also pass a prefixed rel",
                resync_ignore: ".loom/CLAUDE.md\n",
                rel: ".loom/CLAUDE.md",
                expect_ignored: true,
            },
        ]
    }

    /// A KNOWN, currently-tracked divergence (issue #6515, fix pending in PR
    /// #6532, unmerged as of this test's authorship): a pin written in the
    /// repo-relative form (`.loom/hooks/post-worktree.sh`, the form
    /// `OwnershipBoundary`'s own doc comment shows as the example) is honored
    /// by the Rust side (`is_declared_repo_owned` strips a leading `.loom/`
    /// from BOTH the pin and the checked path before comparing) but is
    /// silently a no-op on the shell side today (`is_ignored()` does a bare
    /// `[[ "$line" == "$rel" ]]`, no normalization) when `rel` itself is the
    /// un-prefixed internal label `resync-installed.sh` uses for the
    /// hooks/scripts/roles/docs/bin surfaces. This is intentionally asserted
    /// as a MISMATCH, not papered over — once #6515/#6532 lands this
    /// assertion will start failing (loudly, not silently) and should be
    /// moved into `agreement_cases()` above.
    #[test]
    fn known_divergence_prefixed_pin_vs_unprefixed_rel_tracked_by_6515() {
        let Some(script) = resync_installed_sh() else {
            return; // not a source checkout; nothing to cross-check
        };
        let resync_ignore = ".loom/hooks/post-worktree.sh\n";
        let rel = "hooks/post-worktree.sh";

        let rust = rust_is_ignored(resync_ignore, rel);
        let tmp = workspace_with(Some(resync_ignore), None);
        let shell = shell_is_ignored(&script, &tmp.path().join(".loom/resync-ignore"), rel);

        assert!(rust, "Rust is_declared_repo_owned should honor the .loom/-prefixed pin form");
        assert!(
            !shell,
            "shell is_ignored() is expected to STILL miss the prefixed pin form as of \
             #6515/#6532 being unmerged — if this now fails, #6532 (or an equivalent fix) \
             has landed: promote this case into agreement_cases() and delete this test"
        );
    }

    #[test]
    fn rust_and_shell_resync_ignore_parsers_agree_on_shared_fixture() {
        let Some(script) = resync_installed_sh() else {
            return; // not a source checkout; nothing to cross-check
        };

        let mut mismatches = Vec::new();
        for case in agreement_cases() {
            let rust = rust_is_ignored(case.resync_ignore, case.rel);
            let tmp = workspace_with(Some(case.resync_ignore), None);
            let shell =
                shell_is_ignored(&script, &tmp.path().join(".loom/resync-ignore"), case.rel);

            if rust != case.expect_ignored || shell != case.expect_ignored || rust != shell {
                mismatches.push(format!(
                    "[{}] rel={:?} resync_ignore={:?} -> expected={} rust={} shell={}",
                    case.label, case.rel, case.resync_ignore, case.expect_ignored, rust, shell
                ));
            }
        }

        assert!(
            mismatches.is_empty(),
            "Rust/shell resync-ignore parser disagreement (or a fixture that doesn't match \
             the documented exact-match, no-globbing semantics):\n{}",
            mismatches.join("\n")
        );
    }
}
