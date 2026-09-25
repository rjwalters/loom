//! The automated remedy for the required-check freshness guard (#8248),
//! plus its bounded escalation to a durable operator hold (#8508).
//!
//! # Why this exists
//!
//! #8248's guard is correct: a green required check whose evidence predates
//! the base branch's current tip must not be trusted. But nothing in the
//! fleet automatically produces FRESH evidence once the guard fires. On a
//! busy `main`, PR #8493 failed three consecutive Champion merge attempts
//! (2026-09-21) with the identical refusal, because:
//!
//! - the PR's branch had no new commits, so no CI run ever re-dated its
//!   checks;
//! - `main` kept advancing, so every retry compared against an even later
//!   base tip;
//! - both `merge-pr.sh`'s internal check re-run and a direct `gh run rerun`
//!   failed with `Resource not accessible by integration` — the installation
//!   token has no `actions:write`.
//!
//! The only escapes on record were a human merging with an elevated token, or
//! a human pushing a no-op commit by hand. Neither happens automatically, so
//! a Judge-approved, safety-criteria-clean PR could sit blocked forever with
//! no visible record once Champion's own idempotency guard suppressed the
//! third near-identical failure comment.
//!
//! # The remedy
//!
//! `actions:write` (re-running a workflow) is not the only way to produce a
//! fresh check run: pushing ANY commit re-triggers every `pull_request`
//! workflow on the new head, which is exactly [`super::stale_checks::
//! stale_message`]'s own documented remedy ("push any no-op commit to re-date
//! every check"). `merge-pr.sh` already exercises `contents: write` on the
//! head branch — `head_sync`'s self-sync push lands there via the same token —
//! so this needs no new grant.
//!
//! This module creates a commit that repoints at the SAME tree as the current
//! head (so the diff Judge reviewed is byte-for-byte unchanged) with ONE new
//! parent, and fast-forwards the branch ref onto it via the Git Data API
//! (`git/commits` + `git/refs/heads/<branch>`) rather than a local clone —
//! `merge-pr.sh`'s own "worktree-safe, API-only" discipline.
//!
//! # Why the remedy is bounded, and what happens when the bound is reached
//!
//! The remedy is not unconditionally repeatable. If CI on the re-dated head
//! takes longer than the interval between merges on `main`, the guard is stale
//! again the moment it finishes, and an unbounded remedy would push a fresh
//! no-op commit every tick forever — burning a full CI run and a Judge
//! re-review each time while never out-racing `main`.
//!
//! So the bound is one remedy per head: every push records
//! `<!-- loom:stale-check-redate to=<new-sha> -->` on the PR, and finding that
//! marker for the CURRENT head means "we already re-dated, CI ran, and the
//! guard STILL blocks — nothing automated is making progress". That is #8508's
//! bounded signal, evaluated from durable forge state rather than from a tick
//! counter no process owns. It is also strictly stronger than counting ticks:
//! it can only fire when a full re-date → CI → block cycle has completed with
//! no forward progress.
//!
//! Reaching the bound escalates exactly the way `champion-pr-merge.md`'s
//! merge-risk hold does: one idempotent rationale comment (keyed on the head,
//! so a later push re-opens the question) plus the `loom:operator` label — the
//! first-class "engine will not act further, a human is the only transition
//! out" state. Without it the blocked PR is invisible: Champion's own
//! rejection-comment idempotency guard suppresses the repeat failures, and the
//! refusal is only ever seen in a log nobody reads.
//!
//! # Concurrency
//!
//! The caller supplies `expected_head_sha` — the SHA the blocked merge
//! attempt actually gated on. Before writing anything, this module re-reads
//! the branch's current ref and refuses to proceed if it has already moved:
//! a foreign push already changed the tree (Judge's approval is stale
//! regardless, per #5686) and a second, unrelated no-op commit on top would
//! only add noise. [`RemedyOutcome::HeadMoved`] mirrors `merge-pr.sh`'s own
//! #5579 exit-3 contract ("not a failure, re-queue and re-evaluate fresh").
//!
//! # What this deliberately does NOT do
//!
//! It never asserts a check passed, fabricates a check run, or otherwise
//! forges evidence — it only produces a new commit for CI to run against for
//! real. The #8248 guard is left completely unweakened: the NEXT merge attempt
//! still requires a check that actually started at/after the (possibly
//! still-moving) base tip. It also moves the head SHA, which — correctly, per
//! #5686 — invalidates any standing Judge approval; the PR is expected to
//! cycle back through Judge once CI on the new head is green, exactly as any
//! other push would.

use std::io::Write;
use std::process::{Command, Stdio};

/// The `gh` binary, honoring `LOOM_GH_BIN` — the same seam
/// `stale_checks::fetch` / `head_sync::fetch` provide.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// `gh api …`, parameterized on the binary — the injection seam this module's
/// tests use so a stub `gh` can be passed as a plain function argument instead
/// of a global `LOOM_GH_BIN` env var, which would race across parallel
/// `cargo test` threads in the same process. [`gh_bin`]'s env lookup stays the
/// CLI's own default; only tests take this path directly.
pub(crate) fn gh_api_with(bin: &str, args: &[&str]) -> Result<String, String> {
    gh_api_body(bin, args, None)
}

/// [`gh_api_with`] with an optional JSON request body on stdin.
///
/// Writes are sent as `--input -` JSON rather than repeated `-f key=value`
/// flags: every body here is multi-line markdown, and `-f`'s shell-adjacent
/// quoting has already corrupted comment bodies elsewhere in this repo.
/// `serde_json` escaping is the only encoder in the path.
fn gh_api_body(bin: &str, args: &[&str], body: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.arg("api").args(args);
    if body.is_some() {
        cmd.arg("--input").arg("-").stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not exec gh api: {e}"))?;
    if let Some(payload) = body {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| "gh api stdin was not piped".to_string())?
            .write_all(payload.as_bytes())
            .map_err(|e| format!("could not write the gh api request body: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("could not read gh api output: {e}"))?;
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
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit message every re-date commit carries — deterministic and
/// greppable, so a later pass (or a human) can tell "this is an automated
/// remedy commit" from the commit log alone without reading this source.
#[must_use]
pub fn commit_message(pr: &str) -> String {
    format!(
        "chore: re-date required checks for PR #{pr} (#8248 guard, automated by #8508)\n\n\
This commit intentionally changes NOTHING in the tree — it exists only to \
give every required check a fresh started_at, because the #8248 \
required-check-freshness guard blocked this merge on evidence that predates \
the base branch's current tip, and the merge token lacks actions:write to \
re-run the stale check directly."
    )
}

/// The marker a successful push records, naming the sha it created.
///
/// Read back on a later tick as "the remedy already ran against this head" —
/// see the module header's bounding rule.
#[must_use]
pub fn redate_marker(new_sha: &str) -> String {
    format!("<!-- loom:stale-check-redate to={new_sha} -->")
}

/// The escalation notice's per-head idempotency marker. Keyed on the head so
/// a later push (human or Doctor) re-opens the question with a fresh notice
/// instead of being silenced by the previous episode's hold.
#[must_use]
pub fn hold_marker(head_sha: &str) -> String {
    format!("<!-- loom:stale-check-hold head={head_sha} -->")
}

/// The label the escalation applies: the first-class "a human is needed"
/// state (#5502), the same one `champion-pr-merge.md`'s merge-risk hold uses.
pub const HOLD_LABEL: &str = "loom:operator";

/// Whether a re-date push may proceed against the evidence the caller gated
/// on, or the branch already moved out from under it.
///
/// Pure and unit-testable, deliberately split from the forge I/O — mirrors
/// `merge-pr.sh`'s own #5579 "gate on a fresh read, refuse to act on a stale
/// one" discipline.
#[derive(Debug, Clone, PartialEq)]
pub enum PushDecision {
    /// The branch's current head still matches what the caller gated on —
    /// safe to push the re-date commit on top of it.
    Proceed,
    /// The branch has already moved; pushing here would land on a tree
    /// nobody gated this decision on. Not a failure — see
    /// [`RemedyOutcome::HeadMoved`].
    HeadMoved { current: String },
}

#[must_use]
pub fn decide(expected_head_sha: &str, current_head_sha: &str) -> PushDecision {
    if expected_head_sha == current_head_sha {
        PushDecision::Proceed
    } else {
        PushDecision::HeadMoved {
            current: current_head_sha.to_string(),
        }
    }
}

/// Has the remedy already run against exactly this head?
///
/// `comments` is the PR's comment bodies as one blob; the markers are
/// single-line HTML comments, so a substring test over the concatenation is
/// equivalent to a per-comment scan and costs one API read instead of N.
#[must_use]
pub fn already_redated(comments: &str, head_sha: &str) -> bool {
    comments.contains(&redate_marker(head_sha))
}

/// Has the escalation notice for exactly this head already been posted?
#[must_use]
pub fn hold_already_posted(comments: &str, head_sha: &str) -> bool {
    comments.contains(&hold_marker(head_sha))
}

/// The comment recorded after a successful push.
#[must_use]
pub fn redate_comment_body(pr: &str, new_sha: &str) -> String {
    let short = &new_sha[..new_sha.len().min(7)];
    format!(
        "{}\n**Automated re-date of stale required checks (#8508)**\n\n\
The #8248 required-check-freshness guard blocked the merge of PR #{pr}: a required \
check's green result predates the current base-branch tip, and this repo's merge token \
has no `actions:write` to re-run that check directly. Pushed a **tree-identical** no-op \
commit (`{short}`) instead — the diff is byte-for-byte unchanged, but every required \
check now re-runs against a current timestamp.\n\n\
This moves the head SHA, which invalidates the standing Judge approval (#5686). Expect \
this PR to cycle back through `loom:review-requested` once CI on `{short}` completes, \
then merge normally once re-approved.\n\n\
The #8248 guard itself is unchanged and still applies to the next merge attempt.",
        redate_marker(new_sha)
    )
}

/// The escalation notice: the durable record #8508 asks for, so a stuck PR is
/// visible on the PR itself rather than only in a Champion log line that the
/// rejection-comment idempotency guard has since suppressed.
#[must_use]
pub fn hold_comment_body(pr: &str, head_sha: &str, reason: &str) -> String {
    let short = &head_sha[..head_sha.len().min(7)];
    format!(
        "{}\n**Merge held: the #8248 required-check-freshness guard has no automated remedy left**\n\n\
PR #{pr} is blocked at head `{short}` by the required-check-freshness guard (#8248), and the \
automated remedy (#8508) cannot make further progress: {reason}\n\n\
Adding `{HOLD_LABEL}` — a human is the only transition out of this state. Either:\n\n\
- merge it yourself with a token that can re-run the stale check (`actions:write`) or that \
carries elevated merge permission, or\n\
- push any commit to this branch, which re-dates every required check and returns the PR to \
the normal Judge -> Champion path.\n\n\
Remove `{HOLD_LABEL}` once you have acted; the guard itself is correct and is deliberately \
not being bypassed here.",
        hold_marker(head_sha)
    )
}

/// The result of attempting the remedy.
#[derive(Debug, Clone, PartialEq)]
pub enum RemedyOutcome {
    /// A new, tree-identical commit was pushed as the branch's new tip.
    Pushed { new_sha: String },
    /// The remedy had already run against this exact head and the guard still
    /// blocks: the bound is reached, and the PR was escalated to a durable
    /// `loom:operator` hold. `notice_posted` is false when a notice for this
    /// head was already on the PR (idempotency), true when this call posted it.
    Escalated { notice_posted: bool },
    /// The branch already moved past `expected_head_sha` before this ran —
    /// no push was attempted. Mirrors `merge-pr.sh`'s #5579 exit-3 contract:
    /// re-evaluate fresh next tick, do not treat this as an error.
    HeadMoved { current: String },
    /// Could not read or write the forge state needed to run the remedy.
    Failed(String),
}

/// Run the remedy for `pr`: push a tree-identical commit onto `branch`'s
/// current tip, or — when that has already been done for this exact head —
/// escalate to a durable operator hold.
///
/// `nwo` is `owner/repo`.
pub fn remedy(nwo: &str, branch: &str, expected_head_sha: &str, pr: &str) -> RemedyOutcome {
    remedy_with(&gh_bin(), nwo, branch, expected_head_sha, pr)
}

/// [`remedy`]'s implementation, parameterized on the `gh` binary — the
/// injection seam the test suite drives directly (see [`gh_api_with`]).
fn remedy_with(
    gh: &str,
    nwo: &str,
    branch: &str,
    expected_head_sha: &str,
    pr: &str,
) -> RemedyOutcome {
    let current = match read_nonempty(
        gh,
        &[
            &format!("repos/{nwo}/git/refs/heads/{branch}"),
            "--jq",
            ".object.sha",
        ],
        &format!("ref heads/{branch} resolved to an empty sha"),
    ) {
        Ok(s) => s,
        Err(e) => return RemedyOutcome::Failed(e),
    };

    match decide(expected_head_sha, &current) {
        PushDecision::HeadMoved { current } => return RemedyOutcome::HeadMoved { current },
        PushDecision::Proceed => {}
    }

    // Durable attempt state, read before any write: one remedy per head.
    let comments = match gh_api_with(
        gh,
        &[
            &format!("repos/{nwo}/issues/{pr}/comments"),
            "--paginate",
            "--jq",
            ".[].body",
        ],
    ) {
        Ok(s) => s,
        Err(e) => return RemedyOutcome::Failed(format!("could not read PR #{pr}'s comments: {e}")),
    };

    if already_redated(&comments, &current) {
        return escalate(gh, nwo, pr, &current, &comments);
    }

    let tree = match read_nonempty(
        gh,
        &[
            &format!("repos/{nwo}/git/commits/{current}"),
            "--jq",
            ".tree.sha",
        ],
        &format!("commit {current} resolved to an empty tree sha"),
    ) {
        Ok(s) => s,
        Err(e) => return RemedyOutcome::Failed(e),
    };

    let message_arg = format!("message={}", commit_message(pr));
    let tree_arg = format!("tree={tree}");
    let parent_arg = format!("parents[]={current}");
    let new_sha = match read_nonempty(
        gh,
        &[
            &format!("repos/{nwo}/git/commits"),
            "-f",
            &message_arg,
            "-f",
            &tree_arg,
            "-f",
            &parent_arg,
            "--jq",
            ".sha",
        ],
        "commit creation returned an empty sha",
    ) {
        Ok(s) => s,
        Err(e) => {
            return RemedyOutcome::Failed(format!("could not create the re-date commit: {e}"))
        }
    };

    let sha_arg = format!("sha={new_sha}");
    if let Err(e) = gh_api_with(
        gh,
        &[
            "-X",
            "PATCH",
            &format!("repos/{nwo}/git/refs/heads/{branch}"),
            "-f",
            &sha_arg,
            "-F",
            "force=false",
        ],
    ) {
        return RemedyOutcome::Failed(format!(
            "commit {new_sha} was created but updating heads/{branch} to it failed \
(the commit is dangling, not on any branch, and will be garbage-collected): {e}"
        ));
    }

    // Record the attempt LAST: a marker without a landed push would bound the
    // remedy out of existence on the next tick for a push that never happened.
    if let Err(e) = post_comment(gh, nwo, pr, &redate_comment_body(pr, &new_sha)) {
        return RemedyOutcome::Failed(format!(
            "the re-date commit {new_sha} landed on heads/{branch}, but recording it on PR #{pr} \
failed ({e}) — the next tick cannot tell the remedy already ran and may push a second one; \
re-run once the forge is writable, or post {} by hand",
            redate_marker(&new_sha)
        ));
    }

    RemedyOutcome::Pushed { new_sha }
}

/// The bound is reached: post the notice (once per head) and apply the hold
/// label. Label application is idempotent at the forge, so it is re-asserted
/// even when the notice was already present — the label is what actually
/// parks the PR, and a hand-removed label with an unresolved block would
/// otherwise never come back.
fn escalate(gh: &str, nwo: &str, pr: &str, head: &str, comments: &str) -> RemedyOutcome {
    let reason =
        "an automated re-date commit was already pushed for this head and its CI has run, \
yet the guard still reports the required checks stale — the base branch is moving faster than CI \
can re-date them, so another no-op commit would not help";
    let notice_posted = if hold_already_posted(comments, head) {
        false
    } else {
        if let Err(e) = post_comment(gh, nwo, pr, &hold_comment_body(pr, head, reason)) {
            return RemedyOutcome::Failed(format!("could not post the #8508 hold notice: {e}"));
        }
        true
    };
    if let Err(e) = gh_api_body(
        gh,
        &[&format!("repos/{nwo}/issues/{pr}/labels")],
        Some(&format!("{{\"labels\":[\"{HOLD_LABEL}\"]}}")),
    ) {
        return RemedyOutcome::Failed(format!("could not apply {HOLD_LABEL} to PR #{pr}: {e}"));
    }
    RemedyOutcome::Escalated { notice_posted }
}

fn post_comment(gh: &str, nwo: &str, pr: &str, body: &str) -> Result<String, String> {
    let payload = serde_json::json!({ "body": body }).to_string();
    gh_api_body(gh, &[&format!("repos/{nwo}/issues/{pr}/comments")], Some(&payload))
}

/// `gh_api_with` where an empty answer is itself a failure — every read here
/// feeds a write decision, and an empty SHA must never reach one.
fn read_nonempty(gh: &str, args: &[&str], empty_msg: &str) -> Result<String, String> {
    match gh_api_with(gh, args) {
        Ok(s) if !s.is_empty() => Ok(s),
        Ok(_) => Err(empty_msg.to_string()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests;
