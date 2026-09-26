//! The race-safe stale-worktree reset — `worktree.sh`'s **rescue-or-refuse**
//! guard in front of `git reset --hard` (#8195 slice 6, epic #7810).
//!
//! # What moved here
//!
//! `defaults/scripts/lib/worktree-race-rescue.sh`'s `loom_worktree_reset_or_rescue`
//! and its `loom_worktree_has_live_process` helper, in the shell's own order —
//! the order is observable, because each rung refuses with its own message and
//! the later rungs never run once an earlier one has:
//!
//! 1. **A live process holds the worktree open** (cwd inside it, #7463) →
//!    refuse. Runs before git is touched at all: git-level signals are a
//!    point-in-time snapshot, and a live writer is evidence that more tracked
//!    edits are in flight than any snapshot can see.
//! 2. **The worktree gained commits** since the caller's staleness check →
//!    refuse, rather than rewind history out from under whoever committed it.
//! 3. **The worktree has foreign uncommitted TRACKED changes** → capture them
//!    to `<worktree>/.snapshots/<label>-<UTC>.patch` *first*; refuse if that
//!    capture cannot be made. Untracked files are not rescued because
//!    `git reset --hard` never touches them and this path never runs
//!    `git clean` — there is nothing for them to be rescued *from*.
//! 4. Only then `git reset --hard <target>`.
//!
//! # Why this family
//!
//! It is the remaining **named data-loss class** in `worktree.sh`: *"rescue
//! foreign work instead of discarding it on a raced reset/remove"* (#6706),
//! from the #6320 incident where an unqualified `git reset --hard` silently
//! discarded a second builder's work. The other two the issue leads with are
//! already ported — the orphan guard's `rm -rf` in slice 5 (#7858/#7849) and
//! the `remove` verb in slice 3 — and the lock's ownership check (#6017) is
//! implemented and tested in [`super::lock`], awaiting a delegation that
//! answers #8226's CI blast radius.
//!
//! It is also the one place the issue's *"two implementations of 'is this
//! worktree safe to touch'"* observation is literally true. The shell's own
//! header claimed its liveness probe's *"probe order and posture mirror the
//! Rust twin exactly (`worktree_ops::safety::find_processes_using_directory`)"*
//! — and they did not: the Rust twin has matched **any open file descriptor**
//! under the directory since #7466, while the shell matched cwd only. Mirroring
//! is what drifts. There is now one `/proc` walk and one `lsof` parse in this
//! codebase, reached through
//! [`safety::find_processes_with_cwd_in_directory`] with a flag; see that
//! function for why this call site kept the narrower signal instead of
//! inheriting the wider one.
//!
//! # Exit codes — the shell's three, unchanged
//!
//! | code | meaning |
//! |---|---|
//! | 0 | the reset landed (after a rescue, if one was needed) |
//! | 1 | **refused** — nothing was attempted, the worktree is untouched |
//! | 2 | the reset itself failed (bad ref, git error); any rescue already succeeded |
//!
//! All three are *answers* the caller branches on, which is what makes the
//! degraded cases below worth arguing rather than defaulting.
//!
//! **A missing binary returns 1, not 2.** The shell wrapper that resolves this
//! binary makes that choice, and it is the only truthful one available: the
//! worktree was not reset and nothing was changed, which is exactly what 1
//! says. 0 would claim a reset that never happened, and `worktree.sh` would
//! hand a Judge or Doctor a worktree it believes is at the base ref. 2 would
//! claim a reset was attempted and failed — also false, and it is the code an
//! operator reads as "git refused", sending them to look at the ref. This is
//! the one place in epic #7810 where the "could not run at all" code is *not*
//! 2, precisely because 2 is already spoken for here.
//!
//! That degradation is safe in the direction that matters: `worktree.sh`'s call
//! site treats any non-zero as *"Could not reset stale worktree (continuing to
//! use as-is)"* and exits 0, which is the pre-#6334 behaviour — a worktree left
//! alone. No `loom-daemon` on the host therefore costs a reset, never data.
//! (Contrast slice 1, reverted in #8226 for making the always-taken create path
//! hard-depend on a built binary: this rung is reached only when the worktree
//! directory already exists *and* is stale, and its absence degrades instead of
//! failing.)
//!
//! A malformed invocation exits 2 via clap rather than 1, for the same reason
//! [`super::cleanup`] accepts clap's usage error: the only caller is a generated
//! command line inside a shell library, no human types it, and 2 lands on the
//! same non-destructive "could not reset" arm.
//!
//! # Behaviour deliberately preserved
//!
//! - **Every message goes to stderr**, verbatim, including the
//!   `loom_worktree_reset_or_rescue:` / `loom_worktree_has_live_process:`
//!   prefixes. They name shell functions that still exist as the entry points,
//!   role logs are greppable by them, and `test-worktree-race-rescue.sh`
//!   asserts on one of them.
//! - **The worktree path is reported exactly as passed**, not canonicalised.
//!   The liveness probe resolves it physically (the shell's `cd … && pwd -P`)
//!   but the messages quote the caller's spelling, which is what an operator
//!   can copy.
//! - `rev-list --count` failing is read as `"0"` (the shell's
//!   `|| ahead="0"`), so a bad target ref reaches the reset and is reported by
//!   git as exit 2 rather than being pre-empted as "gained commits".
//! - The rescue patch is written and then required to be **non-empty**; an
//!   empty or failed write is deleted and refuses the reset. A patch that
//!   cannot be replayed is not a rescue.
//! - `git diff HEAD --quiet` exit >1 is a *refusal*, not a "clean" reading:
//!   not knowing whether there is work to lose is a reason to keep it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::worktree_ops::safety::{self, CwdProbe};

/// What to reset, and what to call the rescue patch if one is needed.
pub struct Options {
    /// The worktree to reset. Reported in every message exactly as given.
    pub worktree: PathBuf,
    /// The ref to reset to (`origin/main`, a SHA, `feature/issue-41`, …).
    pub target_ref: String,
    /// Filename stem for a rescue patch. `worktree.sh` passes
    /// `issue-<N>-stale-worktree-reset`; the shell's default was
    /// `loom-race-rescue` and the CLI keeps it.
    pub rescue_label: String,
    /// PIDs the liveness probe must not count as foreign holders — the calling
    /// shell's own `$$`/`$BASHPID`.
    ///
    /// The shell excluded exactly those two inline, because its probe ran
    /// *inside* the process that had called it. Here the probe runs in a child,
    /// so the caller's PID is no longer self-evident and has to be passed. It
    /// is not optional bookkeeping: `test-worktree-race-rescue.sh` drives the
    /// function from a shell whose cwd is the worktree under test, and without
    /// this every one of its reset cases would be refused by the harness's own
    /// shell.
    pub ignore_pids: Vec<u32>,
}

/// Run the guard. Returns the process exit code — see the module docs for what
/// each of 0/1/2 means.
pub fn run(opts: &Options) -> i32 {
    let wt = opts.worktree.as_path();
    let shown = wt.display();

    // --- 1. Live holder (#7463) --------------------------------------------
    if has_live_process(wt, &opts.ignore_pids) {
        eprintln!(
            "loom_worktree_reset_or_rescue: refusing to reset {shown} — a live process still has it open (cwd inside the worktree); leaving it untouched instead of discarding its in-progress tracked edits"
        );
        return 1;
    }

    // --- 2. Commits gained since the caller's staleness check --------------
    // `ahead="$(git -C "$wt" rev-list --count "$target..HEAD")" || ahead="0"`.
    // A failure reads as "0", deliberately: the shell could not distinguish a
    // bad ref from a clean count, and pre-empting the reset here would report
    // "gained commits" for what is really an unresolvable ref. Letting it
    // through means `git reset --hard` answers, and exit 2 names the real
    // problem.
    let ahead =
        match git_stdout(wt, &["rev-list", "--count", &format!("{}..HEAD", opts.target_ref)]) {
            Some(out) => out,
            None => "0".to_string(),
        };
    if ahead != "0" {
        eprintln!(
            "loom_worktree_reset_or_rescue: refusing to reset {shown} to {} — it gained {ahead} commit(s) ahead since the staleness check; leaving it untouched instead of discarding them",
            opts.target_ref
        );
        return 1;
    }

    // --- 3. Foreign uncommitted TRACKED changes ---------------------------
    // `git diff HEAD --quiet` exits 1 when tracked content differs from HEAD
    // (staged or unstaged), 0 when it does not, and >1 on a genuine git error.
    // This is exactly what `git reset --hard` is about to discard; untracked
    // files are excluded because it never touches them.
    let diff_status = git_status_code(wt, &["diff", "HEAD", "--quiet"]);
    if diff_status > 1 {
        eprintln!(
            "loom_worktree_reset_or_rescue: refusing to reset {shown} — could not determine its tracked-diff state against HEAD (git diff exit {diff_status})"
        );
        return 1;
    }

    if diff_status == 1 {
        let rescue_dir = wt.join(".snapshots");
        // `mkdir -p` — succeeds on an existing directory, fails when a
        // non-directory (the retained suite pre-creates a regular FILE there)
        // occupies the path.
        if std::fs::create_dir_all(&rescue_dir).is_err() || !rescue_dir.is_dir() {
            eprintln!(
                "loom_worktree_reset_or_rescue: refusing to reset {shown} — could not create rescue directory {} for its foreign tracked changes",
                rescue_dir.display()
            );
            return 1;
        }

        let patch_path = rescue_dir.join(format!(
            "{}-{}.patch",
            opts.rescue_label,
            utc_stamp(std::time::SystemTime::now())
        ));

        if !write_rescue_patch(wt, &patch_path) {
            // `rm -f "$patch_path"`: a partial or empty capture is not a
            // rescue, and leaving one behind would look like one.
            let _ = std::fs::remove_file(&patch_path);
            eprintln!(
                "loom_worktree_reset_or_rescue: refusing to reset {shown} — failed writing its foreign tracked changes to a rescue patch"
            );
            return 1;
        }

        let patch = patch_path.display();
        eprintln!(
            "loom_worktree_reset_or_rescue: rescued foreign tracked changes in {shown} to {patch} before resetting to {} (replay with: git apply {patch})",
            opts.target_ref
        );
    }

    // --- 4. The reset ------------------------------------------------------
    if git_status_code(wt, &["reset", "--hard", &opts.target_ref]) == 0 {
        return 0;
    }
    2
}

/// `git -C "$wt" diff HEAD > "$patch_path"`, plus the shell's own two
/// post-conditions: the write succeeded, and the file is non-empty (`! -s`).
///
/// Returns whether the patch is a usable rescue.
fn write_rescue_patch(worktree: &Path, patch_path: &Path) -> bool {
    let Ok(file) = std::fs::File::create(patch_path) else {
        return false;
    };
    let ok = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "HEAD"])
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return false;
    }
    std::fs::metadata(patch_path).is_ok_and(|m| m.len() > 0)
}

/// The liveness veto (#7463), with the shell's exact posture: an unprobable
/// host contributes **nothing** and says so on stderr, rather than refusing
/// every reset (which is what an `lsof`-only probe did on a CI runner, silently
/// disabling the rescue path there).
fn has_live_process(worktree: &Path, ignore_pids: &[u32]) -> bool {
    match safety::find_processes_with_cwd_in_directory(worktree) {
        CwdProbe::Pids(pids) => pids.iter().any(|pid| !ignore_pids.contains(pid)),
        CwdProbe::Unprobable => {
            eprintln!(
                "loom_worktree_has_live_process: no process probe available on this host (no /proc, no lsof) -- cannot verify liveness of {}; contributing no evidence (matches worktree_in_use()'s unknown-is-empty contract)",
                worktree.display()
            );
            false
        }
    }
}

/// `$(git -C "$wt" … 2>/dev/null)` — trimmed stdout on success, `None` when git
/// exits non-zero or could not run, so the caller can apply the shell's own
/// `|| default`.
fn git_stdout(worktree: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Command substitution strips trailing newlines; nothing else.
    Some(
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    )
}

/// The exit code of `git -C "$wt" … >/dev/null 2>&1`. A git that cannot be
/// spawned answers 127, which is what bash reports for the same failure — and
/// is >1, so both call sites read it as an error rather than as data.
fn git_status_code(worktree: &Path, args: &[&str]) -> i32 {
    Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(127, |s| s.code().unwrap_or(127))
}

/// `date -u +%Y%m%dT%H%M%SZ`, the rescue patch's timestamp.
///
/// Hand-rolled from the Unix epoch rather than pulled from `chrono` so this
/// module has no new dependency and the format is visible in one place; the
/// civil-date arithmetic is the standard days-from-epoch algorithm.
fn utc_stamp(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests;
