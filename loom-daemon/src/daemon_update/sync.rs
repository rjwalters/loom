//! `sync_with_origin` — the ff-first checkout freshness default (#4330) and
//! the ff-abort classification that decides whether an abort can name (or
//! perform) a safe resolution (#4951).
//!
//! The whole point of running the update is to get the daemon onto the LATEST
//! code, so before the staleness comparison resolves `SOURCE_COMMIT` this
//! attempts a bounded, best-effort `git fetch` and, if local HEAD is behind, a
//! `git merge --ff-only`. If that merge cannot apply the script **aborts**
//! rather than guessing or hard-resetting: a stale rebuild silently missing
//! merged commits (the 2026-07-29 incident) is worse than a loud abort.
//!
//! SAFETY NOTE, inherited verbatim from the shell: this is the
//! safety-critical branch. #4381 was a live incident where an update-script
//! code path silently overwrote a real production binary, and the same
//! "automation quietly does something destructive to real machine state" risk
//! applies to the two auto-resolutions here. Do NOT widen either classifier —
//! not a loose prefix match, not an empty-diff check that misses a rename or a
//! mode-only change. When genuinely unsure, both must answer `false` so the
//! caller falls through to the hard abort.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::args::{Args, FetchMode};
use super::out;
use super::paths;
use super::util;

/// Loom-managed installed-surface prefixes this script may discard local edits
/// to when auto-resolving.
///
/// MUST mirror `defaults/scripts/resync-installed.sh`'s own header comment
/// (search "Surfaces resynced" there) — that file, not this list, is the
/// authoritative source; update both together if it ever widens again (#4239
/// already widened it once).
const MANAGED_PREFIXES: &[&str] = &[
    ".loom/hooks/",
    ".loom/scripts/",
    ".loom/roles/",
    ".loom/docs/",
    ".loom/runtimes/",
    ".loom/bin/",
    ".claude/commands/loom/",
];

const MANAGED_FILES: &[&str] = &[".loom/install-metadata.json"];

const GITIGNORE_BEGIN: &str = "# >>> loom-managed (do not edit) >>>";
const GITIGNORE_END: &str = "# <<< loom-managed <<<";

/// What `sync_with_origin` leaves behind for the staleness echo and the final
/// "installed" line.
#[derive(Default, Debug)]
pub struct SyncState {
    /// Resolved default branch name, or empty if unresolvable.
    pub default_branch: String,
    /// Short commit of `origin/<default>` at fetch time, or `unknown`.
    pub origin_commit: String,
    /// Commits local `<default>` was behind origin BEFORE any sync.
    pub origin_behind_count: u64,
    /// True if this run fast-forwarded local HEAD.
    pub ff_synced: bool,
}

impl SyncState {
    pub fn new() -> Self {
        SyncState {
            default_branch: String::new(),
            origin_commit: "unknown".to_string(),
            origin_behind_count: 0,
            ff_synced: false,
        }
    }
}

/// `loom_default_branch origin` (`lib/default-branch.sh`) — offline-first, and
/// it HARD-FAILS rather than defaulting to `main`.
///
/// A silent wrong default reintroduces exactly the class of bug that helper
/// exists to fix, so the `None` return here must stay a real answer the caller
/// acts on, not a fallback value.
fn default_branch(repo_root: &Path) -> Option<String> {
    if let Some(explicit) = util::env_non_empty("LOOM_DEFAULT_BRANCH") {
        return Some(explicit);
    }
    if let Some(sref) =
        util::git(repo_root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
    {
        if !sref.is_empty() {
            return Some(sref.strip_prefix("origin/").unwrap_or(&sref).to_string());
        }
    }
    if let Some(lsref) = util::git(repo_root, &["ls-remote", "--symref", "origin", "HEAD"]) {
        if let Some(line) = lsref.lines().find(|l| l.starts_with("ref:")) {
            if let Some(reference) = line.split_whitespace().nth(1) {
                return Some(
                    reference
                        .strip_prefix("refs/heads/")
                        .unwrap_or(reference)
                        .to_string(),
                );
            }
        }
    }
    for candidate in ["main", "master"] {
        if util::git_ok(
            repo_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/origin/{candidate}"),
            ],
        ) {
            return Some(candidate.to_string());
        }
    }
    // DELIBERATELY SILENT. `lib/default-branch.sh`'s own `loom_default_branch`
    // prints a three-line "could not determine the default branch / Fix: run
    // git remote set-head" advisory here, and this script's single call site
    // discarded it:
    //
    //   DEFAULT_BRANCH="$(cd "$repo_root" && loom_default_branch origin 2>/dev/null)"
    //
    // Reproducing the advisory instead of the redirect put three stderr lines
    // into EVERY run against a checkout with no resolvable origin — which is
    // every `--check` and `--dry-run` in a scratch clone. Found by
    // `tests/differential_daemon_update.rs` (266 of 338 frozen cases), missed
    // by all three retained suites, because a suite that greps for the lines
    // it expects cannot see a line nobody expected.
    None
}

/// `sync_with_origin <repo_root>` — `true` to proceed, `false` to abort with
/// exit 1.
///
/// A plain `bool` rather than `Result<(), ()>`: the shell's function returned
/// a bare exit status and there is no error VALUE to carry, so a unit-error
/// `Result` would only dress the same one bit in ceremony (`clippy::
/// result_unit_err` says so too). Every refusal has already been REPORTED by
/// the time this returns — the caller's job is to exit, not to explain.
pub fn sync_with_origin(repo_root: &Path, args: &Args, state: &mut SyncState) -> bool {
    let Some(branch) = default_branch(repo_root) else {
        return true;
    };
    if branch.is_empty() {
        return true;
    }
    state.default_branch = branch.clone();

    // Bounded, best-effort fetch — a failure/timeout must NOT make this script
    // network-dependent: warn and proceed with local HEAD as-is (the behind
    // count stays unknown, not "known stale").
    let fetch_ok = if util::have("timeout") {
        Command::new("timeout")
            .arg("5")
            .arg("git")
            .arg("-C")
            .arg(repo_root)
            .args(["fetch", "origin", &branch, "--quiet"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    } else {
        util::git_ok(repo_root, &["fetch", "origin", &branch, "--quiet"])
    };
    if !fetch_ok {
        out::warn(&format!(
            "note: could not reach origin to check {branch} for updates (fetch failed or timed out) — proceeding with local HEAD as-is."
        ));
        return true;
    }

    state.origin_commit =
        util::git(repo_root, &["rev-parse", "--short", &format!("origin/{branch}")])
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());

    let n = util::git(repo_root, &["rev-list", "--count", &format!("{branch}..origin/{branch}")])
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    state.origin_behind_count = n;
    if n == 0 {
        return true;
    }

    // Read-only modes and --allow-stale never write — just advise, mirroring
    // the pre-#4330 advisory-only behavior.
    if args.check_only || args.dry_run {
        out::warn(&format!("note: local {branch} is {n} commit(s) behind origin/{branch}."));
        return true;
    }
    if args.allow_stale {
        out::warn(&format!(
            "note: local {branch} is {n} commit(s) behind origin/{branch} — building the current (stale) checkout as-is per --allow-stale."
        ));
        return true;
    }
    // --fetch never compiles anything (#7609): it REQUIRES a verified release
    // artifact, so the local checkout cannot influence the bytes that get
    // installed. Fast-forwarding here would be an unrelated side effect, and —
    // far worse — the hard-abort branches below would let purely local state
    // block an artifact roll that needs nothing from the source tree. That is
    // exactly the fleet failure #7609 exists to end, one level down.
    if args.fetch_mode == FetchMode::Force {
        out::warn(&format!(
            "note: local {branch} is {n} commit(s) behind origin/{branch} — not fast-forwarding: --fetch installs a release artifact and never builds from this checkout."
        ));
        return true;
    }

    // Only well-defined when HEAD IS the default branch — on a feature branch
    // or a detached HEAD, `git merge --ff-only origin/<default>` would merge
    // into the WRONG ref.
    let current_branch =
        util::git(repo_root, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
    if current_branch != branch {
        let shown = if current_branch.is_empty() {
            "<detached HEAD>".to_string()
        } else {
            current_branch
        };
        out::err(&format!(
            "Local {branch} is {n} commit(s) behind origin/{branch}, but the checkout HEAD is on '{shown}', not '{branch}' — refusing to guess which branch to sync."
        ));
        out::err(&format!(
            "Check out {branch} and re-run, or pass --allow-stale to build the current checkout as-is (e.g. bisecting, testing a local patch)."
        ));
        return false;
    }

    out::say(&format!(
        "Local {branch} is {n} commit(s) behind origin/{branch} — fast-forwarding before building (default; pass --allow-stale to build the current checkout as-is)..."
    ));
    if git_inherit(repo_root, &["merge", "--ff-only", &format!("origin/{branch}"), "--quiet"]) {
        out::ok(&format!("Fast-forwarded local {branch} to origin/{branch} ({n} commit(s))."));
        state.ff_synced = true;
        return true;
    }

    classify_and_report(repo_root, &branch, args, state)
}

/// The three ff-abort branches, in the shell's order: content-identical first,
/// then all-dirty-managed, then the generic hard abort.
fn classify_and_report(repo_root: &Path, branch: &str, args: &Args, state: &mut SyncState) -> bool {
    if content_identical(repo_root, branch) {
        out::warn(&format!(
            "Fast-forward merge from origin/{branch} did not apply, but local {branch} is content-IDENTICAL to origin/{branch} (git diff origin/{branch}...{branch} is empty) — local-only commit(s) that net to no change (e.g. a resync commit and its own revert)."
        ));
        if args.auto_resolve_safe_abort {
            if git_inherit(repo_root, &["reset", "--hard", &format!("origin/{branch}"), "--quiet"])
            {
                out::ok(&format!(
                    "Auto-resolved (--auto-resolve-safe-abort): reset local {branch} to origin/{branch}."
                ));
                state.ff_synced = true;
                return true;
            }
            out::err(&format!(
                "Auto-resolve (--auto-resolve-safe-abort) failed: 'git reset --hard origin/{branch}' did not succeed."
            ));
            return false;
        }
        out::err(&format!(
            "Safe to resolve: git -C \"{}\" reset --hard origin/{branch}",
            repo_root.display()
        ));
        out::err("Re-run with --auto-resolve-safe-abort to perform this automatically, or run the command above by hand.");
        return false;
    }

    if let Some(managed) = all_dirty_tracked_managed(repo_root) {
        out::warn(&format!(
            "Fast-forward merge from origin/{branch} was blocked by dirty tracked file(s), but ALL of them are Loom-managed installed copies (regenerated from defaults/ by resync-installed.sh, not real local work): {}",
            managed.join(" ")
        ));
        if args.auto_resolve_safe_abort {
            let mut checkout: Vec<&str> = vec!["checkout", "--"];
            checkout.extend(managed.iter().map(String::as_str));
            let merged = git_inherit(repo_root, &checkout)
                && git_inherit(
                    repo_root,
                    &["merge", "--ff-only", &format!("origin/{branch}"), "--quiet"],
                );
            if merged {
                out::ok(&format!(
                    "Auto-resolved (--auto-resolve-safe-abort): discarded local edits to managed file(s) and fast-forwarded to origin/{branch}."
                ));
                match paths::resolve_resync_script(repo_root) {
                    Some(script) => {
                        let ok = Command::new(&script)
                            .current_dir(repo_root)
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .is_ok_and(|s| s.success());
                        if ok {
                            out::ok("Post-roll resync-installed.sh completed.");
                        } else {
                            out::warn(&format!(
                                "Post-roll resync-installed.sh failed — run it by hand: {}",
                                script.display()
                            ));
                        }
                    }
                    None => out::warn(
                        "Could not resolve resync-installed.sh — run it by hand after this update to re-sync managed files.",
                    ),
                }
                state.ff_synced = true;
                return true;
            }
            out::err("Auto-resolve (--auto-resolve-safe-abort) failed: discarding managed edits + fast-forward did not both succeed.");
            return false;
        }
        out::err(&format!(
            "Safe to resolve: git -C \"{}\" checkout -- {} && ./.loom/scripts/resync-installed.sh",
            repo_root.display(),
            managed.join(" ")
        ));
        out::err("Re-run with --auto-resolve-safe-abort to perform this automatically, or run the commands above by hand.");
        return false;
    }

    out::err(&format!(
        "Fast-forward merge from origin/{branch} did not apply — local commits have diverged, or a dirty tracked file conflicts with the incoming change."
    ));
    out::err("Refusing to guess or hard-reset: resolve manually (rebase/merge by hand), or pass --allow-stale to build the current (stale) checkout as-is.");
    // #6008: on a fleet host the most common non-managed blocker is a
    // host-specific edit parked in a TRACKED config tier — it re-blocks every
    // roll forever, so the checkout drifts hundreds of commits behind. Name
    // the host-local tier instead of only saying "resolve manually".
    let dirty_cfg = util::git(
        repo_root,
        &[
            "diff",
            "--name-only",
            "HEAD",
            "--",
            ".loom/config.json",
            ".loom-project/project.json",
        ],
    )
    .unwrap_or_default();
    let dirty_cfg_tiers = dirty_cfg
        .lines()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !dirty_cfg_tiers.is_empty() {
        out::err(&format!("Dirty TRACKED config tier(s) present: {dirty_cfg_tiers}"));
        out::err("If that diff is host-specific — a per-host path or socket, or an on/off switch that is true of this box and false of the others (worktree.root, safehouse.enabled, safehouse.socket) — it does not belong in a tracked file at all. Move it to the gitignored .loom-local/local.json tier, which never shows up in git status and so can never block this sync again. Per-key runbook: https://github.com/rjwalters/loom/blob/main/docs/design/config-resolution-tiers.md");
    }
    let _ = state;
    false
}

/// `git -C <root> …` with stdio inherited, as the shell's bare invocations ran.
fn git_inherit(repo_root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

/// `git status --porcelain` lines, empty ones dropped.
fn porcelain(repo_root: &Path) -> Vec<String> {
    util::git(repo_root, &["status", "--porcelain"])
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// `_ff_abort_no_dirty_tracked_files` — true iff NO tracked file is dirty.
///
/// Untracked (`??`) entries are excluded: they cannot conflict with a
/// fast-forward merge, and `git reset --hard` never touches them either.
fn no_dirty_tracked_files(repo_root: &Path) -> bool {
    !porcelain(repo_root)
        .iter()
        .any(|line| &line[..2.min(line.len())] != "??")
}

/// `_ff_abort_content_identical` — local `<default>` and `origin/<default>`
/// are content-IDENTICAL despite having diverged in commit history.
///
/// Three guards, each load-bearing:
///
/// 1. **`--is-ancestor` first.** When local IS an ancestor of origin (the far
///    more common shape: origin advanced and local added nothing), the
///    three-dot diff trivially reduces to diffing local HEAD against itself
///    and is ALWAYS empty — so without this guard every plain
///    "blocked by a dirty tracked file" abort would misclassify as
///    content-identical and risk an incorrect `reset --hard`.
/// 2. **A clean working tree.** Both ref comparisons say nothing about
///    uncommitted work. A host can have diverged-but-net-zero commits AND an
///    unrelated dirty tracked file; the `reset --hard` this classifier
///    vouches for would silently discard it.
/// 3. **Plain three-dot `git diff --quiet`, no rename detection.** Matches the
///    incident's own manual check, and `--quiet` treats a mode-only change as
///    non-empty — the conservative direction.
fn content_identical(repo_root: &Path, branch: &str) -> bool {
    if util::git_ok(
        repo_root,
        &[
            "merge-base",
            "--is-ancestor",
            branch,
            &format!("origin/{branch}"),
        ],
    ) {
        return false;
    }
    if !no_dirty_tracked_files(repo_root) {
        return false;
    }
    util::git_ok(
        repo_root,
        &[
            "diff",
            "--quiet",
            &format!("origin/{branch}...{branch}"),
            "--",
        ],
    )
}

/// `_ff_abort_gitignore_only_managed_block_dirty` — the ONLY diff between the
/// working tree's `.gitignore` and HEAD's is inside the marker-delimited
/// Loom-managed block, so a consumer's own hand-edited lines outside it are
/// never silently discarded.
fn gitignore_only_managed_block_dirty(repo_root: &Path) -> bool {
    let file = repo_root.join(".gitignore");
    if !file.is_file() {
        return false;
    }
    let Ok(working) = std::fs::read_to_string(&file) else {
        return false;
    };
    let head = util::git(repo_root, &["show", "HEAD:.gitignore"]).unwrap_or_default();
    strip_managed_block(&working) == strip_managed_block(&head)
}

/// The awk filter: drop the marker lines themselves and everything between
/// them. An unterminated block swallows the rest of the file, exactly as the
/// awk did.
fn strip_managed_block(text: &str) -> Vec<String> {
    let mut skip = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line == GITIGNORE_BEGIN {
            skip = true;
            continue;
        }
        if line == GITIGNORE_END {
            skip = false;
            continue;
        }
        if !skip {
            out.push(line.to_string());
        }
    }
    out
}

/// `_ff_abort_is_managed_path`.
fn is_managed_path(repo_root: &Path, path: &str) -> bool {
    if MANAGED_FILES.contains(&path) {
        return true;
    }
    if MANAGED_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    if path == ".gitignore" {
        return gitignore_only_managed_block_dirty(repo_root);
    }
    false
}

/// `_ff_abort_all_dirty_tracked_managed` — `Some(paths)` iff at least one
/// TRACKED file is dirty and EVERY one of them is a managed path.
///
/// Conjunctive by design, matching #4951's "every blocking file" wording: one
/// unmanaged dirty file alongside managed ones still falls through to the hard
/// abort.
fn all_dirty_tracked_managed(repo_root: &Path) -> Option<Vec<String>> {
    let mut managed = Vec::new();
    let mut found_any = false;
    for line in porcelain(repo_root) {
        if &line[..2.min(line.len())] == "??" {
            continue;
        }
        let mut path = line.get(3..).unwrap_or("").to_string();
        if let Some(idx) = path.find(" -> ") {
            path = path[idx + " -> ".len()..].to_string();
        }
        // `${path%\"}` then `${path#\"}` — one trailing quote, then one
        // leading quote, in that order.
        if path.ends_with('"') {
            path.pop();
        }
        if path.starts_with('"') {
            path.remove(0);
        }
        found_any = true;
        if !is_managed_path(repo_root, &path) {
            return None;
        }
        managed.push(path);
    }
    if found_any {
        Some(managed)
    } else {
        None
    }
}

/// The resolved `resync-installed.sh`, exposed for the caller's own messages.
#[must_use]
pub fn resync_script(repo_root: &Path) -> Option<PathBuf> {
    paths::resolve_resync_script(repo_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_managed_surface_list_matches_resync_installed_sh() {
        // Not a tautology: this is the list the auto-resolve is allowed to
        // `git checkout --`, and #4239 already widened it once. If
        // resync-installed.sh grows a surface and this does not, the abort
        // message silently stops offering the safe resolution for it.
        assert_eq!(MANAGED_PREFIXES.len(), 7);
        assert!(MANAGED_PREFIXES.contains(&".loom/runtimes/"));
        assert!(MANAGED_PREFIXES.contains(&".claude/commands/loom/"));
        assert_eq!(MANAGED_FILES, &[".loom/install-metadata.json"]);
    }

    #[test]
    fn stripping_the_managed_block_leaves_hand_edited_lines() {
        let text =
            format!("mine.txt\n{GITIGNORE_BEGIN}\n.loom/worktrees/\n{GITIGNORE_END}\nalso-mine\n");
        assert_eq!(strip_managed_block(&text), vec!["mine.txt", "also-mine"]);
        // An unterminated block swallows the rest, as the awk did.
        let unterminated = format!("keep\n{GITIGNORE_BEGIN}\ngone\nalso-gone\n");
        assert_eq!(strip_managed_block(&unterminated), vec!["keep"]);
    }

    #[test]
    fn porcelain_paths_drop_a_rename_arrow_and_one_quote_pair() {
        // Exercised through is_managed_path's own prefix rule rather than a
        // private parse helper, since that is where the parsed path is used.
        let tmp = std::env::temp_dir().join(format!("loom-update-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(is_managed_path(&tmp, ".loom/scripts/cli/x.sh"));
        assert!(is_managed_path(&tmp, ".loom/install-metadata.json"));
        assert!(!is_managed_path(&tmp, ".loom/config.json"));
        assert!(!is_managed_path(&tmp, "src/main.rs"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
