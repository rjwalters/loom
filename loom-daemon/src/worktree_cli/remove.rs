//! `worktree.sh remove <N>` — the operator-facing single-worktree removal verb
//! (#3769), ported to Rust (#8195 slice 3, epic #7810).
//!
//! # Why this verb, and why now
//!
//! It is the destructive half of `worktree.sh`. Every irreversible operation
//! the issue counts — `git worktree remove --force`, `rm -rf`, `git branch -D`
//! — is reachable from here, and two of that script's three data-loss-class
//! fixes landed on this path or its immediate neighbours (#4449, a
//! tested-but-uncommitted fix destroyed because nothing checked for dirt;
//! #7858, an unquoted path turning a cleanup into an `rm -rf` on a LIVE
//! worktree). It is also **subcommand-shaped**, which is what makes it
//! portable now: the `loom-daemon` dependency lands on `worktree.sh remove`
//! and never on `worktree.sh <issue>`, so exactly three retained suites need a
//! built binary and the other sixteen stay hermetic. That was the explicit
//! reason slice 1's lock port was unwired and slice 2's WIP verbs were not.
//!
//! # The guard order is the contract
//!
//! Preserved step-for-step from the shell, because each step exists for a
//! named incident and the *order* is what makes them sound:
//!
//! 1. Absent directory ⇒ idempotent no-op success (still prune).
//! 2. No `.loom-managed` sentinel ⇒ refuse. User-provisioned worktrees are
//!    never removed by Loom; this is the sentinel contract.
//! 3. Uncommitted changes ⇒ refuse unless `--force` (#4449). Step 6 is
//!    `--force`, which has no safe variant to fall back to once it runs.
//! 4. Discover the attached branch, and resolve the cargo target dir, BEFORE
//!    removal — the porcelain entry and the manifest both vanish with the
//!    worktree.
//! 5. Hop out of the worktree if the process cwd is inside it.
//! 6. Remove; on the #5177 "is not a working tree" orphan shape only, fall
//!    back to a direct directory removal.
//! 7. Reclaim the *redirected* cargo target dir (#7239) — after the worktree
//!    is gone, so it cannot count as a live referent of its own target dir.
//! 8. Delete the attached branch under the squash-aware rule
//!    ([`super::branch_delete`]), then prune.
//!
//! # What Rust removes structurally, not by review
//!
//! Every path here is data: a worktree root (redirectable to an
//! operator-chosen prefix by `LOOM_WORKTREE_ROOT`), a repo root, an untracked
//! filename, a `git worktree list` path. In bash each has to survive word
//! splitting at every interpolation, and #7858 is what one miss costs.
//! `Command::arg` does not word-split and `remove_dir_all` takes a path, not a
//! string, so the class is gone by construction — `tests/worktree_remove_verb.rs`
//! pins that with a worktree root, a worktree and a dirty file whose names all
//! contain spaces and shell metacharacters.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::branch_delete::{self, BranchOutcome, DeleteContext};
use super::wip::{self, Out};
use crate::worktree_ops::{cargo_target, clean, removal_log, safety};

const USAGE: &str =
    "Usage: pnpm worktree remove <issue-number> [--keep-branch] [--force] [--dry-run] [--json]";

/// The ledger's `mechanism` field (#5950). Unchanged from the shell so one
/// `grep`/`jq` over `.loom/logs/worktree-removals.log` still attributes pre-
/// and post-port removals to the same name.
const LEDGER_MECHANISM: &str = "worktree.sh remove";
const LEDGER_REASON: &str = "explicit_remove";

/// Parsed argv. Kept separate from execution so the grammar is unit-testable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub issue_number: String,
    pub keep_branch: bool,
    pub json: bool,
    pub force: bool,
    pub dry_run: bool,
}

/// Why argv parsing refused, so the caller can emit the shell's exact message.
#[derive(Debug, PartialEq, Eq)]
pub enum ArgError {
    UnknownFlag(String),
    Unexpected(String),
    Missing,
    NotNumeric(String),
}

/// Parse the tail of `worktree.sh remove …`.
///
/// Hand-rolled rather than derived from clap for the reason slice 2 recorded:
/// the shell answers a malformed invocation with **exit 1** and its own
/// message, while clap answers with exit 2 — the code the stub reserves for
/// "no binary could be resolved". Collapsing "you typed it wrong" into "the
/// tool is not installed" is exactly the confusion that reservation prevents.
///
/// # Errors
///
/// Returns the shell's own refusal cases: an unknown `--flag`, a second
/// positional, a missing issue number, or a non-numeric one.
pub fn parse_args(argv: &[String]) -> Result<Args, ArgError> {
    let mut out = Args::default();
    for raw in argv {
        match raw.as_str() {
            "--keep-branch" => out.keep_branch = true,
            "--json" => out.json = true,
            "--force" | "-f" => out.force = true,
            "--dry-run" | "-n" => out.dry_run = true,
            other if other.starts_with("--") => {
                return Err(ArgError::UnknownFlag(other.to_string()))
            }
            other => {
                if out.issue_number.is_empty() {
                    out.issue_number = other.to_string();
                } else {
                    return Err(ArgError::Unexpected(other.to_string()));
                }
            }
        }
    }
    if out.issue_number.is_empty() {
        return Err(ArgError::Missing);
    }
    if !out.issue_number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ArgError::NotNumeric(out.issue_number));
    }
    Ok(out)
}

/// Run the verb. Returns the process exit code: `0` for success (including the
/// idempotent no-op and every `--dry-run`), `1` for a refusal, a usage error,
/// or a removal that failed.
///
/// There is deliberately no third code here. `2` belongs to the stub's
/// `LOOM_SCRIPT_HELPER_MISSING_RC`, and the one thing these codes must never
/// do is let "the binary could not be resolved" read as "removed" or as "I
/// looked at your worktree and declined".
#[must_use]
pub fn run(argv: &[String]) -> i32 {
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(ArgError::UnknownFlag(f)) => {
            Out::error(&format!("Unknown flag for remove: {f}"));
            println!();
            println!("{USAGE}");
            return 1;
        }
        Err(ArgError::Unexpected(a)) => {
            Out::error(&format!("Unexpected argument: {a}"));
            return 1;
        }
        Err(ArgError::Missing) => {
            Out::error("remove requires an issue number");
            println!();
            println!("{USAGE}");
            return 1;
        }
        Err(ArgError::NotNumeric(n)) => {
            Out::error(&format!("Issue number must be numeric (got: '{n}')"));
            println!();
            println!("{USAGE}");
            return 1;
        }
    };

    let out = Out::new(args.json);

    // Resolve the repo root even when invoked from inside a worktree: the git
    // common dir's parent is always the main workspace.
    let Some(repo_root) = wip::repo_root_from_cwd() else {
        Out::error("Not inside a git repository");
        return 1;
    };
    let worktree_root = crate::worktree_root::worktree_root(&repo_root);
    let worktree_path = worktree_root.join(format!("issue-{}", args.issue_number));

    let mut report = Report::new(&args, &worktree_path);

    // 1. Idempotent no-op if the worktree dir is absent (still prune any stale
    //    registration).
    if !worktree_path.is_dir() {
        prune(&repo_root);
        out.info(&format!(
            "No worktree found at {} — nothing to remove",
            wip::display(&worktree_path)
        ));
        report.branch_status = "absent";
        report.emit(&out, true, false);
        return 0;
    }

    // 2. Sentinel guard: refuse to remove a user-owned / non-managed worktree.
    if !worktree_path.join(".loom-managed").is_file() {
        Out::error(&format!(
            "Worktree at {} lacks .loom-managed sentinel — refusing to remove (user-owned)",
            wip::display(&worktree_path)
        ));
        report.branch_status = "untouched";
        report.emit(&out, false, false);
        return 1;
    }

    // 3. Dirty guard (#4449).
    let dirty = dirty_lines(&worktree_path);
    if !dirty.is_empty() {
        if !args.force {
            Out::error(&format!(
                "Refusing to remove {} — it has {} uncommitted change(s):",
                wip::display(&worktree_path),
                dirty.len()
            ));
            for line in dirty.iter().take(20) {
                eprintln!("{line}");
            }
            if dirty.len() > 20 {
                eprintln!("  ... and {} more", dirty.len() - 20);
            }
            eprintln!();
            eprintln!("Removing it would destroy that work irreversibly. To proceed, pick one:");
            let wt = wip::display(&worktree_path);
            let n = &args.issue_number;
            eprintln!("  1. Commit it:    git -C {wt} add -A && git -C {wt} commit -m '...'");
            eprintln!("  2. Save a patch: git -C {wt} diff HEAD > /tmp/issue-{n}.patch");
            eprintln!("  3. Stash it:     git -C {wt} stash push -u -m 'issue-{n}'");
            eprintln!("  4. Discard it:   re-run with --force (the uncommitted changes are lost)");
            report.branch_status = "untouched";
            report.emit(&out, false, false);
            return 1;
        }
        if args.dry_run {
            out.warning(&format!(
                "Worktree has {} uncommitted change(s) - a real run would discard them (--force)",
                dirty.len()
            ));
        } else {
            out.warning(&format!(
                "Worktree has {} uncommitted change(s) - discarding them (--force)",
                dirty.len()
            ));
        }
        for line in dirty.iter().take(20) {
            eprintln!("{line}");
        }
    }

    // 4. Discover the attached branch BEFORE removal.
    let attached_branch = attached_branch(&repo_root, &worktree_path);
    report.branch.clone_from(&attached_branch);

    // 4b. Resolve this worktree's cargo target dir BEFORE removal (#7239):
    //     `cargo metadata` needs the worktree's manifest, which is gone the
    //     moment step 6 runs. Resolution short-circuits unless a redirect is
    //     actually possible, so an ordinary host pays no cargo invocation.
    let resolved_target_dir = cargo_target::resolve_for_worktree(&worktree_path);

    // 4c. --dry-run: report the full plan and change nothing. Same decision
    //     path a real removal takes, so what it lists is exactly what a real
    //     run would delete.
    if args.dry_run {
        out.info(&format!("Would remove worktree: {}", wip::display(&worktree_path)));
        match (&attached_branch, args.keep_branch) {
            (Some(b), true) => out.info(&format!("Would keep local branch '{b}' (--keep-branch)")),
            (Some(b), false) => out.info(&format!("Would delete local branch '{b}'")),
            (None, _) => {}
        }
        report.report_target_dir(
            &out,
            &cargo_target::reclaim(&repo_root, &worktree_path, &resolved_target_dir, true),
        );
        report.branch_status = "dry-run";
        report.emit(&out, true, false);
        return 0;
    }

    // 5. CWD-safety: if this process's cwd is inside the worktree, hop out
    //    first. `git worktree remove` can succeed with a deleted cwd, but
    //    every child process spawned afterwards inherits an unresolvable
    //    working directory.
    let worktree_real = real(&worktree_path);
    let in_worktree = std::env::current_dir()
        .map(|cwd| real(&cwd).starts_with(&worktree_real))
        .unwrap_or(false);
    if in_worktree {
        let _ = std::env::set_current_dir(&repo_root);
    }

    // 6. Remove the worktree.
    out.info(&format!("Removing worktree: {}", wip::display(&worktree_path)));
    let mut removed = false;
    match git_remove_worktree(&repo_root, &worktree_path) {
        Ok(()) => {
            removed = true;
            out.success("Worktree removed");
            if in_worktree {
                out.warning("Your shell's working directory was inside the removed worktree.");
                out.warning(&format!("Run this command to fix:  cd {}", wip::display(&repo_root)));
            }
        }
        Err(cause)
            if clean::should_force_remove_orphan_dir(
                &cause,
                worktree_path.join(".loom-managed").is_file(),
                clean::is_under_worktree_root(&repo_root, &worktree_path),
            ) =>
        {
            // #5177: git no longer tracks this path as a worktree (e.g. a
            // stale `git worktree prune` left the directory on disk), so
            // `git worktree remove` can never clean it and it accumulates
            // forever. All three proofs are required — the specific git
            // error, the sentinel re-checked here, and containment under the
            // managed worktree root — so this never degrades into a blanket
            // `rm -rf` on any removal failure. Containment is the one the
            // shell left implicit (its path is built from the worktree root,
            // so it "cannot" escape); asserting it explicitly is what lets
            // this share `clean`'s predicate instead of being a second,
            // slightly weaker copy of the same rule.
            if std::fs::remove_dir_all(&worktree_path).is_ok() {
                removed = true;
                out.success("Removed untracked worktree directory (no git worktree entry)");
            } else {
                out.warning(&format!(
                    "Could not remove untracked worktree directory at {}",
                    wip::display(&worktree_path)
                ));
            }
        }
        Err(_) => {
            out.warning(&format!("Could not remove worktree at {}", wip::display(&worktree_path)));
        }
    }

    if removed {
        // 6b. #5950: record the removal in the shared ledger, covering both
        //     the ordinary path and the #5177 direct-removal fallback.
        removal_log::record(
            &repo_root,
            LEDGER_MECHANISM,
            &worktree_path,
            attached_branch.as_deref(),
            LEDGER_REASON,
        );
        // 6c. #7239: reclaim the redirected cargo target dir resolved in 4b —
        //     only now that the worktree is actually gone. A failed removal
        //     leaves it alone: the worktree that owns it is still there.
        report.report_target_dir(
            &out,
            &cargo_target::reclaim(&repo_root, &worktree_path, &resolved_target_dir, false),
        );
    }

    // 7. Branch cleanup (unless --keep-branch). Deferred until after removal
    //    so the worktree's checkout lock on the branch is released first.
    let mut branch_status = "none";
    if args.keep_branch {
        if let Some(b) = &attached_branch {
            out.info(&format!("Keeping local branch '{b}' (--keep-branch)"));
            branch_status = "kept";
        }
    } else if removed {
        if let Some(b) = &attached_branch {
            if branch_delete::branch_exists(&repo_root, b) {
                let default = super::default_branch::resolve(&repo_root);
                let ctx = DeleteContext {
                    repo_root: &repo_root,
                    default_branch: default.as_deref(),
                    cleanup_primary_checkout: cleanup_primary_checkout_enabled(),
                };
                let _outcome: BranchOutcome =
                    branch_delete::maybe_delete_local_branch(&ctx, &out, b);
                // Re-read the ref rather than trusting the returned outcome:
                // the shell reported `unmerged`/`deleted` from a fresh
                // `show-ref`, and a status that disagrees with the repository
                // is worse than no status at all.
                branch_status = if branch_delete::branch_exists(&repo_root, b) {
                    "unmerged"
                } else {
                    "deleted"
                };
            } else {
                out.info(&format!("Local branch '{b}' does not exist — skipping branch delete"));
                branch_status = "absent";
            }
        }
    }
    report.branch_status = branch_status;

    // 8. Prune the git worktree registration.
    prune(&repo_root);

    if removed {
        report.emit(&out, true, true);
        0
    } else {
        report.emit(&out, false, false);
        1
    }
}

/// `CLEANUP_PRIMARY_CHECKOUT` — `merge-pr.sh`'s `--no-cleanup-primary` opt-out,
/// honoured here because the rule this verb calls reads it. Mirrors the
/// shell's `[[ "${CLEANUP_PRIMARY_CHECKOUT:-true}" == "true" ]]`: unset is on,
/// anything that is not the literal `true` is off.
fn cleanup_primary_checkout_enabled() -> bool {
    std::env::var("CLEANUP_PRIMARY_CHECKOUT").map_or(true, |v| v == "true")
}

/// The `--json` document plus the two fields (`branch`, `targetDir`) that are
/// discovered mid-run.
struct Report {
    issue_number: String,
    worktree_path: String,
    dry_run: bool,
    branch: Option<String>,
    branch_status: &'static str,
    target_dir_path: String,
    target_dir_status: &'static str,
}

impl Report {
    fn new(args: &Args, worktree_path: &Path) -> Self {
        Self {
            issue_number: args.issue_number.clone(),
            worktree_path: wip::display(worktree_path),
            dry_run: args.dry_run,
            branch: None,
            branch_status: "none",
            target_dir_path: String::new(),
            target_dir_status: "unchecked",
        }
    }

    /// Report one cargo-target-dir decision as a human line and stash it for
    /// `--json`. Never writes to stdout directly — stdout purity under
    /// `--json` is the whole reason [`Out`] routes by mode.
    ///
    /// The severity split is the shell's: a reclaim is good news, a dry-run
    /// preview and a *shared* directory are neutral facts, and everything that
    /// kept a directory an operator expected to be freed is a warning they
    /// should see. `inside` / `absent` — the overwhelmingly common,
    /// uninteresting cases on a host with no redirect — are silent by design.
    fn report_target_dir(&mut self, out: &Out, outcome: &cargo_target::TargetDirOutcome) {
        use cargo_target::TargetDirOutcome as O;
        let (status, path) = match outcome {
            O::Inside(p) => ("inside", p),
            O::Absent(p) => ("absent", p),
            O::Refused { path, .. } => ("refused", path),
            O::Shared { path, .. } => ("shared", path),
            O::Protected { path, .. } => ("protected", path),
            O::WouldReclaim { path, .. } => ("would-reclaim", path),
            O::Reclaimed { path, .. } => ("reclaimed", path),
            O::Failed { path, .. } => ("failed", path),
        };
        self.target_dir_status = status;
        self.target_dir_path = wip::display(path);
        let Some(line) = outcome.report_line() else {
            return;
        };
        match outcome {
            O::Reclaimed { .. } => out.success(&line),
            O::WouldReclaim { .. } | O::Shared { .. } => out.info(&line),
            _ => out.warning(&line),
        }
    }

    fn emit(&self, out: &Out, success: bool, removed: bool) {
        out.json_line(&self.render(success, removed));
    }

    /// The document itself, split out from [`Report::emit`] so its shape is
    /// testable without capturing stdout.
    fn render(&self, success: bool, removed: bool) -> String {
        format!(
            r#"{{"success": {success}, "issueNumber": {issue}, "worktreePath": "{path}", "removed": {removed}, "branch": "{branch}", "branchStatus": "{status}", "dryRun": {dry_run}, "targetDir": "{target}", "targetDirStatus": "{target_status}"}}"#,
            issue = self.issue_number,
            path = wip::json_str(&self.worktree_path),
            branch = wip::json_str(self.branch.as_deref().unwrap_or_default()),
            status = self.branch_status,
            dry_run = self.dry_run,
            target = wip::json_str(&self.target_dir_path),
            target_status = self.target_dir_status,
        )
    }
}

/// A worktree's uncommitted-change lines in `git status --porcelain` format,
/// EXCLUDING Loom's own runtime markers (#4449).
///
/// `.loom-managed` / `.loom-in-use` / `.loom-checkpoint` / `.no-changes-needed`
/// are breadcrumbs every managed worktree legitimately carries. A correctly
/// installed repo gitignores them; a stale or pre-#3838 `.gitignore` does not —
/// and if they counted as work, this guard would refuse to remove *every*
/// managed worktree, which is worse than no guard at all.
///
/// The marker test is [`safety::is_loom_own_untracked_path`], shared with the
/// daemon's own reclaim path rather than re-encoded here: #8279 is what
/// happened when two places listed the markers separately, and the WIP verbs
/// ported in slice 2 already share this predicate.
#[must_use]
pub fn dirty_lines(worktree: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| !is_marker_line(line))
        .map(std::string::ToString::to_string)
        .collect()
}

/// Porcelain v1: 2 status chars + 1 space, then the path. Renames render as
/// `old -> new` and never match a bare marker name.
fn is_marker_line(line: &str) -> bool {
    let path = line.get(3..).unwrap_or("");
    let path = path.trim_matches('"');
    safety::is_loom_own_untracked_path(path)
}

/// The short branch name attached to `target`, parsed from
/// `git worktree list --porcelain`.
///
/// Robust to a custom branch name (`worktree.sh <N> <custom-branch>`). `None`
/// for a detached or bare worktree. Both sides of the path comparison are
/// canonicalized, as the shell's `cd … && pwd -P` was.
fn attached_branch(repo_root: &Path, target: &Path) -> Option<String> {
    let target_real = real(target);
    branch_delete::worktree_entries(repo_root)
        .into_iter()
        .find(|(path, branch)| branch.is_some() && real(path) == target_real)
        .and_then(|(_, branch)| branch)
        .map(|b| b.strip_prefix("refs/heads/").unwrap_or(&b).to_string())
}

fn prune(repo_root: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "prune"])
        .output();
}

/// `git worktree remove <path> --force`, returning git's combined output as
/// the error cause (issue #4877's convention) so the #5177 classifier can read
/// it.
fn git_remove_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "remove"])
        .arg(worktree_path)
        .arg("--force")
        .output();
    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let mut cause = String::from_utf8_lossy(&out.stderr).into_owned();
            cause.push_str(&String::from_utf8_lossy(&out.stdout));
            Err(cause.trim().to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests;
