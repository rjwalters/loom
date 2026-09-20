//! `worktree.sh snapshot <N>` — capture one worktree's uncommitted WIP as a
//! standalone patch file, without touching `git stash` (#8195, epic #7810
//! slice 2; originally #4778).
//!
//! # The contract, and why each part of it
//!
//! - **`refs/stash` is never touched.** It is one stack shared by every linked
//!   worktree of the repository, so two builders shelving WIP at the same time
//!   in different `issue-<N>` worktrees can pop or drop each other's entry
//!   (#4821 — observed in production). A patch file is per-invocation and
//!   per-path; there is no shared mutable list to collide on. The retained
//!   suite asserts `git stash list` is byte-identical across a snapshot.
//! - **The location is deterministic and resolved through
//!   [`crate::worktree_root::worktree_root`]**:
//!   `<worktree-root>/.snapshots/issue-<N>-<UTC-timestamp>.patch`. Resolving it
//!   through the shared helper rather than a hardcoded `.loom/worktrees` is
//!   what makes an overridden `LOOM_WORKTREE_ROOT` redirect snapshots along
//!   with the worktrees they belong to.
//! - **A clean worktree still succeeds**, writing an empty patch. "Nothing to
//!   capture" is an answer, not a failure, and a caller that treated it as one
//!   would break the `&&` chains this verb is used in.
//! - **`--include-untracked` never stages anything.** Untracked files are
//!   folded in via a temporary `git add -N` (intent-to-add, no content
//!   staged), and the index is reset immediately after the diff is taken — so
//!   the worktree ends in exactly the state it started in. The retained suite
//!   asserts the file is still reported `??` afterwards.
//! - **Loom's own runtime markers are never captured**, even with
//!   `--include-untracked` — see
//!   [`crate::worktree_ops::safety::is_loom_own_untracked_path`].

use std::path::Path;

use super::wip::{self, Layout, Out, Target};

const USAGE: &str = "Usage: pnpm worktree snapshot <issue-number> [--include-untracked] [--json]";

/// Run the verb. The return value is the process exit code; `0` success, `1`
/// every failure — matching the shell, whose `snapshot_worktree_command` was
/// dispatched as `… "$@" && exit 0; exit 1`.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut issue: Option<String> = None;
    let mut json = false;
    let mut include_untracked = false;

    // Hand-rolled rather than clap-derived, deliberately: the shell's parser
    // answers a missing target with exit 1 and a named message, and an unknown
    // flag with exit 1 and `Unknown flag for snapshot: …`. clap answers both
    // with exit 2 — which is the code the stub reserves for "the binary could
    // not run at all". Collapsing those two meanings is exactly the confusion
    // `LOOM_SCRIPT_HELPER_MISSING_RC` exists to prevent.
    for a in args {
        match a.as_str() {
            "--include-untracked" => include_untracked = true,
            "--json" => json = true,
            s if s.starts_with("--") => {
                Out::error(&format!("Unknown flag for snapshot: {s}"));
                println!();
                println!("{USAGE}");
                return 1;
            }
            s => {
                if issue.is_none() {
                    issue = Some(s.to_string());
                } else {
                    Out::error(&format!("Unexpected argument: {s}"));
                    return 1;
                }
            }
        }
    }

    let out = Out::new(json);

    let Some(raw) = issue else {
        Out::error("snapshot requires an issue number");
        println!();
        println!("{USAGE}");
        return 1;
    };
    // `snapshot` is issue-only: it has no `main` target, so a bare numeric test
    // is the whole grammar and `Target::parse` would wrongly accept `main`.
    let Some(number) = raw
        .parse::<u32>()
        .ok()
        .filter(|_| raw.bytes().all(|b| b.is_ascii_digit()))
    else {
        Out::error(&format!("Issue number must be numeric (got: '{raw}')"));
        println!();
        println!("{USAGE}");
        return 1;
    };
    let target = Target::Issue(number);

    let Some(layout) = Layout::resolve(&target) else {
        Out::error("Not inside a git repository");
        return 1;
    };

    snapshot(&out, &layout, number, include_untracked)
}

fn fail(out: &Out, number: u32) -> i32 {
    out.json_line(&format!(
        "{{\"success\": false, \"issueNumber\": {number}, \"patchPath\": \"\", \"hasChanges\": false, \"bytes\": 0}}"
    ));
    1
}

fn snapshot(out: &Out, layout: &Layout, number: u32, include_untracked: bool) -> i32 {
    let wt = &layout.worktree_path;
    if !wt.is_dir() {
        Out::error(&format!("No worktree found at {} — nothing to snapshot", wip::display(wt)));
        return fail(out, number);
    }
    if !wip::is_git_worktree(wt) {
        Out::error(&format!("{} is not a git working tree", wip::display(wt)));
        return fail(out, number);
    }

    let snapshot_dir = layout.worktree_root.join(".snapshots");
    if std::fs::create_dir_all(&snapshot_dir).is_err() {
        Out::error(&format!(
            "Could not create snapshot directory: {}",
            wip::display(&snapshot_dir)
        ));
        return fail(out, number);
    }

    let patch_path = snapshot_dir.join(format!("issue-{number}-{}.patch", wip::snapshot_stamp()));

    // Intent-to-add makes an untracked file render as a new-file hunk in
    // `git diff HEAD` WITHOUT staging its content. Recorded per-path so the
    // reset below undoes exactly what was added and nothing else — a blanket
    // `git reset` would also unstage whatever the caller had deliberately
    // staged before invoking us.
    let mut added_for_diff: Vec<String> = Vec::new();
    if include_untracked {
        for f in wip::untracked_files(wt) {
            if wip::git_ok(wt, &["add", "-N", "--", &f]) {
                added_for_diff.push(f);
            }
        }
    }

    let diff_status = write_diff(wt, &patch_path);

    if !added_for_diff.is_empty() {
        let mut argv: Vec<&str> = vec!["reset", "--"];
        argv.extend(added_for_diff.iter().map(String::as_str));
        let _ = wip::git(wt, &argv);
    }

    if diff_status != 0 {
        // The redirect created the file before git ran, so a failed diff leaves
        // a truncated (or empty) patch that looks exactly like a legitimate
        // "no changes" capture. Removing it is what keeps "there is a patch
        // file" a reliable statement.
        let _ = std::fs::remove_file(&patch_path);
        Out::error(&format!("git diff failed for {} (exit {diff_status})", wip::display(wt)));
        return fail(out, number);
    }

    let bytes = std::fs::metadata(&patch_path).map_or(0, |m| m.len());
    let has_changes = bytes > 0;
    let shown = wip::display(&patch_path);

    if has_changes {
        out.success(&format!("Snapshot written: {shown} ({bytes} bytes)"));
    } else {
        out.info(&format!("No uncommitted changes — wrote an empty snapshot: {shown}"));
    }
    out.info(&format!("Replay into a fresh worktree with: git apply {shown}"));

    out.json_line(&format!(
        "{{\"success\": true, \"issueNumber\": {number}, \"patchPath\": \"{}\", \"hasChanges\": {has_changes}, \"bytes\": {bytes}}}",
        wip::json_str(&shown)
    ));
    0
}

/// `git diff HEAD > <patch>`, streamed rather than buffered.
///
/// A worktree diff is unbounded in size (a vendored dependency update is
/// megabytes), so this redirects git's stdout straight into the file instead
/// of collecting it in memory first.
///
/// A plain `git diff` does not use its exit code to signal "has changes", so a
/// non-zero status here is a genuine failure (bad HEAD, corrupt worktree) —
/// the same reasoning the shell recorded for omitting `--exit-code`.
///
/// Returns git's exit code (0 on success), which the caller reports verbatim
/// the way the shell's `(exit $diff_status)` did. A git that could not be
/// spawned at all reports 127, bash's own code for that.
fn write_diff(wt: &Path, patch_path: &Path) -> i32 {
    let Ok(file) = std::fs::File::create(patch_path) else {
        return 1;
    };
    std::process::Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["diff", "HEAD"])
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(127, |s| s.code().unwrap_or(1))
}
