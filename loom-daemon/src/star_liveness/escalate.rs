//! Post each operator ask once (#9244 liveness item 2).
//!
//! # Dedupe
//!
//! The unit is (repo, issue, ask key). The key is stable for a cause (see
//! [`crate::types::OperatorAsk::key`]), so the same stall is reported once
//! and a new cause on the same issue is reported again.
//!
//! - **Across ticks:** [`Ledger`] remembers every key this process has posted
//!   or found on the forge, so a repeat needs no forge read at all.
//! - **Across hosts:** before posting, the issue's comments are read and a
//!   trusted comment already carrying the key's [`marker`] counts as posted
//!   (a marker forged by an outside commenter does not). Two
//!   hosts can still race inside the read-then-post window of one pass; that
//!   costs one duplicate comment at worst, never a missed ask.
//! - **Across restarts:** the forge marker again; the ledger is rebuilt from
//!   it on first sight.

use std::collections::HashSet;

use super::forge::{ForgeComment, StarForge};
use crate::types::OperatorAsk;

/// The marker prefix every escalation comment carries.
pub const MARKER_PREFIX: &str = "<!-- loom:operator-priority-escalation key=";

/// The marker for `key`.
#[must_use]
pub fn marker(key: &str) -> String {
    format!("{MARKER_PREFIX}{key} -->")
}

/// Whether any **trusted** comment ([`super::trust`]) carries the marker
/// for `key`. A marker an outside commenter pre-posted suppresses nothing.
#[must_use]
pub fn already_posted(comments: &[ForgeComment], key: &str, self_login: Option<&str>) -> bool {
    let m = marker(key);
    comments
        .iter()
        .any(|c| c.body.contains(&m) && super::trust::trusted(c, self_login))
}

/// The comment body for `ask` on an issue. `inherited_from` names the
/// starred issue when this is an inheriting blocker.
#[must_use]
pub fn comment_body(ask: &OperatorAsk, host: &str, inherited_from: Option<u32>) -> String {
    let why = inherited_from.map_or_else(
        || "This issue is starred (`loom:operator-priority`)".to_string(),
        |n| {
            format!(
                "This issue blocks starred #{n} and inherits its star (`loom:operator-priority`)"
            )
        },
    );
    format!(
        "{}\n**Operator needed** — {why}, and no agent can move it further.\n\n{}\n\n\
         <sub>Posted once per cause by the loom-daemon liveness check on host `{host}` \
         (#9244). Resolving the cause clears it; nothing to acknowledge here.</sub>",
        marker(&ask.key),
        ask.text
    )
}

/// Keys this process knows are on the forge.
#[derive(Debug, Default)]
pub struct Ledger {
    known: HashSet<(String, u32, String)>,
    /// This host's id, named in the comment.
    pub host: String,
    /// Whether forge writes are on (`escalate`).
    pub write: bool,
}

/// What [`Ledger::escalate`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Known from an earlier pass; no forge call.
    Known,
    /// Found on the forge (another host, or before a restart); not posted.
    FoundOnForge,
    /// Posted by this call.
    Posted,
    /// Writes are off (`escalate: false`); recorded nowhere.
    Disabled,
}

impl Ledger {
    /// Whether (`repo`, `issue`, `key`) is known posted.
    #[must_use]
    pub fn knows(&self, repo: &str, issue: u32, key: &str) -> bool {
        self.known
            .contains(&(repo.to_string(), issue, key.to_string()))
    }

    /// Post `ask` on `issue` unless it is already there.
    ///
    /// # Errors
    /// A forge read or write failed; nothing is recorded, so the next pass
    /// retries.
    pub fn escalate(
        &mut self,
        forge: &mut dyn StarForge,
        repo: &str,
        issue: u32,
        ask: &OperatorAsk,
        inherited_from: Option<u32>,
    ) -> anyhow::Result<Outcome> {
        if self.knows(repo, issue, &ask.key) {
            return Ok(Outcome::Known);
        }
        if !self.write {
            return Ok(Outcome::Disabled);
        }
        let comments = forge.comments(issue)?;
        let me = forge.self_login();
        let outcome = if already_posted(&comments, &ask.key, me.as_deref()) {
            Outcome::FoundOnForge
        } else {
            forge.post_comment(issue, &comment_body(ask, &self.host, inherited_from))?;
            Outcome::Posted
        };
        self.known
            .insert((repo.to_string(), issue, ask.key.clone()));
        Ok(outcome)
    }
}
