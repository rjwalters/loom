//! One "Landing order recorded" comment per follower, upserted by marker
//! (#10634).
//!
//! # The incident
//!
//! On 2026-10-05 one PR got the same "lands AFTER #214" comment thirteen
//! times in seven hours, twice in the same second, and about 30 PRs in that
//! repo got the same series. Two causes compounded:
//!
//! - **Plan-id churn.** The apply side's idempotency check was "is this exact
//!   `loom:sequence` marker line already on the thread". The line carries the
//!   component's `plan=` id, a hash over EVERY member's head, so any push to
//!   any PR in a 30-PR component minted a new id and a fresh comment for every
//!   follower, even though its own order (after the same PR, at the same
//!   heads) had not changed. The label never landing in that repo (its
//!   `--add-label` failed) kept every follower out of the holder set, so the
//!   next tick tried again.
//! - **Two writers.** Every host runs the pass on every workspace, so two
//!   hosts that read the same thread both posted.
//!
//! # What this module does
//!
//! Every comment the pass writes now carries a hidden key line,
//! `<!-- loom:landing-order v1 after=N after_head=<sha> follower_head=<sha> -->`:
//! the follower's order (after PR N), pinned at both heads. The `plan=` id is
//! deliberately NOT part of the key. A comment written before the key existed
//! is recognized by its heading and its `source=pass` marker, so the threads
//! already carrying a series converge too.
//!
//! [`plan`] decides from the follower's trusted comments, read once per pass
//! (the same listing the edge already read for the sticky-release check — no
//! extra read):
//!
//! - **Same key, nothing after it** → no comment write. The existing comment's
//!   own marker (its older `plan=` id included) is the marker of record.
//! - **Key changed, nothing after it** → PATCH that comment in place.
//! - **No landing comment**, or the newest one is followed by a sequencing
//!   state line (a newer marker, a `released` / `replanned` tombstone or an
//!   operator-release record) → post one new comment. Editing a comment that
//!   history has moved past would put the live marker *before* a tombstone,
//!   which `parse_live` and the #10398 sticky-release reader would then read
//!   as already ended.
//! - **Duplicates** → an older landing comment of ours whose key a newer one
//!   repeats is deleted (at most [`MAX_DELETES_PER_PR`] per PR per pass). This
//!   is the race repair: two hosts that both posted converge on the next pass.
//!
//! Only comments authored by a fleet identity are edited or deleted; a human's
//! words are never rewritten.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::{apply_comment_body, gh_call, gh_pr, EdgeReason, SequenceMarker, SOURCE_PASS};
use crate::merge_pr::sequence::{html_comment_spans, is_full_sha, parse, states_or_ends_hold};

/// The key line's prefix. Versioned so a later key shape can coexist.
pub const LANDING_MARKER_PREFIX: &str = "loom:landing-order v1";

/// The heading every landing comment has carried since #9686 — how a comment
/// written before the key line existed is recognized.
const LEGACY_HEADING: &str = "**Landing order recorded**";

/// Cap on duplicate deletions per PR per pass, so a thread with a long
/// pre-#10634 series is cleaned over a few passes instead of in one burst on
/// a shared, already-strained API bucket.
pub const MAX_DELETES_PER_PR: usize = 5;

/// A follower's order: after PR `after`, pinned at both heads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingKey {
    pub after: u32,
    pub after_head: String,
    pub follower_head: String,
}

impl LandingKey {
    /// The key a sequencing marker describes.
    #[must_use]
    pub fn of(marker: &SequenceMarker) -> Self {
        Self {
            after: marker.after,
            after_head: marker.pred_head.clone(),
            follower_head: marker.follower_head.clone(),
        }
    }
}

/// The hidden key line.
#[must_use]
pub fn landing_marker_text(key: &LandingKey) -> String {
    format!(
        "<!-- {LANDING_MARKER_PREFIX} after={} after_head={} follower_head={} -->",
        key.after, key.after_head, key.follower_head
    )
}

fn parse_key_span(span: &str) -> Option<LandingKey> {
    let rest = span.trim().strip_prefix(LANDING_MARKER_PREFIX)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (mut after, mut after_head, mut follower_head) = (None, None, None);
    for field in rest.split_whitespace() {
        match field.split_once('=')? {
            ("after", v) => after = v.parse::<u32>().ok().filter(|n| *n > 0),
            ("after_head", v) => after_head = is_full_sha(v).then(|| v.to_string()),
            ("follower_head", v) => follower_head = is_full_sha(v).then(|| v.to_string()),
            _ => return None,
        }
    }
    Some(LandingKey {
        after: after?,
        after_head: after_head?,
        follower_head: follower_head?,
    })
}

/// The full comment the pass writes: the key line, then the #9686 body
/// (which carries the `loom:sequence` marker the gate reads).
#[must_use]
pub fn landing_comment_body(marker: &SequenceMarker, reason: EdgeReason) -> String {
    format!(
        "{}\n{}",
        landing_marker_text(&LandingKey::of(marker)),
        apply_comment_body(marker, reason)
    )
}

/// One trusted comment on the follower's thread, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadComment {
    pub id: Option<u64>,
    pub author: Option<String>,
    pub body: String,
}

/// The planner's view of a trusted REST comment listing.
#[must_use]
pub fn thread_comments(trusted: &[Value]) -> Vec<ThreadComment> {
    trusted
        .iter()
        .filter_map(|v| {
            Some(ThreadComment {
                id: v.get("id").and_then(Value::as_u64),
                author: v
                    .pointer("/user/login")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                body: v.get("body")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// The bodies, for the readers that take only those.
#[must_use]
pub fn bodies(thread: &[ThreadComment]) -> Vec<String> {
    thread.iter().map(|c| c.body.clone()).collect()
}

/// The follower's trusted comments: ONE paginated listing, booked under the
/// same `sequence.trusted_bodies` name the bodies-only read used, so the
/// switch adds no forge call. `None` on any failure — the caller skips the
/// edge rather than read "no comment" and post one.
pub fn fetch_thread(gh_bin: &Path, root: &Path, pr: u32) -> Option<Vec<ThreadComment>> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments?per_page=100");
    let stdout =
        gh_call::ok_stdout(gh_call::read("sequence.trusted_bodies", gh_bin, root).args([
            "api",
            &path,
            "--paginate",
        ]))?;
    let trusted = crate::comment_trust::TrustPolicy::for_root(root).trusted_listing(&stdout)?;
    Some(thread_comments(&trusted))
}

/// `(key, marker)` when `comment` is a landing-order comment the pass wrote.
fn landing_of(comment: &ThreadComment) -> Option<(LandingKey, SequenceMarker)> {
    let marker = parse(std::slice::from_ref(&comment.body))?;
    if marker.source.as_deref() != Some(SOURCE_PASS) {
        return None;
    }
    let keyed = comment
        .body
        .lines()
        .flat_map(html_comment_spans)
        .find_map(parse_key_span);
    match keyed {
        Some(key) => Some((key, marker)),
        None if comment.body.contains(LEGACY_HEADING) => Some((LandingKey::of(&marker), marker)),
        None => None,
    }
}

/// Does `comment` state or end a hold, or record an operator release?
fn carries_sequence_state(comment: &ThreadComment) -> bool {
    comment
        .body
        .lines()
        .flat_map(html_comment_spans)
        .any(|span| {
            states_or_ends_hold(span)
                || span
                    .trim()
                    .starts_with(super::sticky::OPERATOR_RELEASE_PREFIX)
        })
}

/// What happens to the landing comment this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandingWrite {
    /// The thread already says this; no write.
    Keep,
    /// Post one new comment.
    Create,
    /// Rewrite the existing comment with this id.
    Patch(u64),
}

/// The decision for one follower.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingPlan {
    pub write: LandingWrite,
    /// The `loom:sequence` marker that will be on the thread afterwards. On
    /// [`LandingWrite::Keep`] this is the existing comment's own marker.
    pub marker: SequenceMarker,
    /// Superseded duplicates of ours to delete, oldest first.
    pub delete: Vec<u64>,
}

/// Decide the landing-comment write for `want` on a thread. `ours` says
/// whether a login is a fleet identity (only those comments are edited or
/// deleted). Pure — see the module docs for the rules.
#[must_use]
pub fn plan(
    thread: &[ThreadComment],
    want: &SequenceMarker,
    ours: impl Fn(&str) -> bool,
) -> LandingPlan {
    let landings: Vec<(usize, LandingKey, SequenceMarker)> = thread
        .iter()
        .enumerate()
        .filter_map(|(i, c)| landing_of(c).map(|(k, m)| (i, k, m)))
        .collect();
    let mine = |i: usize| -> Option<u64> {
        let c = &thread[i];
        c.id.filter(|_| c.author.as_deref().is_some_and(&ours))
    };
    let delete: Vec<u64> = landings
        .iter()
        .enumerate()
        .filter(|(n, (_, key, _))| landings[n + 1..].iter().any(|(_, k, _)| k == key))
        .filter_map(|(_, (i, _, _))| mine(*i))
        .take(MAX_DELETES_PER_PR)
        .collect();
    let want_key = LandingKey::of(want);
    let fresh = |write| LandingPlan {
        write,
        marker: want.clone(),
        delete: delete.clone(),
    };
    let Some((index, key, marker)) = landings.last() else {
        return fresh(LandingWrite::Create);
    };
    if thread[index + 1..].iter().any(carries_sequence_state) {
        return fresh(LandingWrite::Create);
    }
    if *key == want_key {
        return LandingPlan {
            write: LandingWrite::Keep,
            marker: marker.clone(),
            delete,
        };
    }
    match mine(*index) {
        Some(id) => fresh(LandingWrite::Patch(id)),
        None => fresh(LandingWrite::Create),
    }
}

/// Carry out `plan`'s comment write for `follower`. `Ok(false)` on
/// [`LandingWrite::Keep`].
pub(super) fn write(
    gh_bin: &Path,
    root: &Path,
    follower: u32,
    reason: EdgeReason,
    plan: &LandingPlan,
) -> Result<bool> {
    let body = landing_comment_body(&plan.marker, reason);
    match plan.write {
        LandingWrite::Keep => return Ok(false),
        LandingWrite::Create => {
            gh_pr(gh_bin, root, &["comment", &follower.to_string(), "--body", &body])?;
        }
        LandingWrite::Patch(id) => {
            let field = format!("body={body}");
            comment_api(gh_bin, root, "sequence.comment_patch", id, &["PATCH", "-f", &field])?;
        }
    }
    Ok(true)
}

/// Best-effort duplicate cleanup: a failed delete is logged and retried on
/// a later pass (a 404 from a peer that deleted it first is harmless).
pub(super) fn delete_duplicates(gh_bin: &Path, root: &Path, follower: u32, plan: &LandingPlan) {
    for id in &plan.delete {
        match comment_api(gh_bin, root, "sequence.comment_delete", *id, &["DELETE"]) {
            Ok(()) => log::info!(
                "claim_reconciliation (merge sequence): PR #{follower} in {}: deleted duplicate \
                 landing-order comment {id} (#10634)",
                root.display()
            ),
            Err(e) => log::warn!(
                "claim_reconciliation (merge sequence): PR #{follower} in {}: {e}",
                root.display()
            ),
        }
    }
}

/// `api repos/{owner}/{repo}/issues/comments/<id> --method <args…>`.
fn comment_api(gh_bin: &Path, root: &Path, op: &'static str, id: u64, rest: &[&str]) -> Result<()> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
    let inv = gh_call::write(op, gh_bin, root)
        .args(["api", &path, "--method"])
        .args(rest);
    let out = gh_call::output(inv)?;
    if !out.status.success() {
        anyhow::bail!(
            "{op} on comment {id} failed in {}: {}",
            root.display(),
            gh_call::stderr(&out)
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "merge_sequence_landing_tests.rs"]
mod tests;
