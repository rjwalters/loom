//! Agent work state: what a dispatched agent actually left behind (Issue #8267).
//!
//! # The failure this exists to make impossible
//!
//! An agent can finish a long investigation, report its findings in prose, and
//! leave its entire deliverable **uncommitted** — and its completion
//! notification is byte-for-byte indistinguishable from one where everything
//! was committed and pushed. Two reported incidents in one session: a branch
//! with zero commits whose six probe scripts survived only because the files
//! happened to still be on disk, and a 52 KB tool left untracked with a
//! further fix left uncommitted *after* the agent reported done.
//!
//! Loom does not control the harness's own `<usage>` completion block, so the
//! literal fix ("add `branch: N commits, worktree: M uncommitted files` to it")
//! is not ours to make. What Loom does control is its own worktrees and its own
//! turn-end hook surface. This module is the primitive behind both:
//!
//! - `loom-daemon worktree-state report` prints that exact line for a managed
//!   worktree, for any caller that wants to state repo state rather than assert
//!   completion.
//! - `loom-daemon worktree-state stop-hook` consumes a `Stop`/`SubagentStop`
//!   hook payload and **blocks the turn from ending once** when the session's
//!   own worktree holds uncommitted deliverables — the "refuses to report
//!   success with zero tests collected" shape, applied to commits.
//!
//! # Why the counting lives here and not in a second place
//!
//! `buildGate`'s documented **has-commits** check
//! (`git rev-list --count origin/main..HEAD > 0`, see
//! `defaults/docs/build-gate.md`) is the same primitive. It is documented as an
//! orchestrator-side check and had no executable implementation anywhere in the
//! tree; [`collect`] is that implementation, so anything else needing the
//! question answered calls this rather than growing a second commit-counter
//! with its own drift (Issue #8267 AC4).
//!
//! # What counts as a deliverable
//!
//! The same scratch exclusions `buildGate` documents — `.loom-*` runtime
//! markers, `*.log`, and the `.no-changes-needed` no-op signal. A Builder that
//! deliberately concluded "no changes needed" leaves exactly one untracked
//! marker file and must not be flagged for it; a Builder that leaves 725 lines
//! of untracked probe scripts must be.

use std::path::{Path, PathBuf};

pub mod stop_hook;

/// Cap on the number of at-risk paths carried in a verdict (a blocked turn's
/// reason has to be readable, and a worktree with 400 untracked files is
/// exactly as actionable at 8 named paths as at 400).
pub const MAX_REPORTED_PATHS: usize = 8;

/// The sentinel `worktree.sh` drops into every Loom-managed worktree. Nothing
/// here ever inspects a directory that does not carry it: the primary checkout
/// legitimately holds an operator's uncommitted work, and flagging that would
/// be noise at best and a wedged session at worst.
pub const MANAGED_SENTINEL: &str = ".loom-managed";

/// What the worktree's contents say about whether the agent's work survives
/// the turn ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing on the branch and nothing in the worktree. Not a loss: this is
    /// the shape of a deliberate no-op (`.no-changes-needed`) or a session that
    /// genuinely had nothing to write.
    Empty,
    /// Commits exist and the branch is published. The normal done state.
    Committed,
    /// Commits exist but no remote branch carries them. Recoverable locally,
    /// so it is reported and never blocked on.
    Unpushed,
    /// Deliverable-shaped changes are sitting in the worktree uncommitted.
    /// This is the reported failure.
    Uncommitted,
}

impl Verdict {
    /// The stable token used in the `verdict=` field and in JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Empty => "empty",
            Verdict::Committed => "committed",
            Verdict::Unpushed => "unpushed",
            Verdict::Uncommitted => "uncommitted",
        }
    }

    /// Process exit code for `report`. `3` mirrors `check-main-clean.sh`'s
    /// "there is something here you did not mean to leave" code rather than a
    /// bare `1`, which a caller cannot distinguish from "the command failed".
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Uncommitted => 3,
            _ => 0,
        }
    }
}

/// Whether the branch's commits exist anywhere but this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushState {
    /// A remote-tracking ref exists and contains HEAD.
    Pushed,
    /// A remote-tracking ref exists but is behind HEAD.
    Behind,
    /// No remote-tracking ref for this branch.
    Absent,
    /// The question could not be answered (no git, detached, command failed).
    /// Never merged into `Absent`: "we do not know" is not "no".
    Unknown,
}

impl PushState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PushState::Pushed => "yes",
            PushState::Behind => "behind",
            PushState::Absent => "no",
            PushState::Unknown => "unknown",
        }
    }

    /// The same fact in words, for prose a human or an agent reads.
    ///
    /// [`as_str`](Self::as_str) is a machine token in a `key=value` line, where
    /// `pushed=no` reads correctly. Dropped into a sentence it does not: an
    /// early draft of this guard rendered "1 commit(s) ahead (no)", which
    /// states nothing a reader can act on. The whole point of the feature is a
    /// legible completion notification, so the sentence form spells it out.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            PushState::Pushed => "pushed",
            PushState::Behind => "NOT pushed — origin does not have these commits",
            PushState::Absent => "NOT pushed — no remote branch",
            PushState::Unknown => "push state unverifiable",
        }
    }
}

/// The measured state of one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeState {
    /// Absolute path of the worktree that was measured.
    pub path: PathBuf,
    /// Branch name, or `None` when detached / unreadable.
    pub branch: Option<String>,
    /// Commits on HEAD that the base ref does not have.
    pub commits_ahead: u32,
    /// Tracked files with staged or unstaged modifications, after scratch
    /// filtering.
    pub uncommitted: u32,
    /// Untracked, non-ignored files, after scratch filtering.
    pub untracked: u32,
    /// Scratch/runtime-marker paths that were deliberately ignored. Reported
    /// so a caller can see the filter worked rather than guess.
    pub scratch_ignored: u32,
    pub push_state: PushState,
    /// Up to [`MAX_REPORTED_PATHS`] of the deliverable-shaped paths that are
    /// not committed, in `git status` order.
    pub at_risk_paths: Vec<String>,
    pub verdict: Verdict,
}

impl WorktreeState {
    /// The one-line completion-notification form: exactly the
    /// "branch: N commits, worktree: M uncommitted files" the issue asks for,
    /// in this repo's `key=value` diagnostic shape.
    #[must_use]
    pub fn render_line(&self) -> String {
        format!(
            "worktree-state: path={} branch={} commits_ahead={} uncommitted={} untracked={} pushed={} verdict={}",
            self.path.display(),
            self.branch.as_deref().unwrap_or("(detached)"),
            self.commits_ahead,
            self.uncommitted,
            self.untracked,
            self.push_state.as_str(),
            self.verdict.as_str(),
        )
    }

    /// Prose form for a human or an agent reading a blocked turn's reason.
    #[must_use]
    pub fn render_sentence(&self) -> String {
        let branch = self.branch.as_deref().unwrap_or("(detached HEAD)");
        let at_risk = self.uncommitted + self.untracked;
        format!(
            "branch {branch}: {} commit(s) ahead, {}; worktree: {at_risk} uncommitted file(s) ({} modified, {} untracked)",
            self.commits_ahead,
            self.push_state.describe(),
            self.uncommitted,
            self.untracked,
        )
    }

    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path.display().to_string(),
            "branch": self.branch,
            "commits_ahead": self.commits_ahead,
            "uncommitted": self.uncommitted,
            "untracked": self.untracked,
            "scratch_ignored": self.scratch_ignored,
            "pushed": self.push_state.as_str(),
            "at_risk_paths": self.at_risk_paths,
            "verdict": self.verdict.as_str(),
        })
    }

    /// True when work exists that only this machine's working tree holds.
    #[must_use]
    pub fn work_at_risk(&self) -> bool {
        self.verdict == Verdict::Uncommitted
    }
}

/// Decide the verdict from the four measured quantities.
///
/// Pure, so the precedence is testable without a git fixture: uncommitted
/// deliverables outrank everything (they are the loss), then unpublished
/// commits, then published commits, then nothing.
#[must_use]
pub fn classify(commits_ahead: u32, uncommitted: u32, untracked: u32, push: PushState) -> Verdict {
    if uncommitted + untracked > 0 {
        return Verdict::Uncommitted;
    }
    if commits_ahead == 0 {
        return Verdict::Empty;
    }
    match push {
        PushState::Pushed => Verdict::Committed,
        // `Unknown` deliberately reports as `Unpushed` rather than `Committed`:
        // claiming work is published when that could not be verified is the
        // exact "existence treated as evidence of a property" shape this issue
        // cites (#8265). Neither outcome blocks, so the cost is a truthful word.
        PushState::Behind | PushState::Absent | PushState::Unknown => Verdict::Unpushed,
    }
}

/// Whether a `git status` path is scratch rather than a deliverable.
///
/// The list is `buildGate`'s documented default scratch exclusions
/// (`defaults/docs/build-gate.md`): Loom runtime markers, logfiles, and the
/// no-changes-needed signal. Matching is on the final path component, so a
/// nested `logs/run.log` is excluded exactly like a top-level one.
#[must_use]
pub fn is_scratch_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.starts_with(".loom-") || name == ".no-changes-needed" {
        return true;
    }
    if name.ends_with(".log") {
        return true;
    }
    // The per-worktree WIP patches `worktree.sh snapshot` writes: a snapshot is
    // a rescue artifact of uncommitted work, never the work itself.
    path.starts_with(".snapshots/") || path.contains("/.snapshots/")
}

/// Parse `git status --porcelain=v1 -z` output into (tracked-modified,
/// untracked, scratch-ignored, at-risk paths).
///
/// NUL-delimited on purpose: porcelain v1's space-delimited form quotes and
/// escapes paths with spaces or unicode, and a parser that splits on
/// whitespace silently miscounts exactly the filenames an agent is most likely
/// to write. Rename entries (`R`) carry two NUL-separated fields; the second is
/// the origin path and is consumed, not counted twice.
#[must_use]
pub fn parse_status_z(raw: &str) -> (u32, u32, u32, Vec<String>) {
    let mut modified = 0u32;
    let mut untracked = 0u32;
    let mut scratch = 0u32;
    let mut at_risk: Vec<String> = Vec::new();

    let mut fields = raw.split('\0');
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            // A porcelain entry is `XY <path>`; anything shorter is the
            // trailing empty field from the final NUL.
            continue;
        }
        let (status, path) = entry.split_at(3);
        let status = status.as_bytes();
        let (x, y) = (status[0] as char, status[1] as char);

        // A rename/copy entry's origin path follows in its own field.
        if x == 'R' || x == 'C' {
            let _ = fields.next();
        }

        if is_scratch_path(path) {
            scratch += 1;
            continue;
        }

        if x == '?' && y == '?' {
            untracked += 1;
        } else {
            modified += 1;
        }
        if at_risk.len() < MAX_REPORTED_PATHS {
            at_risk.push(path.to_string());
        }
    }

    (modified, untracked, scratch, at_risk)
}

/// Is `path` a Loom-managed worktree (a directory carrying the sentinel)?
#[must_use]
pub fn is_managed_worktree(path: &Path) -> bool {
    path.join(MANAGED_SENTINEL).exists()
}

/// The main checkout that owns `worktree` — the parent of the git *common*
/// dir, so a linked worktree resolves to the primary clone rather than to
/// itself.
///
/// Config must be read from there, not from the worktree: the host-local
/// override tier (`.loom-local/local.json`) is gitignored and so exists **only**
/// in the main checkout, and an uncommitted edit to the main checkout's
/// `.loom/config.json` is likewise invisible from a worktree checked out before
/// it. Resolving against the worktree would silently ignore an operator who had
/// turned this guard off — a toggle that does not toggle is worse than no
/// toggle. `forge_cmd` hit the same main-checkout-only config trap (#4273).
///
/// `None` when the question cannot be answered (no git, not a repository);
/// callers fall back to the worktree, which is still a Loom workspace.
#[must_use]
pub fn main_checkout_root(worktree: &Path) -> Option<PathBuf> {
    use crate::script_helpers::run_git;

    let common = run_git(worktree, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .ok_stdout_trimmed()
        .filter(|s| !s.is_empty())?;
    Path::new(&common).parent().map(Path::to_path_buf)
}

/// Measure `worktree` against `base_ref`.
///
/// Every git question is asked independently and every failure degrades to the
/// least-alarming answer for that field — a host with no `git` on PATH reports
/// `Empty`/`Unknown` rather than blocking a turn. Nothing here mutates the
/// repository.
#[must_use]
pub fn collect(worktree: &Path, base_ref: &str) -> WorktreeState {
    use crate::script_helpers::run_git;

    // `branch --show-current` first: it is the only form that answers on an
    // UNBORN branch (a freshly-created worktree with no commit yet), which is
    // precisely the zero-commit shape this guard exists to catch — reporting
    // that session's branch as `(detached)` would misdescribe the incident in
    // the one message that has to be trusted. `rev-parse --abbrev-ref` is kept
    // as the fallback (it predates `--show-current`, git 2.22) and its literal
    // `HEAD` answer is filtered, since that means genuinely detached.
    let branch = run_git(worktree, &["branch", "--show-current"])
        .ok_stdout_trimmed()
        .filter(|b| !b.is_empty())
        .or_else(|| {
            run_git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])
                .ok_stdout_trimmed()
                .filter(|b| !b.is_empty() && b != "HEAD")
        });

    // `<base>..HEAD` needs the base ref to exist. A worktree whose origin has
    // never been fetched (or a fixture repo with no remote) answers 0 rather
    // than erroring, and the fallback chain tries the local base name next so
    // an offline clone still gets a real count.
    let commits_ahead = count_ahead(worktree, base_ref)
        .or_else(|| {
            base_ref
                .rsplit('/')
                .next()
                .filter(|local| *local != base_ref)
                .and_then(|local| count_ahead(worktree, local))
        })
        .unwrap_or(0);

    let status_raw = run_git(worktree, &["status", "--porcelain=v1", "-z"])
        .ok_output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let (uncommitted, untracked, scratch_ignored, at_risk_paths) = parse_status_z(&status_raw);

    let push_state = push_state(worktree, branch.as_deref());
    let verdict = classify(commits_ahead, uncommitted, untracked, push_state);

    WorktreeState {
        path: worktree.to_path_buf(),
        branch,
        commits_ahead,
        uncommitted,
        untracked,
        scratch_ignored,
        push_state,
        at_risk_paths,
        verdict,
    }
}

/// `git rev-list --count <base>..HEAD`, or `None` when the base ref does not
/// resolve (which is a different fact from "zero commits ahead").
fn count_ahead(worktree: &Path, base: &str) -> Option<u32> {
    use crate::script_helpers::run_git;

    let range = format!("{base}..HEAD");
    run_git(worktree, &["rev-list", "--count", &range])
        .ok_stdout_trimmed()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Whether `origin/<branch>` exists and already contains HEAD.
fn push_state(worktree: &Path, branch: Option<&str>) -> PushState {
    use crate::script_helpers::run_git;

    let Some(branch) = branch else {
        return PushState::Unknown;
    };
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let remote_sha = run_git(worktree, &["rev-parse", "--verify", "--quiet", &remote_ref])
        .ok_stdout_trimmed()
        .filter(|s| !s.is_empty());
    let Some(remote_sha) = remote_sha else {
        return PushState::Absent;
    };
    let head_sha = run_git(worktree, &["rev-parse", "HEAD"])
        .ok_stdout_trimmed()
        .filter(|s| !s.is_empty());
    let Some(head_sha) = head_sha else {
        return PushState::Unknown;
    };
    if head_sha == remote_sha {
        return PushState::Pushed;
    }
    // HEAD reachable from the remote ref means the remote is ahead (someone
    // else pushed on top) — still "your commits are published".
    match run_git(worktree, &["merge-base", "--is-ancestor", &head_sha, &remote_sha]) {
        outcome if outcome.succeeded() => PushState::Pushed,
        _ => PushState::Behind,
    }
}

#[cfg(test)]
#[path = "worktree_state/tests.rs"]
mod tests;
