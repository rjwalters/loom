//! The **closed-unmerged** arm of `worktree.sh`'s branch-resolution contract
//! (#9083) — the third state a pushed `origin/feature/issue-N` can be in, next
//! to the open-PR and merged-PR arms that already had answers.
//!
//! # The defect
//!
//! `worktree.sh N`, on an issue whose `origin/feature/issue-N` exists but has
//! no local ref, resolves the name against origin and reuses it — that is
//! #4823, and it is right: a Doctor fixing review feedback must continue the
//! real PR history, not a fresh branch off `main`. #5657 carved out the one
//! case where it is wrong, a tip that is already the head of a **merged** PR
//! (the partial-increment slice convention reuses `feature/issue-N` across
//! slices, and the forge leaves the ref on origin when
//! auto-delete-head-branches is off), and skips to a fresh branch there.
//!
//! Nothing covered a tip that is the head of a PR **closed without merging**.
//! On #8195 that reused `origin/feature/issue-8195` at PR #8275's head — a
//! reverted delegation attempt plus a rejected follow-up, tens of commits
//! behind `main` — and said so only through the generic "has diverged from
//! main" warning, which also fires for every legitimate in-flight PR branch
//! and therefore carries no information about whether continuing is right.
//!
//! A closed-unmerged tip is the *same* hazard as a merged one with a strictly
//! worse payload: a merged branch at least contains work that is on `main`,
//! whereas a closed-unmerged one contains work somebody decided **not** to
//! take.
//!
//! # Why refuse rather than skip, and why not unconditionally
//!
//! Three shapes were on the table (see #9083). Skipping to a fresh branch —
//! the merged case's answer — is wrong here: the merged branch is dead history
//! whose content is already on `main`, so force-pushing over it costs nothing,
//! while a closed PR's branch is live-but-rejected history that a `git push`
//! from a fresh same-named branch would have to force over. A warning alone
//! loses to the failure mode it is warning about: the reused worktree is
//! already built and the agent is already committing into it.
//!
//! So this refuses — exit 1, naming the PR number and both remedies — matching
//! the precedent the sibling LOCAL-branch arm set for an already-landed branch
//! in #8280, which also cannot silently fall through and also refuses outright.
//!
//! It is emphatically **not** an unconditional refusal of a closed PR's
//! branch. A PR closed by accident, or closed with the intent that the branch
//! be picked up again, must stay resumable (#4823/#7765's "attach to the
//! existing PR head" path). The remedy the refusal prints is the escape hatch,
//! and it is the one this family already prescribes elsewhere for the
//! forge-unavailable case: create the local branch first, and `worktree.sh`'s
//! local-ref arm reuses it —
//!
//! ```text
//! git branch feature/issue-N origin/feature/issue-N && ./.loom/scripts/worktree.sh N
//! ```
//!
//! That path needs no flag, stays explicit (two commands, typed on purpose),
//! and is covered by `test-worktree-stale-closed-branch.sh`.
//!
//! # Why this is not a `branch_landed` verdict
//!
//! [`super::branch_landed`] is a three-way `landed` / `not-landed` / `unknown`
//! primitive, and a closed-unmerged branch is `not-landed` — correctly and
//! load-bearingly so. That verdict is what stops
//! [`super::branch_delete`] escalating to `git branch -D` on it, and folding
//! "closed-unmerged" into the ladder as a fourth state would put a new token in
//! front of every consumer of a contract whose whole point is that `unknown`
//! cannot be collapsed. The closed-unmerged question is a *different* question
//! asked at *one* call site, so it lives here, and `branch-landed.sh` /
//! `branch_landed.rs` each carry a one-line note pointing at this module so a
//! future reader does not re-derive it there.
//!
//! # Why Rust and not four more lines of `worktree.sh`
//!
//! Same reason as [`super::stale_ref`] and [`super::issue_lock`]:
//! `lib/worktree-forge-pr-check.sh` is `contract`-category and the portable
//! shell ratchet gives its growth no override
//! (`.loom/docs/shell-language-policy.md`). The shell side is one delegating
//! line, so there is exactly **one** implementation of this arm and no twin to
//! drift.
//!
//! # Exit-code contract
//!
//! | code | meaning |
//! |---|---|
//! | 0 | proceed — reuse `origin/<branch>` exactly as before |
//! | 1 | refuse — the tip is a closed-unmerged PR's head; a message was printed |
//!
//! Nothing else is ever returned, and **every** inability to decide returns 0.
//! That direction is fixed by the arm this guard sits in: the merged check it
//! follows fails open to reuse on an `unknown` verdict, because a forge outage
//! must never block worktree creation, and `test-worktree-stale-merged-branch.sh`
//! pins that. A guard that refused on a failed probe would make every
//! hermetic/offline clone unable to reuse its own pushed branch.
//!
//! `LOOM_BRANCH_LANDED_OFFLINE=1` skips the probe, the same seam
//! [`super::branch_landed::forge_probe`] honours.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::branch_landed::resolve_commit;
use super::wip::Out;

/// Everything the guard needs, assembled by the shell wrapper.
pub struct Options {
    /// `$BRANCH_NAME` — the branch whose `origin/` tip is under question. The
    /// caller has already confirmed `refs/remotes/origin/<branch>` exists and
    /// that no LOCAL ref of this name does.
    pub branch: String,
    /// `$ISSUE_NUMBER`, quoted into the message and the JSON document.
    pub issue: String,
    /// `$BASE_DISPLAY` — how the base ref is spelled to a human (`main`).
    pub base_display: String,
    /// The main workspace root every `git` and forge call runs in.
    pub repo: PathBuf,
    /// `$JSON_OUTPUT`. A string rather than a flag because the shell dispatch
    /// has to stay a single line under the portable-shell ratchet: passing
    /// `--json-output "$json_output"` needs no conditional in bash, where a
    /// boolean flag would.
    pub json_output: String,
}

/// A pull request as the probe reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub number: String,
    /// `OPEN` / `CLOSED` / `MERGED`, verbatim from the forge.
    pub state: String,
    pub head_sha: String,
    pub url: String,
    /// `true` when `mergedAt` is a non-null timestamp. Belt-and-braces next to
    /// `state`: `gh pr list --state closed` has historically resolved through
    /// GitHub's search API, where the `state:closed` qualifier *includes*
    /// merged PRs, so neither field alone is a safe discriminator.
    pub merged: bool,
}

impl Pr {
    /// Closed, and never merged — the state this guard exists for.
    #[must_use]
    pub fn is_closed_unmerged(&self) -> bool {
        !self.merged && self.state.eq_ignore_ascii_case("CLOSED")
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("OPEN")
    }
}

/// What one forge round-trip produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The forge answered; these are every PR it knows head-matching the
    /// branch, in whatever order it returned them.
    Answered(Vec<Pr>),
    /// The forge could not be asked or did not answer usably. Always proceeds
    /// — see the module doc's exit-code contract.
    Unavailable,
}

/// The decision, with the evidence that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Reuse `origin/<branch>`, exactly as before this guard existed.
    Proceed(Proceed),
    /// The tip is the head of a closed-unmerged PR.
    Refuse(Pr),
}

/// Why a [`Decision::Proceed`] proceeded. Carried so a test can pin *which*
/// rung allowed the reuse, not merely that it was allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proceed {
    /// The forge could not be asked, or `origin/<branch>` did not resolve.
    Undecidable,
    /// An OPEN PR head-matches this branch: there is live work to continue, so
    /// a same-named closed PR in its history is not a reason to refuse. This
    /// is the reopened-as-a-new-PR shape (close #A, open #B on the same
    /// branch), where the tip can still equal #A's head.
    OpenPr(String),
    /// No closed-unmerged PR head-matches the branch at all.
    NoClosedPr,
    /// A closed-unmerged PR exists for the name, but `origin/<branch>` has
    /// moved past its head — the branch carries commits the closure never saw,
    /// so the tip is not "the closed PR's head". Mirrors #7872's
    /// `merged-head-mismatch` rung: an exact tip match, or no answer.
    TipMovedPast(String),
}

/// Decide from already-gathered inputs. Pure — no git, no forge — so the
/// decision table is testable without either.
#[must_use]
pub fn decide(tip: Option<&str>, probe: &Probe) -> Decision {
    let Probe::Answered(prs) = probe else {
        return Decision::Proceed(Proceed::Undecidable);
    };
    let Some(tip) = tip.filter(|t| !t.is_empty()) else {
        return Decision::Proceed(Proceed::Undecidable);
    };
    if let Some(open) = prs.iter().find(|p| p.is_open()) {
        return Decision::Proceed(Proceed::OpenPr(open.number.clone()));
    }
    let mut closed = prs.iter().filter(|p| p.is_closed_unmerged()).peekable();
    if closed.peek().is_none() {
        return Decision::Proceed(Proceed::NoClosedPr);
    }
    let mut first: Option<&Pr> = None;
    for pr in closed {
        if pr.head_sha == tip {
            return Decision::Refuse(pr.clone());
        }
        first.get_or_insert(pr);
    }
    Decision::Proceed(Proceed::TipMovedPast(first.map(|p| p.number.clone()).unwrap_or_default()))
}

/// Run the guard against the real forge. Returns the process exit code.
pub fn run(opts: &Options) -> i32 {
    run_with(opts, &|branch| forge_probe(&opts.repo, branch))
}

/// [`run`] with the forge round-trip injected — the seam a test uses instead
/// of a network, a `gh`, or a `loom-daemon`.
pub fn run_with(opts: &Options, forge: &dyn Fn(&str) -> Probe) -> i32 {
    let tip = resolve_commit(&opts.repo, &format!("refs/remotes/origin/{}", opts.branch));
    // Do not spend a forge round-trip when the local half already cannot
    // answer: with no resolvable tip there is nothing to match a PR head
    // against, and the decision is Proceed either way.
    let probe = if tip.is_some() {
        forge(&opts.branch)
    } else {
        Probe::Unavailable
    };
    match decide(tip.as_deref(), &probe) {
        Decision::Proceed(_) => 0,
        Decision::Refuse(pr) => {
            report(opts, &pr);
            1
        }
    }
}

/// The refusal, in whichever of the two output modes the caller is in.
///
/// Deliberately distinct wording from both siblings so the three states are
/// tellable apart in a log: the merged arm says "is the head of already-merged
/// PR #N - creating a fresh branch", the generic divergence warning says "has
/// diverged from main", and this says "CLOSED WITHOUT MERGING - refusing".
fn report(opts: &Options, pr: &Pr) {
    let branch = &opts.branch;
    let issue = &opts.issue;
    if opts.json_output == "true" {
        Out::new(true).json_line(&format!(
            "{{\"success\": false, \"error\": \"closed-unmerged-pr-branch\", \"issueNumber\": {issue}, \"branch\": \"{branch}\", \"prNumber\": {}}}",
            json_number(&pr.number)
        ));
        return;
    }
    Out::error(&format!(
        "origin/{branch} is the head of PR #{} (CLOSED WITHOUT MERGING) - refusing to reuse it as the base for issue {issue}. That branch carries work somebody decided not to take, and a push would have to force over a closed PR's history.",
        pr.number
    ));
    if !pr.url.is_empty() {
        println!("  PR: {}", pr.url);
    }
    println!(
        "  To build this issue on {} instead, pass a branch name that is not the closed PR's:",
        opts.base_display
    );
    println!("    ./.loom/scripts/worktree.sh {issue} <custom-branch-name>");
    println!(
        "  To resume PR #{} deliberately (e.g. it was closed by accident), create the local branch first - worktree.sh then reuses it:",
        pr.number
    );
    println!("    git branch {branch} origin/{branch} && ./.loom/scripts/worktree.sh {issue}");
}

/// A PR number as a JSON value: bare when it is numeric, `null` when the forge
/// gave us something that is not (never a quoted string, which would change
/// the field's type for an existing consumer).
fn json_number(number: &str) -> String {
    if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
        number.to_string()
    } else {
        "null".to_string()
    }
}

/// One forge round-trip: every PR — open, closed, or merged — whose head
/// branch is `branch`.
///
/// `--state all` rather than `--state closed` on purpose. `gh`'s `closed`
/// filter has resolved through GitHub's search API, where `state:closed`
/// includes merged PRs, so it is not a reliable discriminator; and the OPEN
/// rung of [`decide`] needs to see an open PR that shares the branch with a
/// closed one anyway. One query answers both.
///
/// Command selection mirrors [`super::branch_landed::forge_probe`] exactly —
/// `loom-daemon forge` when one is on `PATH` (the Gitea passthrough), else
/// `gh`. We *are* loom-daemon, and calling the in-process forge client would
/// be the obvious shortcut; it is deliberately not taken, for the reason
/// `branch_landed` records: which credential path a probe uses is observable
/// forge behaviour, not an implementation detail.
#[must_use]
pub fn forge_probe(repo: &Path, branch: &str) -> Probe {
    let branch = branch.strip_prefix("origin/").unwrap_or(branch);
    if branch.is_empty() {
        return Probe::Unavailable;
    }
    if std::env::var("LOOM_BRANCH_LANDED_OFFLINE").as_deref() == Ok("1") {
        return Probe::Unavailable;
    }
    let (program, leading): (&str, &[&str]) = if on_path("loom-daemon") {
        ("loom-daemon", &["forge"])
    } else if on_path("gh") {
        ("gh", &[])
    } else {
        return Probe::Unavailable;
    };
    let out = Command::new(program)
        .args(leading)
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,state,mergedAt,headRefOid,url",
            "--limit",
            "20",
        ])
        .current_dir(repo)
        .output();
    let Ok(out) = out else {
        return Probe::Unavailable;
    };
    if !out.status.success() {
        // Every failure shape — an unauthenticated `gh`, a rate limit, a local
        // filesystem `origin` with no forge relationship at all, a
        // `loom-daemon forge` decline on Gitea — lands here, and all of them
        // proceed. There is no need to tell them apart the way
        // `_worktree_open_pr_for_branch` must: that helper gates a refusal on
        // "could not check", this one gates a refusal on positive evidence.
        return Probe::Unavailable;
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the forge's `--json number,state,mergedAt,headRefOid,url` array.
///
/// Separated from the round-trip so the shape the real forge returns can be
/// pinned as a fixture. Unparseable output is [`Probe::Unavailable`], never an
/// empty [`Probe::Answered`] — "the forge said nothing matches" and "we could
/// not read the answer" must not collapse, even though both proceed today.
#[must_use]
pub fn parse_probe(text: &str) -> Probe {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return Probe::Unavailable;
    };
    let Some(items) = value.as_array() else {
        return Probe::Unavailable;
    };
    Probe::Answered(
        items
            .iter()
            .map(|item| Pr {
                number: item
                    .get("number")
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default(),
                state: item
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                head_sha: item
                    .get("headRefOid")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url: item
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                merged: item
                    .get("mergedAt")
                    .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty())),
            })
            .collect(),
    )
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

#[cfg(test)]
mod tests;
