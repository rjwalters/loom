//! `worktree.sh stash-push` / `stash-pop` — the per-target clean-baseline
//! stash pair (#8195, epic #7810 slice 2; originally #5217, `main` target
//! #6076).
//!
//! # What this replaces and why it cannot use `git stash`
//!
//! The pattern being replaced is `git stash && <baseline check> && git stash
//! pop`: shelve WIP, run clippy/shellcheck/tests against a clean tree, put the
//! WIP back. `refs/stash` is a single stack shared by every linked worktree of
//! the repository, so a concurrent builder's push can land between *your* push
//! and *your* pop, and your pop then restores their work into your worktree.
//! That is not theoretical — `tests 10 and 11` of the retained suite
//! demonstrate it happening and not happening, side by side.
//!
//! So this pair anchors each capture to its OWN ref,
//! `refs/loom/stash-baseline/<slug>`, built with `git stash create` (which
//! produces a stash-format commit object but, unlike `git stash push`, writes
//! nothing to `refs/stash`). There is no shared stack to interleave on, which
//! removes the race's precondition rather than trying to detect it.
//!
//! # The three things that must not go wrong
//!
//! 1. **`git reset --hard HEAD` only ever runs AFTER a successful capture.**
//!    It is the one irreversible operation here. `stash create` runs first and
//!    its output is anchored to the ref first; if either fails, the reset does
//!    not happen and the caller's tree is untouched.
//! 2. **A pending capture is never silently overwritten.** A second
//!    `stash-push` before its `stash-pop` is refused, loudly, naming the
//!    restore command — because the alternative is capturing on top of an
//!    outstanding baseline and stranding the first one.
//! 3. **A failed restore preserves everything.** If `git stash apply`
//!    conflicts, the ref is NOT deleted and the holding directory is NOT
//!    cleared; the error names the exact commands to recover by hand.
//!
//! # Why a pending marker exists even when nothing was captured
//!
//! The intended use is one `&&` chain. On an already-clean worktree there is
//! nothing to capture, but `stash-pop` must still succeed or the chain breaks
//! mid-sweep — reintroducing exactly the headless stall #5217 removed. The
//! marker is what distinguishes "you pushed and there was nothing" (a
//! legitimate no-op restore) from "you never pushed" (a real error). For the
//! `main` target it doubles as the concurrency interlock the shared stack
//! never had.

use super::wip::{self, Layout, Out, Target};

const PUSH_USAGE: &str =
    "Usage: pnpm worktree stash-push <issue-number|main> [--include-untracked] [--json]";
const POP_USAGE: &str = "Usage: pnpm worktree stash-pop <issue-number|main> [--json]";

/// Parsed argv for either verb.
struct Parsed {
    target: Target,
    json: bool,
    include_untracked: bool,
}

/// Shared argv parsing. `verb` and `usage` differ only in the messages, which
/// are contract (the retained suite greps the refusal text).
///
/// Hand-rolled for the same reason as `snapshot`'s: the shell answers a bad
/// target with exit 1, and clap would answer with exit 2 — the code the stub
/// reserves for "the binary could not run".
fn parse(args: &[String], verb: &str, usage: &str, allow_untracked: bool) -> Result<Parsed, i32> {
    let mut target: Option<String> = None;
    let mut json = false;
    let mut include_untracked = false;

    for a in args {
        match a.as_str() {
            "--include-untracked" if allow_untracked => include_untracked = true,
            "--json" => json = true,
            s if s.starts_with("--") => {
                Out::error(&format!("Unknown flag for {verb}: {s}"));
                println!();
                println!("{usage}");
                return Err(1);
            }
            s => {
                if target.is_none() {
                    target = Some(s.to_string());
                } else {
                    Out::error(&format!("Unexpected argument: {s}"));
                    return Err(1);
                }
            }
        }
    }

    let Some(raw) = target else {
        Out::error(&format!("{verb} requires an issue number (or 'main')"));
        println!();
        println!("{usage}");
        return Err(1);
    };
    let Some(target) = Target::parse(&raw) else {
        Out::error(&format!("Target must be an issue number or 'main' (got: '{raw}')"));
        println!();
        println!("{usage}");
        return Err(1);
    };

    Ok(Parsed {
        target,
        json,
        include_untracked,
    })
}

/// Paths a capture lives in. Every one of them is OUTSIDE the worktree: the
/// ref lives in the repo's common git dir, the holding directory and marker
/// under `<worktree-root>/.stash-baseline/<slug>`. So removing the worktree
/// while a push is pending strands nothing — `git stash apply <ref>` still
/// replays the diff.
struct Capture {
    ref_name: String,
    holding_dir: std::path::PathBuf,
    manifest: std::path::PathBuf,
    pending: std::path::PathBuf,
}

impl Capture {
    fn of(layout: &Layout, target: &Target) -> Self {
        let holding_dir = layout.holding_dir(target);
        Self {
            ref_name: target.baseline_ref(),
            manifest: holding_dir.join("untracked.manifest"),
            pending: holding_dir.join("pending"),
            holding_dir,
        }
    }

    fn untracked_dir(&self) -> std::path::PathBuf {
        self.holding_dir.join("untracked")
    }
}

// ---------------------------------------------------------------------------
// stash-push
// ---------------------------------------------------------------------------

/// Run `stash-push`. Returns the process exit code.
#[must_use]
pub fn push(args: &[String]) -> i32 {
    let parsed = match parse(args, "stash-push", PUSH_USAGE, true) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let out = Out::new(parsed.json);

    let Some(layout) = Layout::resolve(&parsed.target) else {
        Out::error("Not inside a git repository");
        return 1;
    };
    push_inner(&out, &layout, &parsed)
}

fn push_json(out: &Out, t: &Target, ok: bool, tracked: bool, untracked: u32, r: &str) {
    out.json_line(&format!(
        "{{\"success\": {ok}, \"issueNumber\": {}, \"target\": \"{}\", \"hasTrackedChanges\": {tracked}, \"untrackedCount\": {untracked}, \"ref\": \"{}\"}}",
        t.json_issue(),
        wip::json_str(&t.label()),
        wip::json_str(r)
    ));
}

#[allow(clippy::too_many_lines)]
fn push_inner(out: &Out, layout: &Layout, parsed: &Parsed) -> i32 {
    let target = &parsed.target;
    let wt = &layout.worktree_path;
    let label = target.label();

    if !wt.is_dir() {
        Out::error(&format!("No worktree found at {} — nothing to stash-push", wip::display(wt)));
        push_json(out, target, false, false, 0, "");
        return 1;
    }
    if !wip::is_git_worktree(wt) {
        Out::error(&format!("{} is not a git working tree", wip::display(wt)));
        push_json(out, target, false, false, 0, "");
        return 1;
    }

    let cap = Capture::of(layout, target);

    // Refuse rather than capture on top of an outstanding baseline. All three
    // conditions are checked because any one of them alone can be the residue
    // of a push that got part-way: a ref with no marker, a marker with no ref
    // (the already-clean case), or a manifest with neither.
    if wip::ref_exists(wt, &cap.ref_name) || cap.manifest.is_file() || cap.pending.is_file() {
        Out::error(&format!(
            "A pending stash-push already exists for {label} — run 'stash-pop {label}' first (or resolve manually: ref {} / {})",
            cap.ref_name,
            wip::display(&cap.holding_dir)
        ));
        push_json(out, target, false, false, 0, "");
        return 1;
    }

    // `stash create` builds the commit object WITHOUT writing refs/stash.
    // Empty output means the tracked tree was already clean.
    let stash_commit = wip::git_stdout(wt, &["stash", "create"]).unwrap_or_default();
    let has_tracked = !stash_commit.is_empty();

    if has_tracked {
        if !wip::git_ok(wt, &["update-ref", &cap.ref_name, &stash_commit]) {
            // Anchor first, reset second. The commit object exists but is
            // unreferenced and will be gc'd, so refusing here loses nothing —
            // whereas resetting a tree whose capture is not anchored loses the
            // tree.
            Out::error(&format!("Failed to anchor baseline commit under {}", cap.ref_name));
            push_json(out, target, false, false, 0, "");
            return 1;
        }
        if !wip::git_ok(wt, &["reset", "--hard", "HEAD"]) {
            Out::error(&format!(
                "Failed to reset {} to a clean baseline after capturing WIP — baseline preserved at {}, nothing lost",
                wip::display(wt),
                cap.ref_name
            ));
            push_json(out, target, false, true, 0, &cap.ref_name);
            return 1;
        }
    }

    let mut untracked_count: u32 = 0;
    if parsed.include_untracked {
        let untracked = wip::untracked_files(wt);
        if !untracked.is_empty() {
            if std::fs::create_dir_all(cap.untracked_dir()).is_err() {
                Out::error(&format!(
                    "Could not create holding directory: {}",
                    wip::display(&cap.untracked_dir())
                ));
                push_json(out, target, false, has_tracked, 0, &cap.ref_name);
                return 1;
            }
            // The manifest is written INCREMENTALLY, one line per file as soon
            // as that file has moved — never buffered and written at the end.
            // A crash between the move and the write would otherwise leave the
            // file sitting in the holding directory with nothing recording
            // where it came from, i.e. unrecoverable by `stash-pop` and
            // findable only by hand. The shell had the same property via
            // `echo "$f" >> "$manifest_path"`, and it is the only thing making
            // a half-finished push recoverable.
            let Ok(mut manifest) = std::fs::File::create(&cap.manifest) else {
                Out::error(&format!(
                    "Could not create the untracked manifest at {}",
                    wip::display(&cap.manifest)
                ));
                push_json(out, target, false, has_tracked, 0, &cap.ref_name);
                return 1;
            };
            for f in &untracked {
                let dest = cap.untracked_dir().join(f);
                let Some(parent) = dest.parent() else {
                    continue;
                };
                if std::fs::create_dir_all(parent).is_err() {
                    continue;
                }
                if wip::move_file(&wt.join(f), &dest) {
                    use std::io::Write as _;
                    let _ = writeln!(manifest, "{f}");
                    let _ = manifest.flush();
                    untracked_count += 1;
                }
            }
            drop(manifest);
            if untracked_count == 0 {
                let _ = std::fs::remove_file(&cap.manifest);
            }
        }
    }

    if std::fs::create_dir_all(&cap.holding_dir).is_err()
        || std::fs::write(&cap.pending, wip::iso_stamp() + "\n").is_err()
    {
        Out::error(&format!(
            "Could not record the pending-push marker at {}",
            wip::display(&cap.pending)
        ));
        push_json(out, target, false, has_tracked, untracked_count, &cap.ref_name);
        return 1;
    }

    if !has_tracked && untracked_count == 0 {
        out.info(&format!(
            "No uncommitted changes to push for {label} — working tree was already clean"
        ));
    } else {
        out.success(&format!(
            "Baseline captured for {label} (tracked: {has_tracked}, untracked files moved: {untracked_count})"
        ));
    }
    out.info(&format!("Restore with: ./.loom/scripts/worktree.sh stash-pop {label}"));

    push_json(out, target, true, has_tracked, untracked_count, &cap.ref_name);
    0
}

// ---------------------------------------------------------------------------
// stash-pop
// ---------------------------------------------------------------------------

/// Run `stash-pop`. Returns the process exit code.
#[must_use]
pub fn pop(args: &[String]) -> i32 {
    let parsed = match parse(args, "stash-pop", POP_USAGE, false) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let out = Out::new(parsed.json);

    let Some(layout) = Layout::resolve(&parsed.target) else {
        Out::error("Not inside a git repository");
        return 1;
    };
    pop_inner(&out, &layout, &parsed.target)
}

fn pop_json(out: &Out, t: &Target, ok: bool, tracked: bool, untracked: u32) {
    out.json_line(&format!(
        "{{\"success\": {ok}, \"issueNumber\": {}, \"target\": \"{}\", \"restoredTracked\": {tracked}, \"restoredUntrackedCount\": {untracked}}}",
        t.json_issue(),
        wip::json_str(&t.label())
    ));
}

fn pop_inner(out: &Out, layout: &Layout, target: &Target) -> i32 {
    let wt = &layout.worktree_path;
    let label = target.label();

    if !wt.is_dir() {
        Out::error(&format!("No worktree found at {}", wip::display(wt)));
        pop_json(out, target, false, false, 0);
        return 1;
    }
    if !wip::is_git_worktree(wt) {
        Out::error(&format!("{} is not a git working tree", wip::display(wt)));
        pop_json(out, target, false, false, 0);
        return 1;
    }

    let cap = Capture::of(layout, target);

    let has_tracked = wip::ref_exists(wt, &cap.ref_name);
    let stash_commit = if has_tracked {
        wip::git_stdout(wt, &["rev-parse", &cap.ref_name]).unwrap_or_default()
    } else {
        String::new()
    };
    let has_manifest = cap.manifest.is_file();
    let has_pending = cap.pending.is_file();

    // Nothing captured AND no record of a push means the caller never pushed —
    // a real error. Nothing captured WITH a marker means the push found an
    // already-clean tree, which must restore as a no-op so the `&&` chain does
    // not break (retained-suite tests 12 and 17).
    if !has_tracked && !has_manifest && !has_pending {
        Out::error(&format!("Nothing to restore for {label} — run 'stash-push {label}' first"));
        pop_json(out, target, false, false, 0);
        return 1;
    }

    if has_tracked {
        // `stash apply <commit>` with an explicit commit reads nothing from
        // refs/stash, so a concurrent worktree's entry on the shared stack is
        // neither consumed nor able to answer here.
        if !wip::git_ok(wt, &["stash", "apply", &stash_commit]) {
            Out::error(&format!(
                "Failed to apply baseline commit {stash_commit} for {label} (likely conflicts with the current tree). The captured baseline is PRESERVED at {} — resolve manually with 'git -C {} stash apply {stash_commit}', then delete the ref with 'git -C {} update-ref -d {}'.",
                cap.ref_name,
                wip::display(wt),
                wip::display(wt),
                cap.ref_name
            ));
            pop_json(out, target, false, false, 0);
            return 1;
        }
        let _ = wip::git(wt, &["update-ref", "-d", &cap.ref_name]);
    }

    let mut restored_untracked: u32 = 0;
    if has_manifest {
        let manifest = std::fs::read_to_string(&cap.manifest).unwrap_or_default();
        let mut restore_failed = false;
        for f in manifest.lines().filter(|l| !l.is_empty()) {
            let src = cap.untracked_dir().join(f);
            if !src.is_file() {
                continue;
            }
            let dest = wt.join(f);
            let Some(parent) = dest.parent() else {
                restore_failed = true;
                continue;
            };
            if std::fs::create_dir_all(parent).is_err() {
                restore_failed = true;
                continue;
            }
            if wip::move_file(&src, &dest) {
                restored_untracked += 1;
            } else {
                restore_failed = true;
            }
        }

        if restore_failed {
            // Keep the manifest and whatever is left in the holding directory:
            // a partial restore that then erased its own record would make the
            // remaining files unfindable.
            Out::error(&format!(
                "Some untracked files for {label} could not be restored — remaining files are still under {} (manifest kept at {} for manual recovery)",
                wip::display(&cap.untracked_dir()),
                wip::display(&cap.manifest)
            ));
            pop_json(out, target, false, has_tracked, restored_untracked);
            return 1;
        }

        let _ = std::fs::remove_file(&cap.manifest);
        // Non-recursive on purpose: a leftover subdirectory means something is
        // still in there, and removing it would be a delete this verb never
        // promised.
        let _ = std::fs::remove_dir(cap.untracked_dir());
    }

    // Cleared last: every path above either restored cleanly or returned early
    // with the captured state preserved, so reaching here means the pair is
    // complete.
    let _ = std::fs::remove_file(&cap.pending);
    let _ = std::fs::remove_dir(&cap.holding_dir);

    if !has_tracked && restored_untracked == 0 {
        out.info(&format!(
            "Nothing was captured for {label} — the working tree was already clean at stash-push time"
        ));
        pop_json(out, target, true, false, 0);
        return 0;
    }

    out.success(&format!(
        "Baseline restored for {label} (tracked: {has_tracked}, untracked files restored: {restored_untracked})"
    ));
    pop_json(out, target, true, has_tracked, restored_untracked);
    0
}
