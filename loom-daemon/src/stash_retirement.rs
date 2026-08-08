//! Auto-retirement classifier for `loom-quarantine:` git stashes (#5693,
//! sub-issue of #5690).
//!
//! `check-main-clean.sh --quarantine` rescues contamination it finds in the
//! primary clone's working tree into a labeled `git stash` entry rather than
//! discarding it (see that script's header) — but nothing ever reviews,
//! expires, or retires those stashes afterward. #5690's fleet-wide audit
//! found the accumulated population (148 stashes across three hosts, twelve
//! days) collapses cleanly once classified: 58 held no real work at all
//! (installer-managed paths, `.venv/`, `__pycache__`, `.egg-info`,
//! lockfiles), 48 more referenced an issue that had since closed because the
//! work landed by another route (the `compute-drift.sh` "superseded local
//! copy" case), and exactly one held genuinely unlanded engineering content
//! (`2AMLogic/gf180-ldo#51`'s abandoned device-sizing experiment).
//!
//! ## The two required conditions
//!
//! Dropping a stash is irrevocable in the same sense any `git stash drop` is,
//! and a quarantine stash is *by construction* the only copy of some
//! uncommitted work. So this module requires **two independent conditions,
//! both of them, never either alone**:
//!
//! 1. **Content check** ([`classify_stash_content`]): every path the stash
//!    touches is individually *provably* recoverable-without-the-stash —
//!    see [`PathVerdict`] for the four ways a path can prove that, each of
//!    which is a byte/object-identity or a pure-generated-artifact argument,
//!    never a heuristic about whether a change "looks important".
//! 2. **Provenance check** ([`IssueStateLookup`]): the issue named by the
//!    stash's `loom-quarantine:` label is CLOSED.
//!
//! Retiring on either alone is unsafe: a closed issue's stash can still hold
//! real unrecovered work (that is exactly the gf180-ldo#51 shape — the issue
//! moved on without the experiment), and harmless content proves nothing
//! about a still-open issue's stash. [`classify_stash`] enforces both.
//!
//! Everything else — an unparseable label, a missing `issue=` token, a forge
//! lookup that failed, a `git` invocation that failed, an empty change set —
//! resolves to [`RetireVerdict::Keep`]. There is no path through this module
//! on which "we could not tell" becomes "safe to drop".
//!
//! ## Irrevocability handling
//!
//! - [`plan_and_execute_retirement`] classifies and reports; it drops nothing
//!   unless called with `execute = true`, and even then only for a
//!   [`RetireVerdict::Retire`] verdict. The CLI wires `execute` to an explicit
//!   `--execute` flag, so the default invocation is always a dry run.
//! - Every drop is journaled to `.loom/logs/stash-retirement.log` **before**
//!   the `git stash drop` runs ([`append_retirement_log`]), recording the
//!   stash's commit sha and path list. A dropped stash commit stays in the
//!   object database as an unreachable object until it is gc'd, so the
//!   journaled sha is a real recovery handle (`git stash apply <sha>`), not
//!   just an audit trail.
//! - [`drop_stash`] re-resolves the live `stash@{N}` selector from the
//!   entry's commit sha immediately before dropping, and treats an
//!   already-gone entry as a no-op — the operation is safely re-runnable.
//!
//! ## Cadence
//!
//! Deliberately **not** wired to a daemon timer. #5693 calls for an explicit
//! operator-invoked command first ("with automatic cadence as a possible
//! fast-follow once the classifier has a track record"), so the only entry
//! point is `loom-daemon stashes list` / `loom-daemon stashes retire`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::main_health_gate::is_ignorable_dirt_with_readers;
use crate::quarantine_stash_status::QUARANTINE_STASH_LABEL;

/// How far back through a path's history [`superseding_commit`] will look for
/// a commit whose blob at that path is byte-identical to the stash's. Bounded
/// so one pathological path (a file touched by thousands of commits) cannot
/// stall the whole scan; exceeding the bound simply means the path is not
/// *proven* superseded, which resolves to Keep.
const HISTORY_SCAN_COMMIT_LIMIT: usize = 500;

// ============================================================================
// Enumeration & parsing
// ============================================================================

/// A single `loom-quarantine:` labeled entry parsed out of the stash reflog
/// (`git log -g --format=... refs/stash`), mirroring
/// `check-quarantine-stashes.sh`'s enumeration (#5185).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuarantineStashEntry {
    /// The `stash@{N}` selector at parse/list time. **Not** a durable
    /// identity — dropping any stash renumbers every older entry, so
    /// [`drop_stash`] always re-resolves the current selector from
    /// [`commit`](Self::commit) immediately before dropping, never trusting
    /// this field directly.
    pub stash_ref: String,
    /// The stash entry's own commit sha — durable across index churn from
    /// concurrent pushes/drops on the shared `refs/stash` stack, and the
    /// recovery handle journaled before any drop.
    pub commit: String,
    /// Human-readable relative age (`%cr`), cosmetic only.
    pub age: String,
    /// The full `loom-quarantine: ...` label text (the `On <branch>: `
    /// prefix `git stash list` prepends is stripped).
    pub label: String,
    /// The `issue=<N>` token parsed out of `label`, if present.
    pub issue: Option<u64>,
    /// The `run=<id>` token parsed out of `label`, if present.
    pub run_id: Option<String>,
}

/// Parse `git log -g --format='%gd|%H|%cr|%gs' refs/stash` output (the
/// `%gd|%cr|%gs` triple `check-quarantine-stashes.sh` reads, with `%H`
/// inserted for a durable identity — see [`QuarantineStashEntry::commit`])
/// into the `loom-quarantine:` subset, in reflog order (newest first). Any
/// non-quarantine stash (Auditor's drift shelf, a Judge park stash, ad-hoc
/// WIP) is silently dropped — this function only ever returns entries this
/// module is allowed to reason about retiring.
#[must_use]
pub fn parse_quarantine_reflog(reflog: &str) -> Vec<QuarantineStashEntry> {
    reflog
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '|');
            let stash_ref = parts.next()?.to_string();
            let commit = parts.next()?.to_string();
            let age = parts.next()?.to_string();
            let subject = parts.next()?;
            // subject looks like "On main: loom-quarantine: issue=5388" (or a
            // detached-HEAD "WIP on <sha>: loom-quarantine: ..."). Strip
            // everything up through the "loom-quarantine:" marker itself —
            // the same marker `check-quarantine-stashes.sh` greps for and
            // #5692's status surface counts.
            let idx = subject.find(QUARANTINE_STASH_LABEL)?;
            let label = subject[idx..].trim().to_string();
            let (issue, run_id) = parse_label_tokens(&label);
            Some(QuarantineStashEntry {
                stash_ref,
                commit,
                age,
                label,
                issue,
                run_id,
            })
        })
        .collect()
}

/// Parse the `issue=<N>` and `run=<id>` tokens out of a `loom-quarantine:
/// ...` label. `check-main-clean.sh --label`'s contract is whitespace-separated
/// `key=value` tokens after the marker; both the older `issue=<N>`-only form
/// and the newer `run=<id> issue=<N>` form parse here, and a label with
/// neither (`loom-quarantine: unattributed`) yields `(None, None)`, which
/// [`classify_stash`] treats as unverifiable provenance.
fn parse_label_tokens(label: &str) -> (Option<u64>, Option<String>) {
    let mut issue = None;
    let mut run_id = None;
    for token in label.split_whitespace() {
        if let Some(v) = token.strip_prefix("issue=") {
            issue = v.parse::<u64>().ok();
        } else if let Some(v) = token.strip_prefix("run=") {
            run_id = Some(v.to_string());
        }
    }
    (issue, run_id)
}

/// Enumerate every `loom-quarantine:` stash in `repo_root`'s reflog. Returns
/// an empty vec (not an error) when `refs/stash` does not exist at all,
/// mirroring `check-quarantine-stashes.sh`'s treatment of "no stashes" as the
/// normal steady state rather than a failure.
pub fn list_quarantine_stashes(repo_root: &Path) -> Result<Vec<QuarantineStashEntry>, String> {
    let verify = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "refs/stash"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to spawn `git rev-parse`: {e}"))?;
    if !verify.status.success() {
        return Ok(Vec::new());
    }
    let output = Command::new("git")
        .args(["log", "-g", "--format=%gd|%H|%cr|%gs", "refs/stash"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to spawn `git log -g`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git log -g refs/stash` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_quarantine_reflog(&String::from_utf8_lossy(&output.stdout)))
}

// ============================================================================
// Condition 1: content check
// ============================================================================

/// Path components that are unambiguously machine-generated: a virtualenv, a
/// dependency tree, or an interpreter/tool cache. These normally never reach
/// a stash at all (they are gitignored, and `git stash push --include-untracked`
/// does not stash ignored files) — they land in a quarantine stash only in a
/// repo that forgot to ignore them, which is exactly #5690's worst case (one
/// stash of 1,749 files, 1,743 of them `.venv/` and `__pycache__`).
///
/// Deliberately **narrower** than "things a `.gitignore` usually lists":
/// `dist/`, `build/`, `out/`, and `target/` are excluded because each is a
/// plausible hand-authored source directory in some project, and a false
/// "generated" call here deletes real work. Every entry below is a name no
/// project authors by hand.
///
/// This class is local to stash retirement and is **not** added to
/// [`crate::main_health_gate`]'s dirty-tree ignore list: that list decides
/// whether the gate may hard-reset a live working tree, a different question
/// with a different blast radius.
const GENERATED_ARTIFACT_COMPONENTS: &[&str] = &[
    "__pycache__",
    ".venv",
    "venv",
    "node_modules",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".ipynb_checkpoints",
];

/// Suffixes of a whole path component that mark it generated — `.egg-info`
/// directories (setuptools metadata) are named `<pkg>.egg-info`, so they need
/// a suffix match rather than an exact one.
const GENERATED_ARTIFACT_COMPONENT_SUFFIXES: &[&str] = &[".egg-info"];

/// Basename suffixes that mark a single generated file (compiled Python
/// bytecode) rather than a whole directory.
const GENERATED_ARTIFACT_FILE_SUFFIXES: &[&str] = &[".pyc", ".pyo"];

/// Exact basenames that are always OS/tooling droppings.
const GENERATED_ARTIFACT_BASENAMES: &[&str] = &[".DS_Store"];

/// Whether `path` is provably machine-generated content
/// ([`GENERATED_ARTIFACT_COMPONENTS`] and friends) — the "no real work at
/// all" class that made up 58 of #5690's 148 stashes.
#[must_use]
fn is_generated_artifact(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    if GENERATED_ARTIFACT_BASENAMES.contains(&basename)
        || GENERATED_ARTIFACT_FILE_SUFFIXES
            .iter()
            .any(|s| basename.ends_with(s))
    {
        return true;
    }
    path.split('/').any(|component| {
        GENERATED_ARTIFACT_COMPONENTS.contains(&component)
            || GENERATED_ARTIFACT_COMPONENT_SUFFIXES
                .iter()
                .any(|s| component.ends_with(s))
    })
}

/// Why one path inside a stash is (or is not) safe to lose along with the
/// stash. Every "safe" variant is an identity or provenance *proof*, never a
/// judgement about importance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PathVerdict {
    /// The stash's blob for this path is the same object as `HEAD`'s — the
    /// content is already checked in. Dropping loses nothing.
    IdenticalToHead,
    /// The stash's blob for this path is the same object as some commit
    /// reachable from `HEAD` recorded at that path. The exact bytes are
    /// preserved in history (recoverable with `git show <commit>:<path>`),
    /// so dropping the stash loses nothing even though `HEAD` has since
    /// moved on. This is #5690's `compute-drift.sh` "superseded local copy"
    /// shape.
    SupersededInHistory { commit: String },
    /// Installer-managed / regenerable dirt per
    /// [`crate::main_health_gate`]'s `is_ignorable_dirt` (#4332/#3950/#4239):
    /// a Loom-owned transient path, a known lockfile basename, the
    /// re-stamped install manifest, or an installed-surface copy whose bytes
    /// match its committed `defaults/` source.
    IgnorableDirt,
    /// Machine-generated content per [`is_generated_artifact`].
    GeneratedArtifact,
    /// None of the above proofs applies. Not necessarily precious — just not
    /// provably recoverable, which is the only thing this module is allowed
    /// to act on.
    NotProvenRecoverable,
}

impl PathVerdict {
    /// Whether this path may be lost with the stash.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        !matches!(self, PathVerdict::NotProvenRecoverable)
    }
}

/// Verdict of the content-only check ([`classify_stash_content`]) — the
/// first of the two required conditions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ContentVerdict {
    /// Every changed path carries a recoverability proof.
    Safe { paths: Vec<(String, PathVerdict)> },
    /// At least one changed path is not provably recoverable — never safe to
    /// retire regardless of provenance. One such path taints the whole stash:
    /// `git stash drop` is all-or-nothing, so a partially-superseded stash
    /// must survive intact. Carries *every* path's verdict, not just the
    /// blocking ones, so an operator triaging by hand can see how close the
    /// stash was to retirable.
    Unsafe { paths: Vec<(String, PathVerdict)> },
    /// The stash's change set could not be determined (a `git` failure, or an
    /// unexpectedly empty result) — treated as "not proven safe", never as
    /// "safe by default".
    Indeterminate { reason: String },
}

/// Run a `git` subcommand in `repo_root`, returning trimmed stdout on success.
fn git_stdout(repo_root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to spawn `git {}`: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// List the paths a stash touches, tracked and untracked (`-u`), via `git
/// stash show --name-only -u <ref>`. Deduplicated and sorted: a path can be
/// reported by both the tracked diff and the untracked parent, and a stable
/// order keeps reports diffable.
fn stash_changed_paths(repo_root: &Path, stash_ref: &str) -> Result<Vec<String>, String> {
    let stdout = git_stdout(repo_root, &["stash", "show", "--name-only", "-u", stash_ref])?;
    let unique: BTreeSet<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    Ok(unique.into_iter().collect())
}

/// Resolve `<treeish>:<path>` to a git object id, or `None` when the path
/// does not exist there (or `git` failed).
fn blob_id(repo_root: &Path, treeish: &str, path: &str) -> Option<String> {
    let spec = format!("{treeish}:{path}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &spec])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// Read `path`'s bytes as recorded by `<treeish>`, or `None` if absent there.
fn read_git_blob(repo_root: &Path, treeish: &str, path: &str) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(["show", &format!("{treeish}:{path}")])
        .current_dir(repo_root)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// The stash's own snapshot of `path`, as a git object id. Tracked changes
/// live in the stash commit's tree; untracked files pushed with `-u` live
/// only under the stash's third parent (`<ref>^3`), so that is the fallback —
/// mirrors how `git stash show -u` surfaces both classes.
fn stash_blob_id(repo_root: &Path, stash_ref: &str, path: &str) -> Option<String> {
    blob_id(repo_root, stash_ref, path)
        .or_else(|| blob_id(repo_root, &format!("{stash_ref}^3"), path))
}

/// The stash's own snapshot of `path`, as bytes (same tracked/untracked
/// fallback as [`stash_blob_id`]). Only needed for the installed-surface
/// byte-match, which compares against a `defaults/` counterpart path rather
/// than the same path.
fn read_stash_blob(repo_root: &Path, stash_ref: &str, path: &str) -> Option<Vec<u8>> {
    read_git_blob(repo_root, stash_ref, path)
        .or_else(|| read_git_blob(repo_root, &format!("{stash_ref}^3"), path))
}

/// Resolve a newline-separated batch of `<rev>:<path>` specs to object ids in
/// one `git cat-file --batch-check` pass. Returns `None` if git could not be
/// spawned; a spec that does not resolve yields a `<spec> missing` line, which
/// simply never matches the target blob.
fn git_batch_check(repo_root: &Path, query: &str) -> Option<String> {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(["cat-file", "--batch-check=%(objectname)"])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(query.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Find a commit reachable from `HEAD` whose blob at `path` is the same
/// object as `target_blob` — i.e. proof that the stash's exact bytes for that
/// path are already preserved in this repository's history and survive the
/// stash being dropped.
///
/// Bounded to the most recent [`HISTORY_SCAN_COMMIT_LIMIT`] commits that
/// touched `path`; beyond that the answer is simply "not proven". Paths
/// containing a newline are refused outright rather than risk a mis-split in
/// the `git cat-file --batch-check` stream (git quotes such paths in
/// `--name-only` output anyway, so this is belt-and-braces).
fn superseding_commit(repo_root: &Path, path: &str, target_blob: &str) -> Option<String> {
    if path.contains('\n') {
        return None;
    }
    let limit = HISTORY_SCAN_COMMIT_LIMIT.to_string();
    let commits =
        git_stdout(repo_root, &["rev-list", "--max-count", &limit, "HEAD", "--", path]).ok()?;
    let commits: Vec<&str> = commits.lines().filter(|l| !l.is_empty()).collect();
    if commits.is_empty() {
        return None;
    }
    // One `cat-file --batch-check` pass rather than one `rev-parse` per
    // commit: a long-lived path can have hundreds of touching commits and
    // this runs for every not-yet-proven path in every stash.
    let query: String = commits
        .iter()
        .map(|c| format!("{c}:{path}\n"))
        .collect::<Vec<_>>()
        .join("");
    let output = git_batch_check(repo_root, &query)?;
    output
        .lines()
        .zip(commits.iter())
        .find(|(line, _)| line.split_whitespace().next() == Some(target_blob))
        .map(|(_, commit)| (*commit).to_string())
}

/// Classify one path inside a stash. Ordered cheapest-proof-first: pure
/// string classes, then the two-object-id `HEAD` comparison, then the bounded
/// history walk. A `.venv/` path in a 1,700-file stash therefore costs no
/// subprocesses at all.
fn classify_path(repo_root: &Path, stash_ref: &str, path: &str) -> PathVerdict {
    if is_generated_artifact(path) {
        return PathVerdict::GeneratedArtifact;
    }
    if is_ignorable_dirt_with_readers(
        path,
        &mut |p| read_stash_blob(repo_root, stash_ref, p),
        &mut |p| read_git_blob(repo_root, "HEAD", p),
    ) {
        return PathVerdict::IgnorableDirt;
    }
    let Some(stash_blob) = stash_blob_id(repo_root, stash_ref, path) else {
        // The stash records a *deletion* of this path (or the blob could not
        // be resolved at all). A deletion is real intent we cannot prove is
        // reflected anywhere, so it is not retirable.
        return PathVerdict::NotProvenRecoverable;
    };
    if blob_id(repo_root, "HEAD", path).as_deref() == Some(stash_blob.as_str()) {
        return PathVerdict::IdenticalToHead;
    }
    if let Some(commit) = superseding_commit(repo_root, path, &stash_blob) {
        return PathVerdict::SupersededInHistory { commit };
    }
    PathVerdict::NotProvenRecoverable
}

/// The content-check condition (safety condition 1 from the module doc):
/// classify every path `stash_ref` touches, and declare the stash's content
/// safe only when *every* path carries a recoverability proof.
///
/// `HEAD` is read from `repo_root`'s current checkout. Callers resolve
/// `repo_root` to the primary clone (`refs/stash` is shared repo-wide, not
/// per-worktree), which is where `check-main-clean.sh --quarantine` created
/// the stash and is normally on `main`. A `HEAD` that is behind `origin/main`
/// only ever makes this check *more* conservative — a path whose work landed
/// upstream but has not been pulled yet simply fails to prove itself.
#[must_use]
pub fn classify_stash_content(repo_root: &Path, stash_ref: &str) -> ContentVerdict {
    let changed_paths = match stash_changed_paths(repo_root, stash_ref) {
        Ok(paths) => paths,
        Err(reason) => return ContentVerdict::Indeterminate { reason },
    };
    if changed_paths.is_empty() {
        // An empty change set here is far more likely an enumeration miss (or
        // a race with a concurrent drop) than a genuine zero-file stash — err
        // toward "not proven safe" rather than treating it as vacuously safe.
        return ContentVerdict::Indeterminate {
            reason: "stash change set is empty or could not be enumerated".to_string(),
        };
    }

    let verdicts: Vec<(String, PathVerdict)> = changed_paths
        .into_iter()
        .map(|path| {
            let verdict = classify_path(repo_root, stash_ref, &path);
            (path, verdict)
        })
        .collect();

    if verdicts.iter().all(|(_, v)| v.is_safe()) {
        ContentVerdict::Safe { paths: verdicts }
    } else {
        ContentVerdict::Unsafe { paths: verdicts }
    }
}

/// The subset of `paths` that blocked retirement, by name.
#[must_use]
pub fn blocking_paths(paths: &[(String, PathVerdict)]) -> Vec<&str> {
    paths
        .iter()
        .filter(|(_, v)| !v.is_safe())
        .map(|(p, _)| p.as_str())
        .collect()
}

// ============================================================================
// Condition 2: provenance check
// ============================================================================

/// Looks up whether a forge issue is closed — the provenance check (safety
/// condition 2). A trait so tests can fake it without shelling out to `gh`.
pub trait IssueStateLookup {
    /// `Some(true)` closed, `Some(false)` open, `None` unknown/lookup failed.
    /// Unknown must never be treated as "safe to retire" — see
    /// [`classify_stash`].
    fn is_closed(&mut self, issue: u64) -> Option<bool>;
}

/// Production [`IssueStateLookup`]: one `gh issue view --json state` call per
/// distinct issue, memoized so a repo with many stashes attributed to the
/// same issue costs one forge call, not one per stash.
pub struct GhIssueStateLookup {
    repo_root: PathBuf,
    cache: std::collections::HashMap<u64, Option<bool>>,
}

impl GhIssueStateLookup {
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            cache: std::collections::HashMap::new(),
        }
    }

    fn query(&self, issue: u64) -> Option<bool> {
        let output = Command::new("gh")
            .args([
                "issue",
                "view",
                &issue.to_string(),
                "--json",
                "state",
                "-q",
                ".state",
            ])
            .current_dir(&self.repo_root)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        match String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_uppercase()
            .as_str()
        {
            "CLOSED" => Some(true),
            "OPEN" => Some(false),
            _ => None,
        }
    }
}

impl IssueStateLookup for GhIssueStateLookup {
    fn is_closed(&mut self, issue: u64) -> Option<bool> {
        if let Some(cached) = self.cache.get(&issue) {
            return *cached;
        }
        let result = self.query(issue);
        self.cache.insert(issue, result);
        result
    }
}

// ============================================================================
// Combined classifier — both conditions required
// ============================================================================

/// The overall retirement verdict. Only [`RetireVerdict::Retire`] is ever
/// eligible for [`drop_stash`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum RetireVerdict {
    Retire {
        reason: String,
        /// Per-path proofs backing the retire decision, so a reviewer can
        /// audit *why* each file was considered recoverable.
        paths: Vec<(String, PathVerdict)>,
    },
    Keep {
        reason: String,
        /// Per-path verdicts when the content check ran, so a reviewer can
        /// see which paths blocked the retirement *and* which ones passed —
        /// a mostly-superseded stash held back by one file is the interesting
        /// case to triage by hand. Empty when the content check never ran
        /// (provenance failed first) or was indeterminate.
        paths: Vec<(String, PathVerdict)>,
    },
}

/// Classify one [`QuarantineStashEntry`] against both required conditions.
///
/// Order matters only for cost, not correctness: the provenance check is one
/// (memoized) `gh` call, while the content check can be several `git`
/// invocations per path, so provenance runs first and short-circuits the
/// still-open majority. Both must pass; neither alone ever retires.
pub fn classify_stash(
    repo_root: &Path,
    entry: &QuarantineStashEntry,
    issue_lookup: &mut dyn IssueStateLookup,
) -> RetireVerdict {
    let Some(issue) = entry.issue else {
        return RetireVerdict::Keep {
            reason: "no issue= token in the stash label — provenance cannot be verified"
                .to_string(),
            paths: Vec::new(),
        };
    };
    match issue_lookup.is_closed(issue) {
        Some(true) => {}
        Some(false) => {
            return RetireVerdict::Keep {
                reason: format!("issue #{issue} is still open"),
                paths: Vec::new(),
            };
        }
        None => {
            return RetireVerdict::Keep {
                reason: format!("could not determine the state of issue #{issue}"),
                paths: Vec::new(),
            };
        }
    }

    match classify_stash_content(repo_root, &entry.stash_ref) {
        ContentVerdict::Safe { paths } => RetireVerdict::Retire {
            reason: format!(
                "issue #{issue} is closed and all {} changed path(s) are provably recoverable \
                 without this stash",
                paths.len()
            ),
            paths,
        },
        ContentVerdict::Unsafe { paths } => {
            let blocking = blocking_paths(&paths);
            RetireVerdict::Keep {
                reason: format!(
                    "issue #{issue} is closed, but {} of {} path(s) are not provably recoverable \
                     without this stash: {}",
                    blocking.len(),
                    paths.len(),
                    blocking.join(", ")
                ),
                paths,
            }
        }
        ContentVerdict::Indeterminate { reason } => RetireVerdict::Keep {
            reason: format!("content check indeterminate: {reason}"),
            paths: Vec::new(),
        },
    }
}

// ============================================================================
// Retirement (the irrevocable step) — never reached without `execute = true`
// ============================================================================

/// Result of one drop attempt, for reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum DropOutcome {
    Dropped,
    /// Already gone — an idempotent no-op, not an error (#5693's "safely
    /// re-runnable" requirement).
    AlreadyGone,
    Failed {
        reason: String,
    },
}

/// Where [`append_retirement_log`] journals drops, relative to the repo root
/// — alongside `check-main-clean.sh --quarantine`'s own
/// `.loom/logs/main-quarantine.log`, in the same JSON-lines shape, so the
/// create side and the retire side of a stash's life read as one story.
pub const RETIREMENT_LOG_RELPATH: &str = ".loom/logs/stash-retirement.log";

/// Journal a retirement decision as one JSON line, **before** the drop it
/// describes. The recorded `stash_commit` is a working recovery handle for as
/// long as the object survives gc (`git stash apply <sha>`, or `git show
/// <sha>^3:<path>` for an untracked file), which is the only mitigation
/// available for an irrevocable operation.
///
/// Best effort: a log that cannot be written must not abort the run, but it
/// *is* surfaced to the caller so the CLI can warn rather than silently drop
/// unjournaled.
pub fn append_retirement_log(
    repo_root: &Path,
    entry: &QuarantineStashEntry,
    reason: &str,
    paths: &[String],
) -> Result<(), String> {
    let log_path = repo_root.join(RETIREMENT_LOG_RELPATH);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let record = serde_json::json!({
        "event": "stash-retirement.retire",
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "repo": repo_root.display().to_string(),
        "stash_commit": entry.commit,
        "stash_ref_at_classify": entry.stash_ref,
        "label": entry.label,
        "issue": entry.issue,
        "run": entry.run_id,
        "paths": paths,
        "reason": reason,
        "recover_with": format!("git stash apply {}", entry.commit),
    });
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("could not open {}: {e}", log_path.display()))?;
    writeln!(file, "{record}").map_err(|e| format!("could not write {}: {e}", log_path.display()))
}

/// Re-derive the current `stash@{N}` selector for a commit sha by re-reading
/// the live stash list — the only durable way to address a stash entry once
/// other drops (this run's earlier iterations, or a concurrent process on the
/// shared `refs/stash` stack) may have shifted indices.
fn current_stash_ref_for_commit(repo_root: &Path, commit: &str) -> Option<String> {
    let stdout = git_stdout(repo_root, &["log", "-g", "--format=%gd|%H", "refs/stash"]).ok()?;
    stdout.lines().find_map(|line| {
        let (r, c) = line.split_once('|')?;
        (c == commit).then(|| r.to_string())
    })
}

/// Drop `entry` — the irrevocable step. Always re-resolves the entry's
/// current `stash@{N}` selector from its commit sha immediately before
/// dropping (never trusts `entry.stash_ref` captured at classify/list time):
/// stash indices shift on every drop, and `refs/stash` is a stack shared
/// across every linked worktree of the repo, so another process may have
/// already dropped or reordered this exact entry. If the commit is no longer
/// in the stash list at all, that is [`DropOutcome::AlreadyGone`] — a no-op,
/// not an error.
pub fn drop_stash(repo_root: &Path, entry: &QuarantineStashEntry) -> DropOutcome {
    let Some(current_ref) = current_stash_ref_for_commit(repo_root, &entry.commit) else {
        return DropOutcome::AlreadyGone;
    };
    match Command::new("git")
        .args(["stash", "drop", &current_ref])
        .current_dir(repo_root)
        .output()
    {
        Ok(o) if o.status.success() => DropOutcome::Dropped,
        Ok(o) => DropOutcome::Failed {
            reason: String::from_utf8_lossy(&o.stderr).trim().to_string(),
        },
        Err(e) => DropOutcome::Failed {
            reason: format!("failed to spawn `git stash drop`: {e}"),
        },
    }
}

/// Parse the numeric index out of a `stash@{N}` selector — used only to order
/// the destructive pass below. Defaults to `0` (treated as newest) on a
/// malformed selector; never panics.
fn stash_index(stash_ref: &str) -> usize {
    stash_ref
        .strip_prefix("stash@{")
        .and_then(|s| s.strip_suffix('}'))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
}

/// One entry's classification (and, if `execute`d, drop outcome) — the shape
/// both the dry-run report and the real retirement run share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetirementReport {
    pub entry: QuarantineStashEntry,
    pub verdict: RetireVerdict,
    /// `None` in a dry run (`execute = false`) or for a `Keep` verdict — a
    /// drop is only ever attempted for a `Retire` verdict under `execute =
    /// true`.
    pub outcome: Option<DropOutcome>,
    /// A non-fatal problem journaling the drop to
    /// [`RETIREMENT_LOG_RELPATH`], if any. Surfaced so an operator knows a
    /// drop happened without its recovery handle recorded.
    pub log_error: Option<String>,
}

/// Classify every entry in `entries`, and — only when `execute` is `true` —
/// drop every [`RetireVerdict::Retire`] entry. `execute = false` (the default
/// every caller must opt out of explicitly) performs no drops at all.
///
/// Drops are processed oldest-first (descending `stash@{N}` index): dropping
/// stash `N` renumbers every *older* entry down by one but never touches
/// newer ones, so oldest-first is the only order in which each drop cannot
/// invalidate an index this same pass still has queued. [`drop_stash`]
/// re-resolves each selector from its commit sha anyway, so this ordering is
/// belt-and-braces, not the correctness argument.
pub fn plan_and_execute_retirement(
    repo_root: &Path,
    entries: &[QuarantineStashEntry],
    issue_lookup: &mut dyn IssueStateLookup,
    execute: bool,
) -> Vec<RetirementReport> {
    let mut classified: Vec<(QuarantineStashEntry, RetireVerdict)> = entries
        .iter()
        .map(|e| {
            let verdict = classify_stash(repo_root, e, issue_lookup);
            (e.clone(), verdict)
        })
        .collect();

    if execute {
        classified.sort_by_key(|(e, _)| std::cmp::Reverse(stash_index(&e.stash_ref)));
    }

    classified
        .into_iter()
        .map(|(entry, verdict)| {
            let mut outcome = None;
            let mut log_error = None;
            if execute {
                if let RetireVerdict::Retire { reason, paths } = &verdict {
                    // Journal first: an unrecorded drop is unrecoverable in
                    // practice even while the object still exists.
                    let path_names: Vec<String> = paths.iter().map(|(p, _)| p.clone()).collect();
                    if let Err(e) = append_retirement_log(repo_root, &entry, reason, &path_names) {
                        log_error = Some(e);
                    }
                    outcome = Some(drop_stash(repo_root, &entry));
                }
            }
            RetirementReport {
                entry,
                verdict,
                outcome,
                log_error,
            }
        })
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ---------- parsing ----------

    #[test]
    fn parse_quarantine_reflog_filters_to_labeled_entries_only() {
        let reflog = "\
stash@{0}|aaa111|4 hours ago|WIP on feature/issue-5654: 0e703af1 docs: update WORK_LOG\n\
stash@{1}|bbb222|23 hours ago|On feature/issue-5577: judge-5584: parking pre-existing staged reversion\n\
stash@{2}|ccc333|1 day ago|On main: auditor: stray package-lock.json diff before sync\n\
stash@{3}|ddd444|4 hours ago|On main: loom-quarantine: issue=5388\n\
stash@{4}|eee555|5 days ago|On main: loom-quarantine: run=sweep-20260804T023938Z-79774 issue=5187\n\
stash@{5}|fff666|5 days ago|On main: loom-quarantine: unattributed\n\
";
        let entries = parse_quarantine_reflog(reflog);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].stash_ref, "stash@{3}");
        assert_eq!(entries[0].commit, "ddd444");
        assert_eq!(entries[0].issue, Some(5388));
        assert_eq!(entries[0].run_id, None);
        assert_eq!(entries[0].label, "loom-quarantine: issue=5388");

        assert_eq!(entries[1].stash_ref, "stash@{4}");
        assert_eq!(entries[1].issue, Some(5187));
        assert_eq!(entries[1].run_id.as_deref(), Some("sweep-20260804T023938Z-79774"));

        assert_eq!(entries[2].stash_ref, "stash@{5}");
        assert_eq!(entries[2].issue, None);
        assert_eq!(entries[2].run_id, None);
    }

    #[test]
    fn parse_quarantine_reflog_empty_input_is_empty_output() {
        assert!(parse_quarantine_reflog("").is_empty());
    }

    // ---------- generated-artifact classification ----------

    #[test]
    fn generated_artifact_matches_the_no_real_work_class() {
        for path in [
            "sim/.venv/lib/python3.12/site-packages/numpy/__init__.py",
            "venv/bin/activate",
            "tools/__pycache__/helper.cpython-312.pyc",
            "src/thing.pyc",
            "web/node_modules/left-pad/index.js",
            "src/mypkg.egg-info/PKG-INFO",
            ".pytest_cache/v/cache/lastfailed",
            ".mypy_cache/3.12/foo.data.json",
            ".ruff_cache/content",
            "notebooks/.ipynb_checkpoints/x-checkpoint.ipynb",
            "docs/.DS_Store",
        ] {
            assert!(is_generated_artifact(path), "expected generated: {path}");
        }
    }

    #[test]
    fn generated_artifact_does_not_swallow_plausible_source_directories() {
        // `dist/`, `build/`, `out/`, `target/` are deliberately NOT in the
        // set: each is a real hand-authored source directory somewhere, and a
        // false "generated" call here deletes work irrecoverably.
        for path in [
            "dist/index.js",
            "build/Makefile",
            "out/report.txt",
            "target/spec.md",
            "src/venv_helpers.py",
            "src/node_modules_shim.ts",
            "docs/pycache-notes.md",
        ] {
            assert!(!is_generated_artifact(path), "must not be treated as generated: {path}");
        }
    }

    // ---------- test git repo fixture ----------

    /// A throwaway git repo with real commits and stash entries. This
    /// classifier's whole job is to compare git objects, so a fake in-memory
    /// repo would not exercise the code under test.
    struct TestRepo {
        dir: tempfile::TempDir,
    }

    impl TestRepo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            run(dir.path(), &["init", "-q", "-b", "main"]);
            run(dir.path(), &["config", "user.email", "test@example.com"]);
            run(dir.path(), &["config", "user.name", "Test"]);
            Self { dir }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, rel: &str, content: &str) {
            let p = self.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }

        fn commit_all(&self, msg: &str) {
            run(self.path(), &["add", "-A"]);
            run(self.path(), &["commit", "-q", "-m", msg]);
        }

        /// Quarantine-stash the current dirt — mirrors `check-main-clean.sh
        /// --quarantine`'s `stash push -u -m "loom-quarantine: $LABEL"`.
        fn quarantine_stash(&self, label: &str) {
            run(
                self.path(),
                &[
                    "stash",
                    "push",
                    "-u",
                    "-m",
                    &format!("loom-quarantine: {label}"),
                ],
            );
        }

        fn reflog(&self) -> Vec<QuarantineStashEntry> {
            list_quarantine_stashes(self.path()).unwrap()
        }
    }

    fn run(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    struct FakeIssueLookup(HashMap<u64, bool>);

    impl IssueStateLookup for FakeIssueLookup {
        fn is_closed(&mut self, issue: u64) -> Option<bool> {
            self.0.get(&issue).copied()
        }
    }

    fn verdict_for<'a>(paths: &'a [(String, PathVerdict)], want: &str) -> &'a PathVerdict {
        &paths
            .iter()
            .find(|(p, _)| p == want)
            .unwrap_or_else(|| panic!("no verdict for {want} in {paths:?}"))
            .1
    }

    // ---------- content check: the three shapes from #5690's audit ----------

    /// Shape 1 of 3: pure build-artifact / installer content, no real work.
    /// #5690 counted 58 of 148 stashes in this class, the largest being 1,749
    /// files of which 1,743 were `.venv/` and `__pycache__`.
    #[test]
    fn content_check_pure_build_artifact_is_safe() {
        let repo = TestRepo::new();
        repo.write("README.md", "hello\n");
        repo.commit_all("initial");

        repo.write(".loom/logs/sweep-issue-1.log", "log noise\n");
        repo.write("package-lock.json", "{}\n");
        repo.write("sim/.venv/lib/python3.12/site-packages/x.py", "generated\n");
        repo.write("tools/__pycache__/helper.cpython-312.pyc", "\0bytecode\n");
        repo.quarantine_stash("issue=9001");

        let entries = repo.reflog();
        assert_eq!(entries.len(), 1);
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Safe { paths } => {
                assert_eq!(paths.len(), 4);
                assert_eq!(
                    verdict_for(&paths, ".loom/logs/sweep-issue-1.log"),
                    &PathVerdict::IgnorableDirt
                );
                assert_eq!(verdict_for(&paths, "package-lock.json"), &PathVerdict::IgnorableDirt);
                assert_eq!(
                    verdict_for(&paths, "sim/.venv/lib/python3.12/site-packages/x.py"),
                    &PathVerdict::GeneratedArtifact
                );
                assert_eq!(
                    verdict_for(&paths, "tools/__pycache__/helper.cpython-312.pyc"),
                    &PathVerdict::GeneratedArtifact
                );
            }
            other => panic!("expected Safe, got {other:?}"),
        }
    }

    /// Shape 2a of 3: the stash's content is byte-identical to what is at
    /// `HEAD` right now.
    #[test]
    fn content_check_superseded_by_head_is_safe() {
        let repo = TestRepo::new();
        repo.write("scripts/compute-drift.sh", "#!/bin/bash\necho old\n");
        repo.commit_all("initial");

        repo.write("scripts/compute-drift.sh", "#!/bin/bash\necho stopped-host-paging\n");
        repo.quarantine_stash("issue=9002");

        // The same change lands on HEAD by another route (a merged PR).
        repo.write("scripts/compute-drift.sh", "#!/bin/bash\necho stopped-host-paging\n");
        repo.commit_all("feat: stopped-host paging (landed via PR)");

        let entries = repo.reflog();
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Safe { paths } => {
                assert_eq!(
                    verdict_for(&paths, "scripts/compute-drift.sh"),
                    &PathVerdict::IdenticalToHead
                );
            }
            other => panic!("expected Safe, got {other:?}"),
        }
    }

    /// Shape 2b of 3: #5690's actual `compute-drift.sh` shape — the stashed
    /// bytes were committed by a merged PR and then *further* edited, so they
    /// no longer match `HEAD`, but they are still preserved verbatim at a
    /// commit reachable from `HEAD`. Dropping the stash loses nothing.
    #[test]
    fn content_check_superseded_by_an_older_commit_is_safe() {
        let repo = TestRepo::new();
        repo.write("scripts/compute-drift.sh", "v1\n");
        repo.commit_all("initial");

        repo.write("scripts/compute-drift.sh", "v2 stopped-host paging\n");
        repo.quarantine_stash("issue=9006");

        // The stashed bytes land verbatim...
        repo.write("scripts/compute-drift.sh", "v2 stopped-host paging\n");
        repo.commit_all("feat: stopped-host paging (the merged PR)");
        // ...and are then built on further, so HEAD no longer matches.
        repo.write("scripts/compute-drift.sh", "v3 with follow-up fixes\n");
        repo.commit_all("fix: follow-up");

        let entries = repo.reflog();
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Safe { paths } => {
                match verdict_for(&paths, "scripts/compute-drift.sh") {
                    PathVerdict::SupersededInHistory { commit } => {
                        assert_eq!(commit.len(), 40, "expected a full sha, got {commit:?}");
                    }
                    other => panic!("expected SupersededInHistory, got {other:?}"),
                }
            }
            other => panic!("expected Safe, got {other:?}"),
        }
    }

    /// Shape 3 of 3: the `2AMLogic/gf180-ldo#51` case — real engineering
    /// content that never landed anywhere and is not installer-managed. This
    /// is the one stash in 148 that mattered; it must never be retired.
    #[test]
    fn content_check_genuine_unlanded_work_is_unsafe() {
        let repo = TestRepo::new();
        repo.write("src/device_sizing.py", "# baseline\n");
        repo.commit_all("initial");

        repo.write("src/device_sizing.py", "# abandoned experiment: wider ldo sizing sweep\n");
        repo.quarantine_stash("issue=9003");

        let entries = repo.reflog();
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Unsafe { paths } => {
                assert_eq!(blocking_paths(&paths), vec!["src/device_sizing.py"]);
            }
            other => panic!("expected Unsafe, got {other:?}"),
        }
    }

    /// A brand-new untracked source file (no `HEAD` counterpart, no history)
    /// is the most dangerous shape of all — it must never classify safe.
    #[test]
    fn content_check_untracked_new_source_file_is_unsafe() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");

        repo.write("src/brand_new_experiment.py", "print('never committed')\n");
        repo.quarantine_stash("issue=9007");

        let entries = repo.reflog();
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Unsafe { paths } => {
                assert_eq!(blocking_paths(&paths), vec!["src/brand_new_experiment.py"]);
            }
            other => panic!("expected Unsafe, got {other:?}"),
        }
    }

    /// A stash that *deletes* a file still at `HEAD` is not retirable: the
    /// deletion is intent we cannot prove landed anywhere.
    #[test]
    fn content_check_deletion_is_unsafe() {
        let repo = TestRepo::new();
        repo.write("src/doomed.py", "content\n");
        repo.commit_all("initial");

        std::fs::remove_file(repo.path().join("src/doomed.py")).unwrap();
        repo.quarantine_stash("issue=9008");

        let entries = repo.reflog();
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Unsafe { paths } => {
                assert_eq!(blocking_paths(&paths), vec!["src/doomed.py"]);
            }
            other => panic!("expected Unsafe, got {other:?}"),
        }
    }

    #[test]
    fn content_check_mixed_installer_and_superseded_is_still_safe() {
        let repo = TestRepo::new();
        repo.write("scripts/foo.sh", "echo old\n");
        repo.commit_all("initial");

        repo.write("scripts/foo.sh", "echo new\n");
        repo.write(".loom/logs/x.log", "noise\n");
        repo.quarantine_stash("issue=9004");

        repo.write("scripts/foo.sh", "echo new\n");
        repo.commit_all("feat: land foo.sh change via another route");

        let entries = repo.reflog();
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Safe { paths } => assert_eq!(paths.len(), 2),
            other => panic!("expected Safe, got {other:?}"),
        }
    }

    /// `git stash drop` is all-or-nothing, so a stash that is 90% superseded
    /// and 10% real must survive whole. This is precisely the live
    /// `2AMLogic/2am#52` `compute-drift.sh` shape found during the #5693
    /// back-test: `scripts/compute-drift.sh` was provably superseded, but its
    /// sibling test file's stashed bytes were never committed anywhere.
    #[test]
    fn content_check_one_unsafe_path_taints_the_whole_stash() {
        let repo = TestRepo::new();
        repo.write("scripts/compute-drift.sh", "echo old\n");
        repo.write("scripts/tests/test-drift.sh", "baseline\n");
        repo.commit_all("initial");

        repo.write("scripts/compute-drift.sh", "echo new\n");
        repo.write("scripts/tests/test-drift.sh", "unlanded variant\n");
        repo.quarantine_stash("issue=9005");

        repo.write("scripts/compute-drift.sh", "echo new\n");
        repo.commit_all("feat: land compute-drift.sh via another route");

        let entries = repo.reflog();
        match classify_stash_content(repo.path(), &entries[0].stash_ref) {
            ContentVerdict::Unsafe { paths } => {
                assert_eq!(blocking_paths(&paths), vec!["scripts/tests/test-drift.sh"]);
            }
            other => panic!("expected Unsafe, got {other:?}"),
        }
    }

    #[test]
    fn content_check_on_a_nonexistent_stash_ref_is_indeterminate_not_safe() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");

        match classify_stash_content(repo.path(), "stash@{7}") {
            ContentVerdict::Indeterminate { .. } => {}
            other => panic!("expected Indeterminate, got {other:?}"),
        }
    }

    // ---------- combined classifier: both conditions required ----------

    #[test]
    fn classify_stash_retires_only_when_closed_and_safe() {
        let repo = TestRepo::new();
        repo.write("scripts/foo.sh", "echo old\n");
        repo.commit_all("initial");
        repo.write("scripts/foo.sh", "echo new\n");
        repo.quarantine_stash("issue=42");
        repo.write("scripts/foo.sh", "echo new\n");
        repo.commit_all("feat: land it");

        let entries = repo.reflog();
        let mut closed = FakeIssueLookup(HashMap::from([(42, true)]));
        let mut open = FakeIssueLookup(HashMap::from([(42, false)]));
        let mut unknown = FakeIssueLookup(HashMap::new());

        assert!(matches!(
            classify_stash(repo.path(), &entries[0], &mut closed),
            RetireVerdict::Retire { .. }
        ));
        assert!(matches!(
            classify_stash(repo.path(), &entries[0], &mut open),
            RetireVerdict::Keep { .. }
        ));
        assert!(matches!(
            classify_stash(repo.path(), &entries[0], &mut unknown),
            RetireVerdict::Keep { .. }
        ));
    }

    #[test]
    fn classify_stash_never_retires_unsafe_content_even_when_closed() {
        // Half of the central invariant: a closed issue is NOT sufficient on
        // its own. This is the #5690 gf180-ldo#51 shape.
        let repo = TestRepo::new();
        repo.write("src/device_sizing.py", "baseline\n");
        repo.commit_all("initial");
        repo.write("src/device_sizing.py", "abandoned experiment\n");
        repo.quarantine_stash("issue=51");

        let entries = repo.reflog();
        let mut closed = FakeIssueLookup(HashMap::from([(51, true)]));
        assert!(matches!(
            classify_stash(repo.path(), &entries[0], &mut closed),
            RetireVerdict::Keep { .. }
        ));
    }

    #[test]
    fn classify_stash_never_retires_safe_content_when_issue_open() {
        // The other half: safe content is NOT sufficient on its own either —
        // an installer-only stash for a still-open issue must not be retired
        // just because its content is harmless.
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");
        repo.write("package-lock.json", "{}\n");
        repo.quarantine_stash("issue=7");

        let entries = repo.reflog();
        let mut open = FakeIssueLookup(HashMap::from([(7, false)]));
        assert!(matches!(
            classify_stash(repo.path(), &entries[0], &mut open),
            RetireVerdict::Keep { .. }
        ));
    }

    #[test]
    fn classify_stash_keeps_when_no_issue_reference() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");
        repo.write("package-lock.json", "{}\n");
        repo.quarantine_stash("unattributed");

        let entries = repo.reflog();
        let mut lookup = FakeIssueLookup(HashMap::new());
        assert!(matches!(
            classify_stash(repo.path(), &entries[0], &mut lookup),
            RetireVerdict::Keep { .. }
        ));
    }

    // ---------- drop / idempotency / journaling ----------

    #[test]
    fn drop_stash_is_idempotent() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");
        repo.write("package-lock.json", "{}\n");
        repo.quarantine_stash("issue=1");

        let entries = repo.reflog();
        assert_eq!(entries.len(), 1);

        assert_eq!(drop_stash(repo.path(), &entries[0]), DropOutcome::Dropped);
        // Dropping the same (now-gone) entry again must be a no-op, not an
        // error — #5693's "safely re-runnable" requirement.
        assert_eq!(drop_stash(repo.path(), &entries[0]), DropOutcome::AlreadyGone);
    }

    #[test]
    fn drop_stash_re_resolves_the_selector_after_indices_shift() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");
        repo.write("a.txt", "a\n");
        repo.quarantine_stash("issue=1");
        repo.write("b.txt", "b\n");
        repo.quarantine_stash("issue=2");

        let entries = repo.reflog();
        // entries[0] is stash@{0} (issue=2); entries[1] is stash@{1} (issue=1).
        let older = entries[1].clone();
        assert_eq!(older.stash_ref, "stash@{1}");

        // Drop the NEWER one out from under us; the older entry is now
        // stash@{0}, so a naive `git stash drop stash@{1}` would fail (or,
        // worse in a longer stack, drop the wrong entry).
        run(repo.path(), &["stash", "drop", "stash@{0}"]);

        assert_eq!(drop_stash(repo.path(), &older), DropOutcome::Dropped);
        assert!(repo.reflog().is_empty());
    }

    #[test]
    fn plan_and_execute_retirement_dry_run_drops_nothing() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");
        repo.write("package-lock.json", "{}\n");
        repo.quarantine_stash("issue=1");

        let entries = repo.reflog();
        let mut lookup = FakeIssueLookup(HashMap::from([(1, true)]));
        let reports = plan_and_execute_retirement(repo.path(), &entries, &mut lookup, false);

        assert_eq!(reports.len(), 1);
        assert!(matches!(reports[0].verdict, RetireVerdict::Retire { .. }));
        assert_eq!(reports[0].outcome, None);

        // Still there, and nothing journaled — a dry run must not touch the
        // stash stack or the retirement log.
        assert_eq!(repo.reflog().len(), 1);
        assert!(!repo.path().join(RETIREMENT_LOG_RELPATH).exists());
    }

    #[test]
    fn plan_and_execute_retirement_execute_drops_only_retire_verdicts() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");

        // A: closed issue, installer-only content -> Retire.
        repo.write("package-lock.json", "{}\n");
        repo.quarantine_stash("issue=1");
        // B: open issue, installer-only content -> Keep.
        repo.write("pnpm-lock.yaml", "x\n");
        repo.quarantine_stash("issue=2");
        // C: closed issue, real unlanded content -> Keep.
        repo.write("src/real.py", "unlanded\n");
        repo.quarantine_stash("issue=3");

        let entries = repo.reflog();
        assert_eq!(entries.len(), 3);

        let mut lookup = FakeIssueLookup(HashMap::from([(1, true), (2, false), (3, true)]));
        let reports = plan_and_execute_retirement(repo.path(), &entries, &mut lookup, true);

        assert_eq!(reports.len(), 3);
        let mut retired_issues: Vec<u64> = Vec::new();
        let mut kept_issues: Vec<u64> = Vec::new();
        for r in &reports {
            let issue = r.entry.issue.unwrap();
            match &r.verdict {
                RetireVerdict::Retire { .. } => {
                    assert_eq!(r.outcome, Some(DropOutcome::Dropped));
                    assert_eq!(r.log_error, None);
                    retired_issues.push(issue);
                }
                RetireVerdict::Keep { .. } => {
                    assert_eq!(r.outcome, None);
                    kept_issues.push(issue);
                }
            }
        }
        assert_eq!(retired_issues, vec![1]);
        kept_issues.sort_unstable();
        assert_eq!(kept_issues, vec![2, 3]);

        // Only A is gone; B and C survive untouched.
        let remaining_issues: Vec<u64> = repo.reflog().iter().filter_map(|e| e.issue).collect();
        assert_eq!(remaining_issues.len(), 2);
        assert!(!remaining_issues.contains(&1));
    }

    #[test]
    fn executed_drops_are_journaled_with_a_recovery_handle() {
        let repo = TestRepo::new();
        repo.write("README.md", "x\n");
        repo.commit_all("initial");
        repo.write("package-lock.json", "{}\n");
        repo.quarantine_stash("run=sweep-abc issue=1");

        let entries = repo.reflog();
        let stash_commit = entries[0].commit.clone();

        let mut lookup = FakeIssueLookup(HashMap::from([(1, true)]));
        let reports = plan_and_execute_retirement(repo.path(), &entries, &mut lookup, true);
        assert_eq!(reports[0].outcome, Some(DropOutcome::Dropped));
        assert_eq!(reports[0].log_error, None);

        let log = std::fs::read_to_string(repo.path().join(RETIREMENT_LOG_RELPATH)).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["event"], "stash-retirement.retire");
        assert_eq!(record["stash_commit"], stash_commit);
        assert_eq!(record["issue"], 1);
        assert_eq!(record["run"], "sweep-abc");
        assert_eq!(record["paths"][0], "package-lock.json");

        // The journaled sha is a real recovery handle: the dropped stash
        // commit is unreachable but still in the object database, so its
        // content is retrievable until gc.
        let recovered = read_stash_blob(repo.path(), &stash_commit, "package-lock.json")
            .expect("dropped stash content is still recoverable via the journaled sha");
        assert_eq!(recovered, b"{}\n");
    }

    #[test]
    fn stash_index_parses_and_defaults_to_zero_on_malformed_input() {
        assert_eq!(stash_index("stash@{0}"), 0);
        assert_eq!(stash_index("stash@{12}"), 12);
        assert_eq!(stash_index("not-a-stash-ref"), 0);
    }
}
