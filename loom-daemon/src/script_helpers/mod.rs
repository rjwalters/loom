//! Shared plumbing for the natively-ported script helpers (epic #4081 Phase 3,
//! family 5 — issue #4275).
//!
//! Family 5 replaced eight `loom_tools` Python modules (`log_filter`,
//! `model_tiers`, `common/usage`, `validate_phase`, `checkpoints`, `claim`,
//! `sweep_experiment`, `backlog`) with native `loom-daemon` subcommands. Those
//! modules all leaned on the same two pieces of `loom_tools.common` plumbing —
//! repo-root discovery and the `gh`/`gh-cached` runner — so both live here once
//! rather than being copied into each port.
//!
//! Everything in this module is **soft-fail by construction**: the Python
//! originals were explicitly documented never to raise (a config read must not
//! be able to block a sweep dispatch), so the Rust equivalents return `Option`
//! / empty values instead of propagating errors.
//!
//! | Submodule | Shell / CLI entry point | Replaces |
//! |---|---|---|
//! | [`log_filter`] | `strip-ansi.sh` | `loom_tools.log_filter` |
//! | [`model_tiers`] | `resolve-model.sh`, `resolve-tier-model.sh` | `loom_tools.model_tiers` |
//! | [`usage`] | `check-usage.sh` | `loom_tools.common.usage` |
//! | [`checkpoints`] | `checkpoint.sh` | `loom_tools.checkpoints` |
//! | [`claim`] | `loom-claim` (PATH shim → `loom-daemon claim`) | `loom_tools.claim` |
//! | [`sweep_experiment`] | `sweep-experiment.sh` | `loom_tools.sweep_experiment` |
//! | [`validate_phase`] | `validate-phase.sh` | `loom_tools.validate_phase` |
//!
//! `loom_tools.backlog` had no shell caller and no test file — only historical
//! doc references — so it was deleted outright rather than ported.
//!
//! Every entry point keeps its historical name, flags, stdout shape and exit
//! codes so a zero-pip consumer workspace behaves identically (the epic
//! "callers switch in the same PR; script names/flags persist" contract).

pub mod checkpoints;
pub mod claim;
pub mod log_filter;
pub mod model_tiers;
pub mod sweep_experiment;
pub mod transcript_usage;
pub mod usage;
pub mod validate_phase;

use crate::cmd_out::{decode_json, run_command, CmdOutcome, Query, DEFAULT_TIMEOUT};

/// Ceiling for the `gh-cached --version` capability probe.
///
/// Deliberately short: the probe delegates straight to `gh --version` with no
/// API call, so anything slow here is already the breakage it is testing for.
const GH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use std::path::{Path, PathBuf};
use std::process::Command;

/// Repo-root discovery, re-exported from the single-source
/// [`crate::repo_root`] helper (issue #5140 — this module used to carry its own
/// copy of the walk, one of three that had drifted apart).
///
/// Semantics are unchanged from the `loom_tools.common.repo.find_repo_root`
/// port that lived here: `.git` + `.loom/` required, worktree gitlinks resolved
/// to the main checkout, `None` (not a panic) outside any Loom repository —
/// which is what keeps `resolve-model.sh` working outside a Loom repo (issue
/// #4060 contract).
pub use crate::repo_root::{find_repo_root, find_repo_root_from_cwd};

/// The current UTC instant in the `%Y-%m-%dT%H:%M:%SZ` shape every Loom
/// on-disk record uses (claims, checkpoints, experiment records, recovery
/// events). Second precision and a literal `Z`, matching the Python
/// `datetime.now(timezone.utc).strftime(...)` calls this replaces — the claim
/// expiry check compares these strings *lexicographically*, so the exact width
/// is load-bearing.
#[must_use]
pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Resolve the `gh` binary to invoke: the repo's `.loom/scripts/gh-cached`
/// wrapper when it is present, executable, and answers `--version`; plain `gh`
/// otherwise.
///
/// Port of `validate_phase._gh_cmd`, including its `--version` probe: the
/// executable bit alone is not enough, because a broken Python runtime (an
/// unaccepted Xcode license, a missing interpreter) leaves an executable
/// wrapper that fails on every call. The probe is free — `gh-cached --version`
/// delegates straight to `gh --version` with no API call and no cache hit.
#[must_use]
pub fn gh_cmd(repo_root: &Path) -> PathBuf {
    let cached = repo_root.join(".loom").join("scripts").join("gh-cached");
    if is_executable_file(&cached) {
        // #7810 PR 2: bounded. The point of this probe is to catch a wrapper
        // that is executable but broken (an unaccepted Xcode licence, a missing
        // interpreter); one that HANGS is the same class of broken, and an
        // unbounded probe would wait for it forever on every single call.
        let mut probe = Command::new(&cached);
        probe.arg("--version").stdin(std::process::Stdio::null());
        let probed = run_command(probe, GH_PROBE_TIMEOUT).succeeded();
        if probed {
            return cached;
        }
    }
    PathBuf::from("gh")
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Run a `gh` (or `gh-cached`) command inside `repo_root`, under a deadline.
///
/// Epic #7810 PR 2: returns [`CmdOutcome`] instead of the retired `GhResult`.
/// That type reported a **spawn failure as `success: false`**, so "gh is not
/// installed" and "gh ran and exited 1" were the same value; it also decoded
/// lossily at the boundary and ran unbounded, so a wedged `gh` blocked forever
/// (the 2026-07-26 incident). All three are now distinguished or bounded.
pub fn run_gh(args: &[&str], repo_root: &Path, use_cache: bool) -> CmdOutcome {
    let program = if use_cache {
        gh_cmd(repo_root)
    } else {
        PathBuf::from("gh")
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null());
    run_command(cmd, DEFAULT_TIMEOUT)
}

/// Run a `gh` JSON query and decode it in-process (epic #7810 PR 2).
///
/// Callers pass `--json <fields>` and **no `--jq`**. Flattening JSON to a
/// scalar inside the subprocess is what made *command failed*, *field absent*,
/// *valid-but-empty* and *no match* the same empty string.
pub fn gh_query<T, F>(args: &[&str], repo_root: &Path, use_cache: bool, is_empty: F) -> Query<T>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(&T) -> bool,
{
    decode_json(run_gh(args, repo_root, use_cache), is_empty)
}

/// Run `git` in `dir` under a deadline, capturing output.
///
/// Output is **not** trimmed here. `main_health_gate` needed a whole separate
/// `git_status_porcelain` helper because a blanket trim ate the leading space
/// of a porcelain v1 status line (`" M file"`), changing what it meant; a
/// shared layer that trimmed by default would spread that bug rather than fix
/// it. Trim at the call site, where the format is known.
pub fn run_git(dir: &Path, args: &[&str]) -> CmdOutcome {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null());
    run_command(cmd, DEFAULT_TIMEOUT)
}

// --------------------------------------------------------------------------
// Diagnostics (port of `loom_tools.common.logging`)
// --------------------------------------------------------------------------

/// Emit one `[ts] [LABEL] message` diagnostic line on stderr.
///
/// Mirrors `loom_tools.common.logging._emit`. Colour is intentionally omitted:
/// the Python original only colourised when stderr was a tty, and every caller
/// of these helpers in the sweep/agent pipeline captures stderr to a log file.
fn emit(label: &str, message: &str) {
    eprintln!("[{}] [{}] {}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"), label, message);
}

pub fn log_info(message: &str) {
    emit("INFO", message);
}

pub fn log_warning(message: &str) {
    emit("WARN", message);
}

pub fn log_error(message: &str) {
    emit("ERROR", message);
}

pub fn log_success(message: &str) {
    emit("OK", message);
}

// --------------------------------------------------------------------------
// JSON state files (port of `loom_tools.common.state`)
// --------------------------------------------------------------------------

/// Atomic JSON write: temp file in the same directory + rename.
///
/// Mirrors `loom_tools.common.state.write_json_file` — two-space indent plus a
/// trailing newline, parent directories created on demand.
pub fn write_json_file(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(d) = dir {
        std::fs::create_dir_all(d)?;
    }
    let mut body = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    body.push('\n');

    // A pid-suffixed sibling keeps the rename atomic (same filesystem) while
    // staying collision-free across concurrent agents.
    let file_name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("state.json"), std::ffi::OsStr::to_os_string);
    let tmp_name = format!(".{}.loom-tmp-{}", file_name.to_string_lossy(), std::process::id());
    let tmp = match dir {
        Some(d) => d.join(tmp_name),
        None => PathBuf::from(tmp_name),
    };
    match std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Read and parse a JSON file, soft-failing to `None` on any error.
///
/// Mirrors `loom_tools.common.state.read_json_file`'s never-raise contract.
#[must_use]
pub fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn find_repo_root_returns_none_outside_a_loom_repo() {
        let dir = tempdir().unwrap();
        // A bare temp dir has neither `.git` nor `.loom`.
        assert!(find_repo_root(dir.path()).is_none());
    }

    #[test]
    fn find_repo_root_requires_a_loom_directory() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        // `.git` alone is not a Loom repo (matches the Python contract).
        assert!(find_repo_root(dir.path()).is_none());

        std::fs::create_dir(dir.path().join(".loom")).unwrap();
        let found = find_repo_root(dir.path()).unwrap();
        assert_eq!(found, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn find_repo_root_walks_up_from_a_nested_directory() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join(".loom")).unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_repo_root(&nested).unwrap(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn find_repo_root_follows_a_worktree_gitdir_pointer_to_the_main_root() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git").join("worktrees").join("issue-42")).unwrap();
        std::fs::create_dir(root.join(".loom")).unwrap();

        // A linked worktree: `.git` is a FILE pointing back into the main repo.
        let wt = root.join(".loom").join("worktrees").join("issue-42");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", root.join(".git/worktrees/issue-42").display()),
        )
        .unwrap();

        // Resolving from inside the worktree must land on the MAIN root, not
        // the worktree — that is what keeps `.loom/claims/` shared.
        assert_eq!(find_repo_root(&wt).unwrap(), root.canonicalize().unwrap());
    }

    #[test]
    fn now_iso_has_second_precision_and_a_literal_z() {
        let ts = now_iso();
        assert_eq!(ts.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ, got {ts}");
        assert!(ts.ends_with('Z'));
        // Lexicographic comparability is load-bearing for claim expiry.
        assert!(ts.as_str() > "2020-01-01T00:00:00Z");
    }

    #[test]
    fn gh_cmd_falls_back_to_plain_gh_without_a_wrapper() {
        let dir = tempdir().unwrap();
        assert_eq!(gh_cmd(dir.path()), PathBuf::from("gh"));
    }

    #[test]
    fn run_git_reports_failure_without_panicking() {
        let dir = tempdir().unwrap();
        let r = run_git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
        // Not a git repo: git exits non-zero but the helper still returns.
        assert!(!r.succeeded());
        // #7810 PR 2 — the distinction the old `GhResult` could not make: git
        // was found and RAN, it just answered no. That must not look the same
        // as git being absent, which is what `success: false` used to report
        // for both.
        assert!(
            matches!(r, CmdOutcome::Ran(_)),
            "git ran and exited non-zero; this is an answer, not an inability to ask"
        );
    }

    #[test]
    fn run_git_reports_a_missing_directory_as_gits_own_answer() {
        // `run_git` passes the directory as git's `-C` argument rather than as
        // the process cwd, so a missing path is something GIT rejects — it runs
        // and exits non-zero. That is an answer, and it must be reported as one.
        //
        // Worth pinning because the intuitive reading ("bad directory =>
        // couldn't run") is wrong here, and a future change to `current_dir`
        // would silently flip this outcome from `Ran` to `Unavailable` and
        // change what every caller concludes.
        let missing = std::path::Path::new("/nonexistent/loom/definitely-not-here");
        let r = run_git(missing, &["rev-parse", "HEAD"]);
        assert!(!r.succeeded());
        assert!(
            matches!(r, CmdOutcome::Ran(_)),
            "git itself rejects a bad -C path; that is an answer, got {r:?}"
        );
        assert!(!r.stderr_trimmed().is_empty(), "git should say why it refused");
    }

    #[test]
    fn write_json_file_creates_parents_and_trailing_newline() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("a").join("b").join("state.json");
        write_json_file(&target, &serde_json::json!({"k": 1})).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({"k": 1})
        );
    }

    #[test]
    fn read_json_file_soft_fails() {
        let dir = tempdir().unwrap();
        assert!(read_json_file(&dir.path().join("missing.json")).is_none());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(read_json_file(&bad).is_none());
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "   ").unwrap();
        assert!(read_json_file(&empty).is_none());
    }
}
