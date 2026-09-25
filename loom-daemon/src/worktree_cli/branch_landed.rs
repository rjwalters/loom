//! `lib/branch-landed.sh` in Rust — the ONE "has this branch landed on the
//! default branch?" primitive, as `worktree.sh remove` consumes it (#8195
//! slice 3, epic #7810).
//!
//! # Why this is here at all
//!
//! The branch-delete step of `worktree.sh remove` is the second irreversible
//! operation on that path (after `git worktree remove --force`), and the one
//! with no undo at all once the branch tip is unreachable. It may only
//! escalate to `git branch -D` on *proof* that the default branch already
//! contains everything the branch carries. `branch_landed` is that proof, and
//! porting the verb without it would have meant either shelling back out to
//! bash from Rust or — far worse — substituting a reachability test, which is
//! wrong under both squash and rebase merges and is exactly the defect class
//! (#5189, #5665, #4918, #4889, #5657) the shared primitive was built to end.
//!
//! # The three-way answer is load-bearing
//!
//! [`Verdict::Unknown`] is a real state, not a pessimistic `NotLanded`.
//! Collapsing it either way is a known bug in both directions: "landed"
//! force-deletes unmerged work, "not-landed" resurrects the pre-#4889 "can
//! never clean up a squash-merged branch" behaviour. [`Answer::forge_status`]
//! carries the fourth distinction the caller needs — "the safety check could
//! not even be attempted" — which is why the shell exports it as a side-channel
//! global rather than folding it into the verdict.
//!
//! # Deliberate divergences from the shell, both argued
//!
//! 1. **No `jq` dependency.** The shell probe returns `unavailable` on a host
//!    without `jq`, because that is how it parses the forge's JSON. Parsing it
//!    natively here means a `jq`-less host now gets a real forge answer instead
//!    of `unavailable`. That is strictly more evidence, and it can only move a
//!    verdict *toward* a definitive answer — it never turns a `NotLanded` into
//!    a `Landed` the shell would have refused, because the tip-equality
//!    requirement below is unchanged.
//! 2. **`loom-daemon forge` is still preferred over `gh` when a `loom-daemon`
//!    is on `PATH`.** We *are* loom-daemon, so calling the in-process forge
//!    client would be the obvious shortcut — and it is deliberately not taken.
//!    The shell's choice is observable behaviour (a Gitea host has no `gh` at
//!    all, and `loom-daemon forge` is how it answers), and a port that quietly
//!    changed which credential path a probe uses would be changing the forge
//!    contract under cover of a refactor.
//!
//! # Why this is NOT [`crate::worktree_ops::landed`]
//!
//! That module is the daemon's other Rust copy of the same #7812 ladder, and
//! this slice deliberately does not fold into it — stated here rather than
//! discovered by a reviewer, because "reuse `worktree_ops`, do not re-derive
//! it" is this port's own instruction and this is the one place it is not
//! followed.
//!
//! The two are not interchangeable today, in a direction that matters:
//!
//! - **Different key.** `landed::probe` takes an `issue_num` and probes the
//!   forge for `feature/issue-<n>`. `worktree.sh remove` deletes whatever
//!   branch the worktree had attached — `worktree.sh <N> <custom-branch>`
//!   allows any name — so the primitive it needs is keyed on a branch *name*.
//! - **Different strictness.** This module requires the merged PR's head SHA
//!   to still equal the local tip (#7872's `merged-head-mismatch` rung);
//!   `landed::probe` treats any merged PR for the name as `Rewritten`. So
//!   this one is the *stricter* of the two, and routing `clean --aggressive`
//!   through it would change what that bulk reaper reaps.
//! - **Different output.** `_maybe_delete_local_branch`'s messages — which
//!   `test-worktree-remove-squash-merge.sh` asserts verbatim — are keyed on
//!   the evidence token and the `forge_status` side-channel. `Landed` carries
//!   neither.
//!
//! Converging them means making `clean --aggressive` stricter, which is a
//! behaviour change to an unattended bulk `rm -rf` path and belongs in its own
//! issue with its own evidence, not in a slice whose whole claim is that the
//! retained suites pass unchanged. Tracked in #8470.

use std::path::Path;
use std::process::Command;

/// What the ladder concluded. Mirrors the shell's stdout token exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// `landed` — the default branch already contains everything this branch
    /// has.
    Landed,
    /// `not-landed` — the branch carries content the default branch does not.
    NotLanded,
    /// `unknown` — nothing could answer. MUST fail closed at every call site.
    Unknown,
}

impl Verdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Landed => "landed",
            Verdict::NotLanded => "not-landed",
            Verdict::Unknown => "unknown",
        }
    }
}

/// Which rung answered. Same token set as `BRANCH_LANDED_EVIDENCE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Ancestor,
    MergedHeadMatch,
    ForgeMergedPr,
    TreeEqual,
    TreeDiffers,
    TreeConflict,
    ForgeNoMergedPr,
    MergedHeadMismatch,
    Inconclusive,
}

impl Evidence {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Evidence::Ancestor => "ancestor",
            Evidence::MergedHeadMatch => "merged-head-match",
            Evidence::ForgeMergedPr => "forge-merged-pr",
            Evidence::TreeEqual => "tree-equal",
            Evidence::TreeDiffers => "tree-differs",
            Evidence::TreeConflict => "tree-conflict",
            Evidence::ForgeNoMergedPr => "forge-no-merged-pr",
            Evidence::MergedHeadMismatch => "merged-head-mismatch",
            Evidence::Inconclusive => "inconclusive",
        }
    }
}

/// Same token set as `BRANCH_LANDED_FORGE_STATUS`.
///
/// `Unavailable` is the one every caller must special-case: it means "the
/// safety check could not even be attempted", never "checked, and it is
/// unmerged".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeStatus {
    Found,
    NotFound,
    Unavailable,
    Hinted,
    Skipped,
}

impl ForgeStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeStatus::Found => "found",
            ForgeStatus::NotFound => "not_found",
            ForgeStatus::Unavailable => "unavailable",
            ForgeStatus::Hinted => "hinted",
            ForgeStatus::Skipped => "skipped",
        }
    }
}

/// Everything the shell exposed: the verdict plus its four side-channel
/// globals, returned as one value instead of smuggled through the environment.
///
/// The shell had to warn callers that `$(branch_landed …)` discards the
/// globals because a subshell drops them. A struct has no such failure mode —
/// that whole hazard class is gone, not merely documented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub verdict: Verdict,
    pub evidence: Evidence,
    pub pr_number: Option<String>,
    pub pr_head_sha: Option<String>,
    pub forge_status: ForgeStatus,
}

impl Answer {
    fn unknown() -> Self {
        Self {
            verdict: Verdict::Unknown,
            evidence: Evidence::Inconclusive,
            pr_number: None,
            pr_head_sha: None,
            forge_status: ForgeStatus::Skipped,
        }
    }
}

/// Outcome of one forge probe. Separated from [`Answer`] so a test can inject
/// a probe without a network, a `gh`, or a `loom-daemon` — the same seam the
/// shell created by making `_branch_landed_forge_probe` a redefinable
/// function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeProbe {
    pub status: ForgeStatus,
    pub head_sha: Option<String>,
    pub number: Option<String>,
}

impl ForgeProbe {
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            status: ForgeStatus::Unavailable,
            head_sha: None,
            number: None,
        }
    }
}

/// `branch_landed <branch> [<default-branch>] [<known-merged-head-sha>]`
/// against the real forge.
#[must_use]
pub fn probe(repo: &Path, branch: &str, default_name: Option<&str>, hint_sha: &str) -> Answer {
    probe_with(repo, branch, default_name, hint_sha, &|b| forge_probe(repo, b))
}

/// What the host's `git` can do. Detected once at the ladder's entry and
/// passed down, never re-read from the environment mid-ladder.
///
/// This is a value rather than an ambient lookup on purpose. The shell's
/// `LOOM_BRANCH_LANDED_GIT_VERSION` seam is process-global, and a test that
/// sets it to simulate a pre-2.38 host silently changes the answer of every
/// *other* test running concurrently in the same process — which is how a
/// rung-4 assertion can pass alone and fail in a full run. Threading the
/// capability through as data removes that class instead of serialising
/// around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// git >= 2.38, where `git merge-tree --write-tree` exists.
    pub merge_tree: bool,
}

impl Caps {
    /// Read the host's capabilities, honouring the shell's
    /// `LOOM_BRANCH_LANDED_GIT_VERSION` override.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            merge_tree: supports_merge_tree(),
        }
    }
}

/// [`probe`] with the forge round-trip injected.
///
/// Every rung *except* the forge is a local `git` call, so this seam is the
/// whole of what a test has to fake.
#[must_use]
pub fn probe_with(
    repo: &Path,
    branch: &str,
    default_name: Option<&str>,
    hint_sha: &str,
    forge: &dyn Fn(&str) -> ForgeProbe,
) -> Answer {
    probe_with_caps(repo, branch, default_name, hint_sha, forge, Caps::detect())
}

/// [`probe_with`] with the host's git capabilities supplied rather than
/// detected — the seam a pre-2.38-host test uses instead of mutating the
/// process environment.
#[must_use]
pub fn probe_with_caps(
    repo: &Path,
    branch: &str,
    default_name: Option<&str>,
    hint_sha: &str,
    forge: &dyn Fn(&str) -> ForgeProbe,
    caps: Caps,
) -> Answer {
    let mut answer = Answer::unknown();
    if branch.is_empty() {
        return answer;
    }

    let tip = resolve_branch(repo, branch);
    let default_sha = resolve_default(repo, default_name);

    // Rung 1: ancestry. Proves `landed` only; its falsity proves nothing —
    // which is exactly the trap the four private heuristics fell into.
    if let (Some(tip), Some(default_sha)) = (tip.as_deref(), default_sha.as_deref()) {
        if git_ok(repo, &["merge-base", "--is-ancestor", tip, default_sha]) {
            answer.verdict = Verdict::Landed;
            answer.evidence = Evidence::Ancestor;
            return answer;
        }
    }

    let mut forge_answered_negative = false;
    if hint_sha.is_empty() {
        // Rung 3: the forge.
        let p = forge(branch);
        answer.forge_status = p.status;
        match p.status {
            ForgeStatus::Found => {
                answer.pr_head_sha.clone_from(&p.head_sha);
                answer.pr_number.clone_from(&p.number);
                // #7872: require an ACTUAL tip match. A branch name that
                // resolves to no local ref is the case with the LEAST
                // evidence, not a free pass.
                if let (Some(tip), Some(sha)) = (tip.as_deref(), p.head_sha.as_deref()) {
                    if !tip.is_empty() && tip == sha {
                        answer.verdict = Verdict::Landed;
                        answer.evidence = Evidence::ForgeMergedPr;
                        return answer;
                    }
                }
                answer.evidence = Evidence::MergedHeadMismatch;
                forge_answered_negative = true;
            }
            ForgeStatus::NotFound => {
                answer.evidence = Evidence::ForgeNoMergedPr;
                forge_answered_negative = true;
            }
            // `unavailable` — only the tree check can answer now.
            _ => {}
        }
    } else {
        // Rung 2: the caller's already-known merged-PR head SHA.
        answer.forge_status = ForgeStatus::Hinted;
        answer.pr_head_sha = Some(hint_sha.to_string());
        if tip.as_deref().is_some_and(|t| t == hint_sha) {
            answer.verdict = Verdict::Landed;
            answer.evidence = Evidence::MergedHeadMatch;
            return answer;
        }
        // The caller already knows the merged head and the tip is not it; no
        // forge round-trip can add anything.
        answer.evidence = Evidence::MergedHeadMismatch;
        forge_answered_negative = true;
    }

    // Rung 4: tree equality. Squash/rebase/merge-commit proof, fully offline.
    if let (Some(tip), Some(default_sha)) = (tip.as_deref(), default_sha.as_deref()) {
        if caps.merge_tree {
            match merge_tree_write_tree(repo, default_sha, tip) {
                MergeTree::Tree(merged) => {
                    let default_tree = git_stdout(
                        repo,
                        &[
                            "rev-parse",
                            "--verify",
                            "-q",
                            &format!("{default_sha}^{{tree}}"),
                        ],
                    );
                    if default_tree
                        .as_deref()
                        .is_some_and(|d| !d.is_empty() && d == merged)
                    {
                        answer.verdict = Verdict::Landed;
                        answer.evidence = Evidence::TreeEqual;
                    } else {
                        answer.verdict = Verdict::NotLanded;
                        answer.evidence = Evidence::TreeDiffers;
                    }
                    return answer;
                }
                MergeTree::Conflict => {
                    // Exit 1 is "merge conflicts": content that collides with
                    // the default branch definitively has not landed.
                    answer.verdict = Verdict::NotLanded;
                    answer.evidence = Evidence::TreeConflict;
                    return answer;
                }
                // Any other non-zero exit (bad args, unreadable object, a
                // pre-2.38 git) is a real failure and must NOT read as an
                // answer.
                MergeTree::NoAnswer => {}
            }
        }
    }

    if forge_answered_negative {
        answer.verdict = Verdict::NotLanded;
    }
    answer
}

/// The forge round-trip: a MERGED pull request whose head branch is `branch`.
///
/// Mirrors `_branch_landed_forge_probe` including its command selection —
/// `loom-daemon forge` when one is on `PATH` (the Gitea passthrough), else
/// `gh` — and its `LOOM_BRANCH_LANDED_OFFLINE=1` seam.
#[must_use]
pub fn forge_probe(repo: &Path, branch: &str) -> ForgeProbe {
    let branch = branch.strip_prefix("origin/").unwrap_or(branch);
    if branch.is_empty() {
        return ForgeProbe::unavailable();
    }
    if std::env::var("LOOM_BRANCH_LANDED_OFFLINE").as_deref() == Ok("1") {
        return ForgeProbe::unavailable();
    }

    let (program, leading): (&str, &[&str]) = if on_path("loom-daemon") {
        ("loom-daemon", &["forge"])
    } else if on_path("gh") {
        ("gh", &[])
    } else {
        return ForgeProbe::unavailable();
    };

    let out = Command::new(program)
        .args(leading)
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "merged",
            "--json",
            "headRefOid,number",
            "--limit",
            "1",
        ])
        .current_dir(repo)
        .output();
    let Ok(out) = out else {
        return ForgeProbe::unavailable();
    };
    if !out.status.success() {
        return ForgeProbe::unavailable();
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return ForgeProbe::unavailable();
    };
    let first = value.get(0);
    let head_sha = first
        .and_then(|v| v.get("headRefOid"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let number = first.and_then(|v| v.get("number")).map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });

    match head_sha {
        // `jq -r '.[0].headRefOid // empty'` — a present SHA is `found`, and
        // anything else (empty array, null field) is a definitive `not_found`.
        Some(sha) => ForgeProbe {
            status: ForgeStatus::Found,
            head_sha: Some(sha),
            number,
        },
        None => ForgeProbe {
            status: ForgeStatus::NotFound,
            head_sha: None,
            number: None,
        },
    }
}

fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Accept a local branch, a remote branch, a bare ref name, or any rev —
/// the shell's `_branch_landed_resolve_branch` order exactly.
fn resolve_branch(repo: &Path, branch: &str) -> Option<String> {
    commit(repo, &format!("refs/heads/{branch}"))
        .or_else(|| commit(repo, branch))
        .or_else(|| commit(repo, &format!("refs/remotes/origin/{branch}")))
}

/// Resolve the default-branch argument (possibly absent) to a commit.
///
/// Prefers the remote-tracking ref — it is what the branch actually has to
/// land ON — then the local branch, then the detected default, then the
/// hardcoded fallbacks. Same order as `_branch_landed_resolve_default`.
fn resolve_default(repo: &Path, name: Option<&str>) -> Option<String> {
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        let bare = name.strip_prefix("origin/").unwrap_or(name);
        let found = commit(repo, &format!("refs/remotes/origin/{bare}"))
            .or_else(|| commit(repo, &format!("refs/heads/{bare}")))
            .or_else(|| commit(repo, name));
        if found.is_some() {
            return found;
        }
    }
    if let Some(detected) = super::default_branch::resolve(repo) {
        let found = commit(repo, &format!("refs/remotes/origin/{detected}"))
            .or_else(|| commit(repo, &format!("refs/heads/{detected}")));
        if found.is_some() {
            return found;
        }
    }
    for fallback in [
        "refs/remotes/origin/main",
        "refs/heads/main",
        "refs/remotes/origin/master",
        "refs/heads/master",
    ] {
        if let Some(sha) = commit(repo, fallback) {
            return Some(sha);
        }
    }
    None
}

fn commit(repo: &Path, rev: &str) -> Option<String> {
    if rev.is_empty() {
        return None;
    }
    git_stdout(repo, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])
        .filter(|s| !s.is_empty())
}

/// git >= 2.38, where `git merge-tree --write-tree` exists.
///
/// `LOOM_BRANCH_LANDED_GIT_VERSION` overrides detection, the same seam the
/// shell exposes so a test can simulate a pre-2.38 host.
fn supports_merge_tree() -> bool {
    let version = std::env::var("LOOM_BRANCH_LANDED_GIT_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            let out = Command::new("git").arg("--version").output().ok()?;
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .nth(2)
                .map(str::to_string)
        });
    version.as_deref().is_some_and(merge_tree_in_version)
}

/// The version predicate on its own, so it can be pinned without a `git` or an
/// environment variable.
fn merge_tree_in_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(Ok(major)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    let minor = parts
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    major > 2 || (major == 2 && minor >= 38)
}

enum MergeTree {
    Tree(String),
    Conflict,
    NoAnswer,
}

fn merge_tree_write_tree(repo: &Path, default_sha: &str, tip: &str) -> MergeTree {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-tree", "--write-tree", default_sha, tip])
        .output()
    else {
        return MergeTree::NoAnswer;
    };
    match out.status.code() {
        Some(0) => {
            // merge-tree prints the OID on the first line.
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            if first.is_empty() {
                MergeTree::NoAnswer
            } else {
                MergeTree::Tree(first)
            }
        }
        Some(1) => MergeTree::Conflict,
        _ => MergeTree::NoAnswer,
    }
}

fn git_ok(repo: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git_stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests;
