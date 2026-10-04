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
//!
//! # Fleet-comms notices (#9321)
//!
//! A forge comment is a notification nobody reads in time (#9268: four agents
//! commented for seven hours and no human saw it). So every comment this
//! module actually posts also yields one [`Notice`], which the liveness task
//! publishes on the `operator_priority.escalation` event-bus topic for the
//! Safehouse sink to relay into the team's Matrix room.
//!
//! **The notice is produced by the same call that posts the comment, and only
//! when it posts.** That is deliberate and structural, not a convention: the
//! dedupe above is what makes the comment once-per-cause fleet-wide, so tying
//! the notice to [`Outcome::Posted`] makes the Matrix post once-per-cause too
//! — across ticks (the ledger short-circuits), across hosts (a peer sees
//! `FoundOnForge` and stays quiet) and across restarts (the forge marker is
//! re-read). There is no second dedupe to keep in sync.
//!
//! [`Ledger::resolve_cleared`] adds the recovery half: one `resolved` notice
//! when a key this process announced stops being asked. Only the announcing
//! host narrates the recovery, and only for repos whose pass succeeded — an
//! unreadable repo must not read as "everything resolved".

use std::collections::HashSet;

use super::forge::{ForgeComment, StarForge};
use crate::types::{AskKind, Event, LandingStage, OperatorAsk};

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
    comment_body_for(ask, host, inherited_from, None)
}

/// [`comment_body`] for a row that may inherit an operator priority level
/// (#10307): `level_from` names the source (`owner/repo#N`).
#[must_use]
pub fn comment_body_for(
    ask: &OperatorAsk,
    host: &str,
    inherited_from: Option<u32>,
    level_from: Option<&str>,
) -> String {
    let why = if let Some(src) = level_from {
        format!(
            "This issue blocks {src} and inherits its operator priority level \
             (`{}`)",
            crate::operator_levels::HIGH_PRIORITY_INHERITED_LABEL
        )
    } else {
        inherited_from.map_or_else(
            || "This issue is starred (`loom:operator-priority`)".to_string(),
            |n| {
                format!(
                "This issue blocks starred #{n} and inherits its star (`loom:operator-priority`)"
            )
            },
        )
    };
    format!(
        "{}\n**Operator needed** — {why}, and no agent can move it further.\n\n{}\n\n\
         <sub>Posted once per cause by the loom-daemon liveness check on host `{host}` \
         (#9244). Resolving the cause clears it; nothing to acknowledge here.</sub>",
        marker(&ask.key),
        ask.text
    )
}

/// The row an escalation is about, beyond the ask itself: everything the
/// fleet-comms [`Notice`] needs that the forge comment does not (#9321).
#[derive(Debug, Clone)]
pub struct Target<'a> {
    /// Forge `owner/repo`.
    pub repo: &'a str,
    pub issue: u32,
    /// The landing stage at escalation time.
    pub stage: LandingStage,
    /// The issue's canonical web URL.
    pub url: String,
    /// The starred issue this row inherited its star from, when it is an
    /// inheriting blocker.
    pub inherited_from: Option<u32>,
    /// The level >= 2 source this row inherits a level from (#10307), as
    /// `owner/repo#N`.
    pub level_from: Option<&'a str>,
}

/// One fleet-comms escalation notice (#9321): the event-bus/Matrix half of an
/// escalation the forge comment already carries.
///
/// Produced only by the [`Ledger::escalate`] call that actually posts
/// ([`Outcome::Posted`]) and by [`Ledger::resolve_cleared`], so it inherits the
/// comment marker's fleet-wide once-per-cause dedupe rather than adding a
/// second one. Converted to the bus event by [`Notice::to_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Forge `owner/repo`.
    pub repo: String,
    pub issue: u32,
    /// The dedupe key, `<kind>:<specifics>`.
    pub key: String,
    pub kind: AskKind,
    pub stage: LandingStage,
    /// The ask itself. Empty on a `resolved` notice.
    pub text: String,
    /// The issue's canonical web URL.
    pub url: String,
    pub inherited_from: Option<u32>,
    /// `false` for the escalation, `true` for the recovery notice.
    pub resolved: bool,
}

impl Notice {
    /// The `operator_priority.escalation` bus event for this notice. `host` is
    /// the observing host's id (the same one the forge comment names).
    #[must_use]
    pub fn to_event(&self, host: &str) -> Event {
        Event::OperatorPriorityEscalation {
            slug: self.repo.clone(),
            issue: self.issue,
            key: self.key.clone(),
            kind: self.kind.as_str().to_string(),
            stage: self.stage.as_str().to_string(),
            text: self.text.clone(),
            url: self.url.clone(),
            host: host.to_string(),
            inherited_from: self.inherited_from,
            resolved: self.resolved,
        }
    }
}

/// The issue's canonical web URL. `web_base` is the forge's web origin
/// (`https://github.com` unless the repo's `origin` remote names another
/// host), so a Gitea fleet's escalation links resolve too.
#[must_use]
pub fn issue_url(web_base: &str, repo: &str, issue: u32) -> String {
    format!("{}/{repo}/issues/{issue}", web_base.trim_end_matches('/'))
}

/// Keys this process knows are on the forge.
#[derive(Debug, Default)]
pub struct Ledger {
    known: HashSet<(String, u32, String)>,
    /// Keys this process posted **and** announced on the bus (#9321), with the
    /// facts the recovery notice needs. A key another host posted is in
    /// [`Self::known`] but not here, so only the announcing host narrates the
    /// recovery.
    announced: Vec<Notice>,
    /// Notices this pass produced, drained by the caller
    /// ([`Self::take_notices`]) and published on the bus.
    pending: Vec<Notice>,
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

    /// Post `ask` on `target`'s issue unless it is already there.
    ///
    /// On [`Outcome::Posted`] — and only then — one [`Notice`] is queued for
    /// [`Self::take_notices`], so the fleet-comms post shares this method's
    /// dedupe instead of owning a second one (#9321).
    ///
    /// # Errors
    /// A forge read or write failed; nothing is recorded, so the next pass
    /// retries.
    pub fn escalate(
        &mut self,
        forge: &mut dyn StarForge,
        target: &Target<'_>,
        ask: &OperatorAsk,
    ) -> anyhow::Result<Outcome> {
        let (repo, issue) = (target.repo, target.issue);
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
            forge.post_comment(
                issue,
                &comment_body_for(ask, &self.host, target.inherited_from, target.level_from),
            )?;
            let notice = Notice {
                repo: repo.to_string(),
                issue,
                key: ask.key.clone(),
                kind: ask.kind,
                stage: target.stage,
                text: ask.text.clone(),
                url: target.url.clone(),
                inherited_from: target.inherited_from,
                resolved: false,
            };
            self.announced.push(notice.clone());
            self.pending.push(notice);
            Outcome::Posted
        };
        self.known
            .insert((repo.to_string(), issue, ask.key.clone()));
        Ok(outcome)
    }

    /// Queue a `resolved` notice for every key this process announced whose
    /// ask is no longer being made (#9321).
    ///
    /// `live` is the set of `(repo, issue, key)` this pass still asked for.
    /// `readable` is the set of forge slugs whose evaluation **succeeded** this
    /// pass — a repo the pass could not read has no rows at all, and must not
    /// be mistaken for "every ask on it resolved".
    pub fn resolve_cleared(
        &mut self,
        live: &HashSet<(String, u32, String)>,
        readable: &HashSet<String>,
    ) {
        let (cleared, still): (Vec<Notice>, Vec<Notice>) = std::mem::take(&mut self.announced)
            .into_iter()
            .partition(|n| {
                readable.contains(&n.repo)
                    && !live.contains(&(n.repo.clone(), n.issue, n.key.clone()))
            });
        self.announced = still;
        self.pending.extend(cleared.into_iter().map(|n| Notice {
            text: String::new(),
            resolved: true,
            ..n
        }));
    }

    /// Take the notices produced since the last call (#9321). The caller
    /// publishes them; a caller with no bus simply drops them, which is the
    /// pre-#9321 behavior.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.pending)
    }
}
