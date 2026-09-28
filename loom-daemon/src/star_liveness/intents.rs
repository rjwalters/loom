//! loom-ui star intents (#9244 "Applying stars from loom-ui").
//!
//! loom-ui has no GitHub write access, so a star made there is recorded as an
//! intent, and the fleet backend returns pending intents in the `/ingest` ack
//! as `operator_priority_intents`. The exporter parses them
//! ([`parse_ack_intents`]) and pushes them onto the process-global
//! [`IntentQueue`]; the liveness pass drains the queue and, per intent:
//!
//! 1. **Validates** ([`validate`]): only `label == "loom:operator-priority"`,
//!    only `action` `star` / `unstar`, only a repo this host's workspace
//!    registry manages, and a non-empty `requested_by`. Anything else is
//!    dropped and logged. A compromised dashboard can at most toggle this one
//!    label on repos this host manages.
//! 2. **Applies** ([`apply`]) idempotently: add or remove the label if needed,
//!    then post one audit comment carrying
//!    `<!-- loom:operator-priority-intent=<id> action=<a> requested_at=<ts> -->`
//!    unless a comment with that intent id already exists. The backend keeps
//!    returning an intent until it sees the label change, and several hosts
//!    may manage the repo, so every step must be safe to repeat.
//! 3. **Records starred-at**: `requested_at` becomes the issue's starred-at
//!    on this host at once ([`IntentStarredAt`]), and on every other host
//!    through the marker in the timeline ([`starred_at_from_timeline`]).
//!
//! The wire shape is what loom-ui shipped: `{id, repo, number, action,
//! label, requested_at, requested_by}` (the actor field is `requested_by`).
//! A backend that sends no field at all is an older backend; `[]` is a
//! supporting backend with nothing to do. Both are no-ops.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;

use super::forge::StarForge;
use crate::types::DroppedStarIntent;
use crate::work_finder::operator_priority::StarredAtSource;
use crate::work_finder::OPERATOR_PRIORITY_LABEL;

/// Most intents the queue holds; older ones are dropped first. The backend
/// resends a pending intent on every ack, so a dropped one comes back.
pub const MAX_QUEUED: usize = 500;

/// The audit-comment marker prefix.
pub const INTENT_MARKER_PREFIX: &str = "<!-- loom:operator-priority-intent=";

/// One intent as loom-ui ships it. Every field defaults so a partial object
/// still parses and is then dropped by [`validate`] with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StarIntent {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub number: u32,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub requested_at: Option<String>,
    #[serde(default)]
    pub requested_by: Option<String>,
}

/// Parse the ack's `operator_priority_intents` field, taken as a raw JSON
/// value so its shape can never fail the ack (and with it the #4830 host-id
/// check). `None` when the field is absent or `null` (an older backend) or
/// not an array at all (logged). Entries that are not objects of the right
/// shape are skipped one by one, never fatal to the rest.
#[must_use]
pub fn parse_ack_intents(raw: Option<&serde_json::Value>) -> Option<Vec<StarIntent>> {
    let raw = raw.filter(|v| !v.is_null())?;
    let Some(arr) = raw.as_array() else {
        log::warn!("star_liveness: ignoring a non-array operator_priority_intents in the ack");
        return None;
    };
    Some(
        arr.iter()
            .filter_map(|v| {
                if !v.is_object() {
                    // serde would read `[]` as an all-default struct.
                    log::warn!("star_liveness: dropping a non-object loom-ui intent");
                    return None;
                }
                serde_json::from_value::<StarIntent>(v.clone())
                    .ok()
                    .or_else(|| {
                        log::warn!("star_liveness: dropping a malformed loom-ui intent");
                        None
                    })
            })
            .collect(),
    )
}

/// The exporter → liveness-pass side channel: a bounded FIFO of intents,
/// deduplicated by id while queued.
#[derive(Debug, Default)]
pub struct IntentQueue {
    inner: Mutex<VecDeque<StarIntent>>,
}

impl IntentQueue {
    /// Queue `intents`, skipping ids already queued.
    pub fn push_all(&self, intents: Vec<StarIntent>) {
        let mut q = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for intent in intents {
            if q.iter().any(|i| i.id == intent.id) {
                continue;
            }
            if q.len() >= MAX_QUEUED {
                q.pop_front();
            }
            q.push_back(intent);
        }
    }

    /// Take everything queued.
    pub fn drain(&self) -> Vec<StarIntent> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// How many intents are queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static GLOBAL_QUEUE: OnceLock<Arc<IntentQueue>> = OnceLock::new();

/// The process-wide queue, created on first use. Only the native HTTPS
/// exporter arm calls this: only that protocol's ack can carry intents
/// (mirroring `HostIdStatus`), and every HTTPS sink shares the one queue.
pub fn register_global_queue() -> Arc<IntentQueue> {
    GLOBAL_QUEUE
        .get_or_init(|| Arc::new(IntentQueue::default()))
        .clone()
}

/// The registered queue, or `None` when no HTTPS exporter started.
#[must_use]
pub fn global_queue() -> Option<Arc<IntentQueue>> {
    GLOBAL_QUEUE.get().cloned()
}

/// Star or unstar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Star,
    Unstar,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Star => "star",
            Self::Unstar => "unstar",
        }
    }
}

/// An intent that passed validation, bound to its workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidIntent {
    pub id: String,
    /// The slug as this host knows it.
    pub repo: String,
    pub root: PathBuf,
    pub number: u32,
    pub action: Action,
    /// RFC 3339, normalized to UTC seconds; `None` when absent or invalid.
    pub requested_at: Option<String>,
    /// Sanitized for display.
    pub requested_by: String,
}

/// An id is safe inside an HTML comment and a marker: `[A-Za-z0-9._:-]`,
/// 1–128 chars, and never `--`.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.contains("--")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// `requested_by` as shown in the audit comment: printable ASCII from a
/// small set (no markdown, no backticks), at most 64 chars. `@` is kept so an
/// email address reads as one; the comment renders the actor inside a code
/// span, where `@name` does not ping anyone. `None` when nothing is left.
fn sanitize_actor(raw: Option<&str>) -> Option<String> {
    let s: String = raw?
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '+' | '@'))
        .take(64)
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn normalize_ts(raw: Option<&str>) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(raw?.trim())
        .ok()
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
}

/// Validate one intent against `managed` (lower-cased slug → workspace root).
///
/// # Errors
/// The drop record, with its reason, for an intent that must not be applied.
pub fn validate(
    intent: &StarIntent,
    managed: &HashMap<String, (String, PathBuf)>,
) -> Result<ValidIntent, DroppedStarIntent> {
    let drop = |reason: &str| DroppedStarIntent {
        id: intent.id.chars().take(128).collect(),
        repo: intent.repo.chars().take(200).collect(),
        number: intent.number,
        reason: reason.to_string(),
    };
    if !safe_id(&intent.id) || intent.number == 0 || intent.repo.trim().is_empty() {
        return Err(drop("malformed"));
    }
    if intent.label != OPERATOR_PRIORITY_LABEL {
        return Err(drop("wrong-label"));
    }
    let action = match intent.action.as_str() {
        "star" => Action::Star,
        "unstar" => Action::Unstar,
        _ => return Err(drop("bad-action")),
    };
    let Some(requested_by) = sanitize_actor(intent.requested_by.as_deref()) else {
        return Err(drop("missing-requested-by"));
    };
    let Some((repo, root)) = managed.get(&intent.repo.trim().to_ascii_lowercase()) else {
        return Err(drop("unmanaged-repo"));
    };
    Ok(ValidIntent {
        id: intent.id.clone(),
        repo: repo.clone(),
        root: root.clone(),
        number: intent.number,
        action,
        requested_at: normalize_ts(intent.requested_at.as_deref()),
        requested_by,
    })
}

/// The marker for `intent`.
#[must_use]
pub fn marker(intent: &ValidIntent) -> String {
    let at = intent
        .requested_at
        .as_deref()
        .map(|t| format!(" requested_at={t}"))
        .unwrap_or_default();
    format!("{INTENT_MARKER_PREFIX}{} action={}{at} -->", intent.id, intent.action.as_str())
}

/// The audit comment for `intent`.
#[must_use]
pub fn audit_comment(intent: &ValidIntent) -> String {
    let verb = match intent.action {
        Action::Star => "⭐ Starred for operator priority",
        Action::Unstar => "Unstarred (operator priority removed)",
    };
    format!("{}\n{verb} by `{}` via loom-ui.", marker(intent), intent.requested_by)
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    pub label_changed: bool,
    pub commented: bool,
}

/// Apply one validated intent through `forge`. Idempotent: the label change
/// is skipped when the label is already in the wanted state, and the comment
/// when a comment with this intent id exists.
///
/// # Errors
/// A forge read or write failed; the caller keeps the intent for a retry.
pub fn apply(forge: &mut dyn StarForge, intent: &ValidIntent) -> anyhow::Result<Applied> {
    let mut applied = Applied::default();
    let Some(issue) = forge.issue(intent.number)? else {
        anyhow::bail!("{}#{} does not exist", intent.repo, intent.number);
    };
    let has = issue.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL);
    match intent.action {
        Action::Star if !has => {
            forge.add_label(intent.number, OPERATOR_PRIORITY_LABEL)?;
            applied.label_changed = true;
        }
        Action::Unstar if has => {
            forge.remove_label(intent.number, OPERATOR_PRIORITY_LABEL)?;
            applied.label_changed = true;
        }
        _ => {}
    }
    let id_marker = format!("{INTENT_MARKER_PREFIX}{} ", intent.id);
    let comments = forge.comments(intent.number)?;
    let me = forge.self_login();
    let posted = comments
        .iter()
        .any(|c| c.body.contains(&id_marker) && super::trust::trusted(c, me.as_deref()));
    if !posted {
        forge.post_comment(intent.number, &audit_comment(intent))?;
        applied.commented = true;
    }
    record_starred_at(intent);
    Ok(applied)
}

// ---------------------------------------------------------------------------
// Starred-at: the intent's `requested_at` is authoritative.
// ---------------------------------------------------------------------------

fn overrides() -> &'static Mutex<HashMap<(String, u32), String>> {
    static MAP: OnceLock<Mutex<HashMap<(String, u32), String>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_starred_at(intent: &ValidIntent) {
    let key = (intent.root.display().to_string(), intent.number);
    let mut map = overrides()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match (intent.action, &intent.requested_at) {
        (Action::Star, Some(at)) => {
            map.insert(key, at.clone());
        }
        _ => {
            map.remove(&key);
        }
    }
}

/// The recorded `requested_at` for `number` in the workspace at `root`.
#[must_use]
pub fn recorded_starred_at(root: &Path, number: u32) -> Option<String> {
    overrides()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(root.display().to_string(), number))
        .cloned()
}

/// A [`StarredAtSource`] that answers from an intent this host applied and
/// falls through to `inner` (the timeline) otherwise. This is the seam slice
/// A left for C.
pub struct IntentStarredAt<'a> {
    pub root: Option<&'a Path>,
    pub inner: &'a mut dyn StarredAtSource,
}

impl StarredAtSource for IntentStarredAt<'_> {
    fn starred_at(&mut self, issue: u32) -> anyhow::Result<Option<String>> {
        if let Some(at) = self.root.and_then(|r| recorded_starred_at(r, issue)) {
            return Ok(Some(at));
        }
        self.inner.starred_at(issue)
    }
}

/// The starred-at from a timeline read that yields one line per relevant
/// event: `L <created_at>` for a `labeled` event and
/// `C <created_at> <requested_at> <author_association> <login>` for a
/// star-intent audit comment. An intent comment counts only from a trusted
/// author ([`super::trust`]): anyone can comment on a public repo, and a
/// forged marker with an old `requested_at` would jump the starred order.
///
/// The latest intent comment posted at or after the latest `labeled` event
/// decides (that labeling came from the intent, and the intent's
/// `requested_at` is when the operator asked). A later direct labeling on
/// GitHub wins over an older intent. With no intent, the latest labeling.
#[must_use]
pub fn starred_at_from_timeline(stdout: &str) -> Option<String> {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s.trim_matches('"')).ok();
    let mut labeled = None;
    let mut intents = Vec::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.trim().trim_matches('"').split_whitespace().collect();
        match fields.as_slice() {
            ["L", at, ..] => {
                if let Some(t) = parse(at) {
                    labeled = labeled.max(Some(t));
                }
            }
            ["C", at, req, assoc, login, ..] => {
                let believed = super::trust::trusted_author(Some(login), Some(assoc), None);
                if let (true, Some(t), Some(r)) = (believed, parse(at), parse(req)) {
                    intents.push((t, r));
                }
            }
            // A bare timestamp is a labeled event (the pre-#9244-C output
            // shape, which slice A's fakes still emit).
            [bare] if parse(bare).is_some() => {
                labeled = labeled.max(parse(bare));
            }
            _ => {}
        }
    }
    let fmt = |t: chrono::DateTime<chrono::FixedOffset>| {
        t.with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    let intent = intents
        .into_iter()
        .filter(|(posted, _)| labeled.is_none_or(|l| *posted >= l))
        .max_by_key(|(posted, _)| *posted)
        .map(|(_, req)| req);
    intent.or(labeled).map(fmt)
}

/// Bounded memory of intent ids this process has applied, so a resent
/// intent costs no forge call.
#[derive(Debug, Default)]
pub struct AppliedIds {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl AppliedIds {
    const CAP: usize = 2000;

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    pub fn insert(&mut self, id: &str) {
        if self.set.insert(id.to_string()) {
            self.order.push_back(id.to_string());
            if self.order.len() > Self::CAP {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }
}
