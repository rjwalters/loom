//! Who owns a star, and what propagation may do about it (#10012 §2–§3).
//!
//! Pure decision logic for the star-propagation pass. The pass that writes
//! the labels is [`super::materialize`]; this module fixes the rules it
//! follows.
//!
//! When propagation takes a star back it first posts the same marker with
//! `action=unstar` ([`unstar_marker`]). That comment is not a star event; it
//! only tells a later pass that the daemon, not a person, removed the star,
//! so a child the operator unstarred by hand is never starred again
//! ([`operator_removed`]).
//!
//! # Provenance
//!
//! A star the daemon materializes on a child carries an audit comment in the
//! loom-ui intent marker shape plus `inherited_from=#P`:
//!
//! ```text
//! <!-- loom:operator-priority-intent=inherit-<P>-<C> action=star requested_at=<P's starred-at> inherited_from=#P -->
//! ```
//!
//! `requested_at` is **P's** starred-at, so `STARRED_AT_JQ` already orders the
//! child at the parent's star time. `inherited_from` names the starred root
//! whose star the child carries ([`super::edges::Inheritance::root`]).
//!
//! # Removal
//!
//! - The **latest** star event decides ownership ([`owner`]): a daemon
//!   inherited marker makes the star inherited; a human `labeled` event, a
//!   loom-ui intent or a `forge star --direction` star makes it the
//!   operator's own. Propagation never removes an operator's star.
//! - An inherited star is removed only when the root it names has **lost its
//!   star**. A root that closed while starred keeps its label (labels are
//!   never cleaned on close), so its children keep theirs (AC 5).
//! - A child still reached from any starred ancestor keeps (or gets) the
//!   star, whatever its marker names (AC 6).
//! - When in doubt (unreadable root, no star events), keep.

use std::sync::OnceLock;

use regex::Regex;

use super::edges::Inheritance;
use super::intents::INTENT_MARKER_PREFIX;

/// The field naming the root an inherited star came from.
pub const INHERITED_FROM_FIELD: &str = "inherited_from=#";

/// The intent id of the star `root` passes to `child`: safe inside a marker
/// (`[A-Za-z0-9._:-]`, no `--`), and the same on every host so the audit
/// comment dedupes across the fleet.
#[must_use]
pub fn intent_id(root: u32, child: u32) -> String {
    format!("inherit-{root}-{child}")
}

/// The inherited-star marker for `child`, carrying `root`'s starred-at.
#[must_use]
pub fn marker(root: u32, child: u32, starred_at: Option<&str>) -> String {
    let at = starred_at
        .map(|t| format!(" requested_at={t}"))
        .unwrap_or_default();
    format!(
        "{INTENT_MARKER_PREFIX}{} action=star{at} {INHERITED_FROM_FIELD}{root} -->",
        intent_id(root, child)
    )
}

fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"<!--\s*loom:operator-priority-intent=[A-Za-z0-9._:-]+\s+action=star(?:\s+requested_at=(\S+?))?\s+inherited_from=#([0-9]+)\s*-->",
        )
        .expect("static inherited-marker pattern")
    })
}

/// A parsed inherited-star marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedMarker {
    pub root: u32,
    pub requested_at: Option<String>,
}

/// The inherited-star marker in `body`, if any. Only the caller can decide
/// whether the comment's author is trusted ([`super::trust`]); an untrusted
/// marker must not be passed on as a [`StarEvent::Inherited`].
#[must_use]
pub fn parse_marker(body: &str) -> Option<InheritedMarker> {
    let c = marker_re().captures(body)?;
    Some(InheritedMarker {
        root: c.get(2)?.as_str().parse().ok()?,
        requested_at: c.get(1).map(|m| m.as_str().to_string()),
    })
}

/// The marker propagation posts just before it removes an inherited star.
#[must_use]
pub fn unstar_marker(root: u32, child: u32) -> String {
    format!(
        "{INTENT_MARKER_PREFIX}{} action=unstar {INHERITED_FROM_FIELD}{root} -->",
        intent_id(root, child)
    )
}

fn any_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"<!--\s*loom:operator-priority-intent=[A-Za-z0-9._:-]+\s+action=(star|unstar)\b[^>]*?\sinherited_from=#[0-9]+\s*-->",
        )
        .expect("static inherited-action pattern")
    })
}

/// Whether the latest **trusted** inherited marker in `comments` (oldest
/// first) is a star: propagation starred the item, and it carries no star
/// now, so someone took it off by hand. Propagation leaves it unstarred.
/// A latest `action=unstar` (propagation removed it) or no marker at all
/// means the star may be (re)applied.
#[must_use]
pub fn operator_removed(comments: &[super::forge::ForgeComment], me: Option<&str>) -> bool {
    super::trust::only_trusted(comments, me)
        .iter()
        .rev()
        .find_map(|c| any_marker_re().captures(&c.body).map(|m| m[1].to_string()))
        .is_some_and(|action| action == "star")
}

/// The star events in one page of a REST timeline (`GET
/// /repos/{o}/{r}/issues/{n}/timeline`): every `labeled` event for the star,
/// and every **trusted** star comment, an inherited marker
/// ([`StarKind::Inherited`]) or an operator intent ([`StarKind::Intent`]:
/// loom-ui or `forge star --direction`). An untrusted marker is ignored,
/// so an outsider cannot make an operator's star look inherited.
#[must_use]
pub fn star_events_from_timeline(page: &serde_json::Value, me: Option<&str>) -> Vec<StarEvent> {
    fn str_at<'v>(v: &'v serde_json::Value, k: &str) -> Option<&'v str> {
        v.get(k).and_then(serde_json::Value::as_str)
    }
    let mut out = Vec::new();
    for ev in page.as_array().into_iter().flatten() {
        let Some(at) = str_at(ev, "created_at") else {
            continue;
        };
        match str_at(ev, "event") {
            Some("labeled")
                if ev.pointer("/label/name").and_then(|x| x.as_str())
                    == Some(crate::work_finder::OPERATOR_PRIORITY_LABEL) =>
            {
                out.push(StarEvent {
                    at: at.to_string(),
                    kind: StarKind::Labeled,
                });
            }
            Some("commented") => {
                let body = str_at(ev, "body").unwrap_or_default();
                if !body.contains(INTENT_MARKER_PREFIX) {
                    continue;
                }
                let login = ev
                    .pointer("/user/login")
                    .or_else(|| ev.pointer("/actor/login"))
                    .and_then(|x| x.as_str());
                if !super::trust::trusted_author(login, str_at(ev, "author_association"), me) {
                    continue;
                }
                let kind = if let Some(m) = parse_marker(body) {
                    StarKind::Inherited { root: m.root }
                } else if body.contains(" action=star") && !body.contains(INHERITED_FROM_FIELD) {
                    StarKind::Intent
                } else {
                    continue;
                };
                out.push(StarEvent {
                    at: at.to_string(),
                    kind,
                });
            }
            _ => {}
        }
    }
    out
}

/// One event that put the star on an item, as read from its timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StarKind {
    /// A `labeled` event (by anyone, including the daemon's own write).
    Labeled,
    /// A trusted operator-intent comment: loom-ui or `forge star --direction`.
    Intent,
    /// A trusted daemon inherited marker naming the root.
    Inherited { root: u32 },
}

/// A star event and when it happened (RFC 3339).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarEvent {
    pub at: String,
    pub kind: StarKind,
}

/// Who owns the star currently on an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The operator (or nothing proves otherwise).
    Operator,
    /// Propagation, from the named root.
    Inherited { root: u32 },
}

fn ts(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s).ok()
}

/// The owner by the **latest** star event. The audit comment follows the
/// label write it documents, so a marker posted at or after the latest
/// `labeled` event explains that labeling; a `labeled` event after every
/// marker is a fresh human star. No events (or none parseable): the operator.
#[must_use]
pub fn owner(events: &[StarEvent]) -> Owner {
    let labeled = events
        .iter()
        .filter(|e| e.kind == StarKind::Labeled)
        .filter_map(|e| ts(&e.at))
        .max();
    let comment = events
        .iter()
        .filter(|e| e.kind != StarKind::Labeled)
        .filter_map(|e| ts(&e.at).map(|t| (t, &e.kind)))
        .filter(|(t, _)| labeled.is_none_or(|l| *t >= l))
        .max_by_key(|(t, _)| *t);
    match comment {
        Some((_, StarKind::Inherited { root })) => Owner::Inherited { root: *root },
        _ => Owner::Operator,
    }
}

/// What the pass knows about the root an inherited star names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    /// It carries the star, open **or closed**.
    Starred,
    /// It was read and carries no star.
    Unstarred,
    /// It could not be read.
    Unknown,
}

/// What propagation does to one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Apply the star with an inherited marker naming `root`.
    Add {
        root: u32,
        starred_at: Option<String>,
    },
    /// Leave the item as it is.
    Keep,
    /// Remove the inherited star: `root` lost its star.
    Remove { root: u32 },
}

/// The decision for one open item.
///
/// - `starred`: it carries the star now.
/// - `events`: its star events (only read when it is starred and not reached).
/// - `reached`: its inheritance from a currently-starred open ancestor, if
///   [`super::edges::descendants`] reached it this pass.
/// - `root_state`: the state of the root an inherited marker names.
pub fn decide(
    starred: bool,
    events: &[StarEvent],
    reached: Option<&Inheritance>,
    root_state: impl FnOnce(u32) -> RootState,
) -> Decision {
    match (starred, reached) {
        (false, Some(inh)) => Decision::Add {
            root: inh.root,
            starred_at: inh.starred_at.clone(),
        },
        (false, None) | (true, Some(_)) => Decision::Keep,
        (true, None) => match owner(events) {
            Owner::Operator => Decision::Keep,
            Owner::Inherited { root } => match root_state(root) {
                RootState::Unstarred => Decision::Remove { root },
                RootState::Starred | RootState::Unknown => Decision::Keep,
            },
        },
    }
}
