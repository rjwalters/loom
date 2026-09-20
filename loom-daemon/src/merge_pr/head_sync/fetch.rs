//! The live-forge inputs for the self-sync head attribution guard (#8164).
//!
//! Three read-only `gh` calls, REST-first exactly where `forge-helpers.sh`
//! already treads, so the shell and the port agree on API shape:
//!
//! 1. **the PR** — `GET /repos/{nwo}/pulls/{n}`: the current head SHA (the
//!    same uncached read `forge_get_pr_nocache` performs, because a cached
//!    one is precisely what the incident's stale precondition was) and the
//!    base ref.
//! 2. **the head commit** — `GET /repos/{nwo}/commits/{head_sha}`: its
//!    parents, in order. The commit *message* is deliberately not read: see
//!    the module header on why attribution is structural, never textual.
//! 3. **containment** — `GET /repos/{nwo}/compare/{second_parent}...{base_ref}`:
//!    a `status` of `ahead` or `identical` means the base branch already
//!    contains the second parent, i.e. the merge that moved the head brought
//!    in base content and nothing else.
//!
//! Every failure is an `Err`, and every caller outcome of an `Err` is "do not
//! retry, re-queue" — never "assume it was our own sync". A guard that cannot
//! establish attribution has not established it.

use std::process::Command;

/// The `gh` binary, honoring `LOOM_GH_BIN` — the same seam
/// `stale_checks::fetch` and `forge_cmd::gh_bin` provide.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

fn gh_api(args: &[&str]) -> Result<String, String> {
    let out = Command::new(gh_bin())
        .arg("api")
        .args(args)
        .output()
        .map_err(|e| format!("could not exec gh api: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let trimmed = stderr.trim();
        let hint = if trimmed.is_empty() {
            format!("exit {}", out.status)
        } else {
            trimmed.to_string()
        };
        return Err(format!("gh api {} failed: {hint}", args.first().unwrap_or(&"")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What [`super::classify`] needs from the forge about the current head.
pub struct LiveInputs {
    pub current_head_sha: String,
    pub base_ref: String,
    pub head_parents: Vec<String>,
    pub second_parent_in_base: Option<bool>,
}

/// The PR's current head SHA and base ref, read uncached.
fn fetch_pr(nwo: &str, pr: &str) -> Result<(String, String), String> {
    let out = gh_api(&[
        &format!("repos/{nwo}/pulls/{pr}"),
        "--jq",
        "[.head.sha, .base.ref] | @tsv",
    ])?;
    let mut cols = out.split('\t');
    let head = cols.next().unwrap_or("").trim().to_string();
    let base = cols.next().unwrap_or("").trim().to_string();
    if head.is_empty() || base.is_empty() {
        return Err(format!("PR #{pr} resolved to an empty head SHA or base ref"));
    }
    Ok((head, base))
}

/// The parents of `sha`, in order.
fn fetch_parents(nwo: &str, sha: &str) -> Result<Vec<String>, String> {
    let out = gh_api(&[
        &format!("repos/{nwo}/commits/{sha}"),
        "--jq",
        ".parents[].sha",
    ])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Is `sha` already contained in `base_ref`?
///
/// `compare/{sha}...{base_ref}` describes the base ref *relative to* `sha`:
/// `ahead` (base has moved on past it) and `identical` both mean `sha` is
/// reachable from the base branch. `behind` and `diverged` mean it is not.
fn fetch_containment(nwo: &str, sha: &str, base_ref: &str) -> Result<bool, String> {
    let out = gh_api(&[
        &format!("repos/{nwo}/compare/{sha}...{base_ref}"),
        "--jq",
        ".status",
    ])?;
    match out.trim() {
        "ahead" | "identical" => Ok(true),
        "behind" | "diverged" => Ok(false),
        other => Err(format!(
            "comparing {sha} with {base_ref} returned an unrecognized status {other:?}"
        )),
    }
}

/// Gather everything [`super::classify`] needs about the live head.
///
/// A commit with fewer than two parents short-circuits: `classify` refuses it
/// on arity alone, and asking the forge to compare a parent that does not
/// exist would report a lookup failure as if it were the interesting one.
pub fn live_inputs(nwo: &str, pr: &str) -> Result<LiveInputs, String> {
    let (current_head_sha, base_ref) = fetch_pr(nwo, pr)?;
    let head_parents = fetch_parents(nwo, &current_head_sha)?;
    let second_parent_in_base = match head_parents.get(1) {
        Some(p) => Some(fetch_containment(nwo, p, &base_ref)?),
        None => None,
    };
    Ok(LiveInputs {
        current_head_sha,
        base_ref,
        head_parents,
        second_parent_in_base,
    })
}
