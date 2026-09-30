//! `lib/worktree-forge-pr-check.sh`'s `_worktree_open_pr_for_branch` — the
//! forge round-trip behind the #7765 fresh-branch-shadow guard (#8195 slice
//! 15, epic #7810).
//!
//! # The question this answers
//!
//! `worktree.sh <N>` reaches this exactly once: neither a local branch nor
//! `refs/remotes/origin/<branch>` exists for the issue, and it is about to
//! create a **fresh** branch of that name off the base ref. Before it does,
//! it needs to know whether an OPEN pull request already claims that branch
//! name — because `origin/<branch>` being absent is not proof that no PR
//! does: a fork PR's head never appears as `origin/<branch>` at all (the
//! headline #7765 case), and a same-repo PR's head can simply not have been
//! fetched yet by the plain-name fetch that precedes this call (a variant of
//! #4823).
//!
//! The answer has to distinguish **four** outcomes, not two, because two of
//! them are silently collapsed into the same "no PR" bucket by a careless
//! read of a failed `gh`/`loom-daemon forge` call:
//!
//! - [`Status::Found`] — an open PR head-matches the branch. The caller then
//!   decides fork vs. same-repo and either refuses or fetches
//!   `refs/pull/<n>/head`.
//! - [`Status::NotFound`] — the forge was reachable and confirmed clean.
//!   Proceed.
//! - [`Status::NoForgeRemote`] — no remote on this repo could plausibly be a
//!   forge (every remote URL is a local filesystem path, or there are none at
//!   all — a throwaway/offline clone), so there is nothing to shadow and no
//!   query worth making. Proceed. Also covers `gh`'s own "no known GitHub
//!   host" refusal and `loom-daemon forge`'s `EX_FORGE_DECLINED` (a Gitea
//!   remote this GitHub-shaped query has no answer for).
//! - [`Status::Unavailable`] — the query itself failed for a reason that is
//!   NOT evidence of "nothing to shadow" (auth, rate limit, network). The
//!   caller refuses rather than guess "safe" — silently proceeding here *is*
//!   the #7765 defect.
//!
//! `NoForgeRemote` and `Unavailable` must never collapse into each other in
//! either direction: the #7863 regression was exactly that, in one
//! direction (an unauthenticated `gh` on a local-only clone bails out before
//! it can say "no known GitHub host", so its failure text carried none of
//! the `NoForgeRemote` signal and was misfiled as `Unavailable` — refusing
//! every worktree creation on every hermetic test suite that builds a
//! synthetic `origin` this way).
//!
//! # Why Rust and not the file that already holds this
//!
//! `lib/worktree-forge-pr-check.sh` is not itself frozen by the file-size
//! ratchet (472 lines, under the 1000-line threshold `scripts/
//! file-size-baseline.txt` tracks) — but it is exactly the class of code the
//! sibling ports in this family (`stale_ref`, `closed_pr_branch`) exist to
//! retire: a forge round-trip's success/failure text parsed by `grep -qi`
//! against two hand-copied substrings, and a `jq` pipeline reparsing the
//! result field by field. Neither half is destructive — this whole family is
//! a REFUSAL/DIAGNOSIS guard, never a `rm -rf` or a `git branch -D` — but the
//! text-matching is exactly the "review cannot see it" fragility the issue's
//! own defect history is about: get the substring wrong and the guard is
//! silently inert on the failure text a real forge actually emits, with no
//! test able to distinguish "the guard ran and passed" from "the guard never
//! matched".
//!
//! # What did NOT move
//!
//! [`Status`] answers a QUESTION; it does not build the refusal. Building the
//! refusal — the `jq -cn` documents `_worktree_guard_fresh_branch_against_open_pr`
//! constructs, and the #9109 invariant that every one of them is built with
//! `jq`, never string concatenation — stays in
//! `lib/worktree-forge-pr-check.sh`. `test-worktree-forge-pr-check.sh`'s Test
//! 10 greps that file for exactly this: every `>&3` emitter must be `jq -cn`
//! or the one whitelisted `loom-daemon` passthrough
//! (`worktree-closed-pr-branch`). Moving the refusal-building into this
//! subcommand too would either break that audit (a second whitelisted name)
//! or require rewriting its regex — for a message-formatting concern with no
//! defect history of its own. The forge round-trip is where the actual bugs
//! have lived, so that is the whole scope of this slice.
//!
//! # One deliberate divergence, argued
//!
//! **No `jq` dependency.** The shell probe treats a missing `jq` as
//! [`Status::NoForgeRemote`] because that is how it would have parsed the
//! forge's JSON. This port uses `serde_json` natively, so a `jq`-less host
//! now gets a real answer instead of a guess. That can only move the verdict
//! *toward* more evidence — same argument [`super::branch_landed`]'s module
//! doc makes for the identical divergence, and no retained suite exercises a
//! `jq`-less host for this specific check (`test-worktree-forge-pr-check.sh`'s
//! ten scenarios all have `jq` on `PATH`, the same environment every
//! `defaults/scripts/tests/` suite runs in).
//!
//! # Command selection
//!
//! Mirrors [`super::closed_pr_branch::forge_probe`] and
//! [`super::branch_landed::forge_probe`]: `loom-daemon forge` when one is on
//! `PATH` (the Gitea passthrough), else `gh`. We *are* loom-daemon, and
//! calling the in-process forge client would be the obvious shortcut — not
//! taken, for the same reason those two record: which credential path a probe
//! uses is observable forge behaviour, not an implementation detail.
//!
//! # Exit-code contract (the CLI wrapper, not this module)
//!
//! Always 0 — this is a QUERY, never a refusal. `worktree-open-pr` never
//! fails the caller, mirroring the shell helper's own "Never fails the
//! caller — always returns 0" contract. `worktree.sh`'s dispatch probes
//! `--help` first and falls through to the (unchanged) shell body when a
//! resolvable daemon does not know this subcommand — the same "optional,
//! always-taken-path" shape [`super::lock`] established in slice 7, chosen
//! here for the same reason: this arm runs on every `worktree.sh <N>` that
//! creates a genuinely NEW branch, so a hard dependency would strand that
//! path on any host running a daemon that predates this slice.

use std::path::Path;
use std::process::Command;

/// What the forge round-trip decided. See the module doc for the four-way
/// distinction and why it cannot collapse to fewer states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Found,
    NotFound,
    NoForgeRemote,
    Unavailable,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Found => "found",
            Status::NotFound => "not_found",
            Status::NoForgeRemote => "no_forge_remote",
            Status::Unavailable => "unavailable",
        }
    }
}

/// The matching PR, populated only when [`Status::Found`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoundPr {
    pub number: String,
    pub is_cross_repo: bool,
    pub head_repo: String,
    pub head_ref: String,
    pub url: String,
}

/// One forge round-trip's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub status: Status,
    pub pr: Option<FoundPr>,
}

impl Probe {
    fn status_only(status: Status) -> Self {
        Self { status, pr: None }
    }
}

/// Does this repository have ANY git remote that could plausibly be a forge?
///
/// Mirrors `_worktree_repo_has_forge_remote` exactly: a URL with a host
/// component (`scheme://…` or an scp-like `user@host:path`) counts as
/// "possibly a forge"; a URL that is unambiguously a local filesystem path
/// (absolute, relative, `~`-relative, or `file://`) does not. A repo with no
/// remotes at all has nothing to shadow either.
#[must_use]
pub fn has_forge_remote(repo: &Path) -> bool {
    let Ok(out) = Command::new("git").arg("remote").current_dir(repo).output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    for name in String::from_utf8_lossy(&out.stdout).lines() {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let url = Command::new("git")
            .args(["remote", "get-url", name])
            .current_dir(repo)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if could_be_forge(&url) {
            return true;
        }
    }
    false
}

/// Pure classification of one remote URL — separated from [`has_forge_remote`]
/// so the rule is testable without a git process.
#[must_use]
fn could_be_forge(url: &str) -> bool {
    if url.is_empty()
        || url.starts_with("file://")
        || url.starts_with('/')
        || url.starts_with("./")
        || url.starts_with("../")
        || url.starts_with("~/")
    {
        return false;
    }
    if url.contains("://") {
        return true;
    }
    // scp-like `user@host:path` — the shell's `*@*:*` glob: an `@` somewhere,
    // then a `:` somewhere after it.
    if let Some(at) = url.find('@') {
        if url[at..].contains(':') {
            return true;
        }
    }
    false
}

/// Ask the forge whether an OPEN PR head-matches `branch`. Never fails the
/// caller — every unreachable/unreadable shape is a [`Status`], not an
/// `Err`.
#[must_use]
pub fn query(repo: &Path, branch: &str) -> Probe {
    if branch.is_empty() {
        return Probe::status_only(Status::Unavailable);
    }
    if !has_forge_remote(repo) {
        return Probe::status_only(Status::NoForgeRemote);
    }
    let (program, leading): (&str, &[&str]) = if on_path("loom-daemon") {
        ("loom-daemon", &["forge"])
    } else if on_path("gh") {
        ("gh", &[])
    } else {
        return Probe::status_only(Status::Unavailable);
    };
    let out = Command::new(program)
        .args(leading)
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--head",
            branch,
            "--json",
            "number,isCrossRepository,headRepository,headRefName,url",
            "--limit",
            "5",
        ])
        .current_dir(repo)
        .output();
    let Ok(out) = out else {
        return Probe::status_only(Status::Unavailable);
    };
    if !out.status.success() {
        return Probe::status_only(classify_failure(&String::from_utf8_lossy(&out.stderr)));
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

/// Classify a failed forge call's stderr. Mirrors the shell's two `grep -qi`
/// checks exactly, in the same order:
///
///   - `gh`'s own "no remote points at a known GitHub host" refusal
///   - `loom-daemon forge`'s `EX_FORGE_DECLINED` text for a forge (Gitea)
///     this GitHub-shaped query has no answer for
///
/// Anything else — auth failure, rate limit, network outage, an
/// unauthenticated `gh` that bails out before it can even say which of the
/// above applies (#7863) — is [`Status::Unavailable`]: genuinely unknown,
/// and the caller must refuse rather than guess.
#[must_use]
fn classify_failure(stderr: &str) -> Status {
    let lower = stderr.to_ascii_lowercase();
    let no_known_github_host = lower.contains(
        "none of the git remotes configured for this repository point to a known github host",
    );
    let gitea_declined = lower.contains("is not handled natively");
    if no_known_github_host || gitea_declined {
        Status::NoForgeRemote
    } else {
        Status::Unavailable
    }
}

/// Parse the forge's `--json number,isCrossRepository,headRepository,
/// headRefName,url` array. Unparseable output degrades to
/// [`Status::Unavailable`] — "the forge said nothing matches" and "we could
/// not read the answer" must not collapse, even though the shell's original
/// `jq` pipeline happened to treat an unparseable `length` the same as
/// leaving the initial `unavailable` default in place (which is what this
/// mirrors, byte for byte in outcome).
#[must_use]
fn parse_probe(text: &str) -> Probe {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return Probe::status_only(Status::Unavailable);
    };
    let Some(items) = value.as_array() else {
        return Probe::status_only(Status::Unavailable);
    };
    let Some(first) = items.first() else {
        return Probe::status_only(Status::NotFound);
    };
    let number = first
        .get("number")
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let is_cross_repo = first
        .get("isCrossRepository")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let head_repo = first
        .get("headRepository")
        .and_then(|v| v.get("nameWithOwner"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let head_ref = first
        .get("headRefName")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let url = first
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Probe {
        status: Status::Found,
        pr: Some(FoundPr {
            number,
            is_cross_repo,
            head_repo,
            head_ref,
            url,
        }),
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

/// Run the query and print the `TOKEN<TAB>text` record stream
/// `lib/worktree-forge-pr-check.sh` replays into its `_WT_OPEN_PR_*` globals.
/// Always exits 0 — see the module doc's exit-code contract.
pub fn run(repo: &Path, branch: &str) -> i32 {
    let probe = query(repo, branch);
    println!("STATUS\t{}", probe.status.as_str());
    if let Some(pr) = probe.pr {
        println!("NUMBER\t{}", pr.number);
        println!("CROSS_REPO\t{}", pr.is_cross_repo);
        println!("HEAD_REPO\t{}", pr.head_repo);
        println!("HEAD_REF\t{}", pr.head_ref);
        println!("URL\t{}", pr.url);
    }
    0
}

#[cfg(test)]
mod tests;
