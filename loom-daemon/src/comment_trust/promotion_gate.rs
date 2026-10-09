//! The author gate on automatic promotion (#10827).
//!
//! The comment-trust rule (#9548) decides which *markers* count; on its own it
//! says nothing about who wrote the issue being promoted, so an issue filed by
//! an outside bot or a non-collaborator could flow Curator -> Champion ->
//! `loom:issue` and be built. This is the one predicate every automatic
//! `loom:curated` -> `loom:issue` write asks first (Champion's Step 3b, its
//! Pass 0c backstop `check-promotion-landed.sh --apply`, and Curator's starred
//! Priority 0 promotion):
//!
//! - the issue's **body author** passes the existing comment-trust predicate
//!   ([`super::TrustPolicy::trusts`]: a repo insider, one of this fleet's
//!   Apps, this daemon, `forge.trustedCommenters`, or a fleet admin) ->
//!   [`Gate::Eligible`]. There is no second trust table;
//! - it does not -> [`Gate::Hold`]: the issue stays at `loom:curated` and gets
//!   one explanatory comment ([`notice_body`], deduplicated by
//!   [`NOTICE_MARKER`]). A trusted actor adopts it by applying `loom:issue` by
//!   hand, or by re-filing it;
//! - the inputs could not be read -> [`Gate::Unavailable`]: no promotion and
//!   no comment this pass (fail closed, never spam).
//!
//! Passing the gate is not approval: every other promotion criterion still
//! applies.

use std::path::Path;

use serde_json::Value;

use super::{Author, TrustPolicy};

/// The dedup marker of the one explanatory comment a hold may post.
pub const NOTICE_MARKER: &str = "<!-- loom:promotion-author-gate -->";

/// The gate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The body author is trusted: may be promoted (on the other criteria).
    Eligible(String),
    /// The body author is untrusted: must not be promoted automatically.
    Hold(String),
    /// The inputs could not be read: must not be promoted this pass.
    Unavailable(String),
}

impl Gate {
    /// `ELIGIBLE` / `HOLD` / `UNAVAILABLE`.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Eligible(_) => "ELIGIBLE",
            Self::Hold(_) => "HOLD",
            Self::Unavailable(_) => "UNAVAILABLE",
        }
    }

    /// The reason text (safe to post: it names only the author).
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Eligible(r) | Self::Hold(r) | Self::Unavailable(r) => r,
        }
    }
}

/// A login as it may appear in a posted reason: forge logins are already
/// restricted to this set, anything else is dropped rather than echoed.
fn safe_login(login: &str) -> String {
    login
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '[' | ']' | '/'))
        .take(64)
        .collect()
}

fn describe(author: &Author) -> String {
    let login = author
        .login
        .as_deref()
        .map(safe_login)
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "<unknown>".to_string());
    let assoc = author
        .association
        .as_deref()
        .map(safe_login)
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "no association".to_string());
    format!("`{login}` ({assoc})")
}

/// The gate for one REST issue object (`user`, `author_association`,
/// `number`). `issue` is `None` when it could not be read.
/// `admin_roster_expected` is whether a fleet store is configured: then an
/// unreadable admin roster leaves a user author undecided (one of its admins
/// may have filed the issue), so the answer is [`Gate::Unavailable`] rather
/// than a hold notice.
#[must_use]
pub fn decide(issue: Option<&Value>, policy: &TrustPolicy, admin_roster_expected: bool) -> Gate {
    let Some(issue) = issue.filter(|i| i.is_object()) else {
        return Gate::Unavailable("the issue could not be read".to_string());
    };
    if issue.get("number").and_then(Value::as_u64).is_none() {
        return Gate::Unavailable("the issue object carries no number".to_string());
    }
    let author = Author::from_json(issue);
    if policy.trusts(&author) {
        return Gate::Eligible(format!("trusted body author {}", describe(&author)));
    }
    if author.login.is_none() {
        // No author at all (a deleted account, an unexpected shape) is no one
        // to trust, and no one to name in a notice: hold silently.
        return Gate::Unavailable("the issue's author could not be read".to_string());
    }
    if !author.app && admin_roster_expected && policy.admins_loaded() == Some(false) {
        return Gate::Unavailable(
            "the fleet admin roster could not be read, so a user author is undecided".to_string(),
        );
    }
    Gate::Hold(format!(
        "its author {} is not a trusted author (repo insider, this fleet's App, this daemon, \
         forge.trustedCommenters or a fleet admin)",
        describe(&author)
    ))
}

/// Whether a hold's explanatory comment is already on the issue: a comment
/// starting with [`NOTICE_MARKER`] by a trusted author (so nobody else's
/// quote can suppress it).
#[must_use]
pub fn notice_posted(comments: &[Value], policy: &TrustPolicy) -> bool {
    comments.iter().any(|c| {
        policy.trusts_json(c)
            && c.get("body")
                .and_then(Value::as_str)
                .is_some_and(|b| super::records::anchored(b, NOTICE_MARKER))
    })
}

/// The one-line explanatory comment for a hold.
#[must_use]
pub fn notice_body(gate: &Gate) -> String {
    format!(
        "{NOTICE_MARKER} **Not promoted automatically (#10827)**: {}. It stays at \
         `loom:curated`. A trusted author or the operator must adopt it: apply `loom:issue` by \
         hand, or re-file it. See `.loom/docs/comment-trust.md`.",
        gate.reason()
    )
}

/// Whether the explanatory comment should be posted now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// A hold with no trusted notice yet: post [`notice_body`] once.
    Needed,
    /// A hold whose notice is already on the issue.
    Posted,
    /// A hold whose comments could not be read: do not post (no spam).
    Unknown,
    /// Not a hold.
    None,
}

impl Notice {
    /// `needed` / `posted` / `unknown` / `none`.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Needed => "needed",
            Self::Posted => "posted",
            Self::Unknown => "unknown",
            Self::None => "none",
        }
    }
}

/// The notice state for `gate`, given the issue's comment listing (`None`
/// when it could not be read).
#[must_use]
pub fn notice(gate: &Gate, comments: Option<&[Value]>, policy: &TrustPolicy) -> Notice {
    match (gate, comments) {
        (Gate::Hold(_), None) => Notice::Unknown,
        (Gate::Hold(_), Some(c)) if notice_posted(c, policy) => Notice::Posted,
        (Gate::Hold(_), Some(_)) => Notice::Needed,
        _ => Notice::None,
    }
}

/// One evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The answer.
    pub gate: Gate,
    /// Whether to post the hold notice.
    pub notice: Notice,
}

/// Resolve every input for issue `number` of `repo` (`owner/name`, or
/// `{owner}/{repo}` for the checkout's) and decide. Reads the REST issue
/// object (the one shape whose `user.login` names an App as `x[bot]`) and,
/// only for a hold, the REST comment listing.
#[must_use]
pub fn evaluate(root: &Path, repo: &str, number: u64) -> Outcome {
    let n = number.to_string();
    let issue = super::records::fetch_issue_object(repo, &n, root, false);
    let policy = TrustPolicy::for_root(root);
    let effective = crate::config_resolver::resolve_effective_config(root);
    // A malformed store location is "expected but unreadable" (fail closed).
    let expected = !matches!(
        crate::fleet_store::resolve_location(&effective, &|k| std::env::var(k).ok()),
        Ok(None)
    );
    let gate = decide(issue.as_ref(), &policy, expected);
    let comments = matches!(gate, Gate::Hold(_))
        .then(|| super::records::fetch_comment_listing(repo, &n, root, false))
        .flatten()
        .as_deref()
        .and_then(super::parse_listing);
    Outcome {
        notice: notice(&gate, comments.as_deref(), &policy),
        gate,
    }
}

#[cfg(test)]
mod tests;
