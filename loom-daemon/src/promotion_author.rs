//! The author gate on automatic promotion to `loom:issue` (#10827).
//!
//! The comment-trust rules (#9548) cover markers and control phrases, not who
//! wrote the issue being promoted. An issue authored by an untrusted identity
//! (an outside bot, a non-collaborator) may be curated, but an automated
//! Champion verdict alone must not approve it for building: a trusted
//! maintainer adopts it (applies `loom:issue` by hand) or re-files it. The
//! decision reuses [`TrustPolicy`], so there is no second trust table. An
//! adoption is narrower than comment trust: only a human maintainer's label
//! counts, never an App's or a bot's ([`adopted_by`]).
//!
//! The caller promotes only on [`Gate::Trusted`]; [`Gate::Untrusted`] and
//! [`Gate::Unknown`] both mean "do not promote", differing only in the
//! explanation. Evidence that cannot be read is never read as trust.

use serde_json::Value;

use crate::comment_trust::{Author, TrustPolicy};

/// Marker on the one explanatory comment, so retries do not repeat it. A
/// legacy notice (before #13142) is exactly this; a current one carries its
/// [`Stamp`] (`<!-- loom:promotion-author-gate rev=… trust=… -->`).
pub const NOTICE_MARKER: &str = "<!-- loom:promotion-author-gate -->";

const NOTICE_OPEN: &str = "<!-- loom:promotion-author-gate";

/// What a gate verdict was reached from (#13142): the issue's title and body
/// and the trust rules. The step tick holds a gated issue out of
/// `champion_promote` while both still match, so an unchanged re-draw (a full
/// Champion session that can only repeat the verdict) is not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// [`revision`] of the issue's title and body.
    pub rev: String,
    /// [`TrustPolicy::digest`] of the trust rules.
    pub trust: String,
}

impl Stamp {
    fn marker(&self) -> String {
        format!("{NOTICE_OPEN} rev={} trust={} -->", self.rev, self.trust)
    }

    /// The stamp in the first gate marker of `comment`; `None` for a legacy
    /// marker (no stamp) or a comment with no gate marker.
    #[must_use]
    pub fn parse(comment: &str) -> Option<Self> {
        let start = comment.find(NOTICE_OPEN)? + NOTICE_OPEN.len();
        let rest = &comment[start..];
        let inner = &rest[..rest.find("-->")?];
        let field = |key: &str| {
            inner
                .split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        Some(Self {
            rev: field("rev=")?,
            trust: field("trust=")?,
        })
    }
}

/// The digest of an issue's `title` and `body` (the revise marker's hash,
/// `champion-issue-promo.md`): it moves when the issue is edited.
#[must_use]
pub fn revision(title: &str, body: &str) -> String {
    let text = format!("{}\n{}", title.trim_end_matches('\n'), body.trim_end_matches('\n'));
    crate::short_hash::short_sha16(&text)
}

/// Whether an issue's author clears automatic promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The author is trusted: normal promotion criteria apply.
    Trusted,
    /// The author is not trusted (the login, if the forge named one).
    Untrusted(Option<String>),
    /// Trust could not be established (the reason): treated as not trusted.
    Unknown(String),
}

impl Gate {
    /// The stdout token a script matches on.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Self::Trusted => "TRUSTED".to_string(),
            Self::Untrusted(login) => {
                format!("UNTRUSTED {}", login.as_deref().unwrap_or("(unknown author)"))
            }
            Self::Unknown(why) => format!("UNKNOWN {why}"),
        }
    }
}

/// The Actions bot, the only login the tracker exception admits (#13378).
const CI_DAILY_AUTHOR: &str = "github-actions[bot]";
/// Title prefix of a `ci-daily.yml` tracker.
const CI_DAILY_TITLE_PREFIX: &str = "[ci-daily] ";
/// The sentence `ci-daily.yml` writes into every tracker body it files.
const CI_DAILY_BODY_MARKER: &str = "opened, updated and closed by `.github/workflows/ci-daily.yml`";

/// Whether `issue` is a `ci-daily.yml` tracker filed by the Actions bot.
///
/// This is an exception to the *promotion author gate only*: it is not part of
/// [`TrustPolicy`], so the bot's comments and markers stay untrusted everywhere
/// (verdicts, holds, `forge trusted-comments`). All three conditions must hold.
fn is_ci_daily_tracker(author: &Author, issue: &Value) -> bool {
    let field = |key: &str| issue.get(key).and_then(Value::as_str).unwrap_or_default();
    author.login.as_deref() == Some(CI_DAILY_AUTHOR)
        && field("title").starts_with(CI_DAILY_TITLE_PREFIX)
        && field("body").contains(CI_DAILY_BODY_MARKER)
}

/// Classify the REST issue object `issue` (`None`: it could not be read).
///
/// An issue object with no author (a deleted account) is untrusted, as for any
/// other trust check. A pull request is not an issue to promote and is refused
/// as unknown rather than judged.
#[must_use]
pub fn classify(policy: &TrustPolicy, issue: Option<&Value>) -> Gate {
    let Some(issue) = issue else {
        return Gate::Unknown("the issue could not be read from the forge".to_string());
    };
    if issue.get("pull_request").is_some() {
        return Gate::Unknown("the target is a pull request, not an issue".to_string());
    }
    let author = Author::from_json(issue);
    if policy.trusts(&author) || is_ci_daily_tracker(&author, issue) {
        Gate::Trusted
    } else {
        Gate::Untrusted(author.login)
    }
}

/// Whether the forge actor `actor` (a timeline `actor` or an edit `editor`
/// object) is trusted: the verdict is [`TrustPolicy::trusts`], with the
/// association `association_of(login)` supplies when the login alone is not
/// enough (`None`: unknown, which no rule trusts). A bot-typed actor is an App.
fn actor_trusted(
    policy: &TrustPolicy,
    actor: &Value,
    association_of: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let login = actor.get("login").and_then(Value::as_str)?;
    let mut author = Author::new(Some(login), None);
    let kind = actor.get("type").or_else(|| actor.get("__typename"));
    author.app = author.app || kind.and_then(Value::as_str) == Some("Bot");
    if !policy.trusts(&author) {
        author.association = association_of(login);
    }
    policy.trusts(&author).then(|| login.to_string())
}

fn instant(v: Option<&Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(v?.as_str()?)
        .ok()
        .map(|t| t.to_utc())
}

/// The login of a human forge account (`type` / `__typename` is exactly
/// `User`, and the login is not App-spelled). An actor with no readable type,
/// a `Bot`, an `Organization` or a `Mannequin` is not one (fail closed).
fn human_login(actor: &Value) -> Option<&str> {
    let login = actor.get("login").and_then(Value::as_str)?.trim();
    let kind = actor
        .get("type")
        .or_else(|| actor.get("__typename"))
        .and_then(Value::as_str)?;
    (kind == "User" && !login.is_empty() && !crate::comment_trust::is_app_login(login))
        .then_some(login)
}

/// Whether `actor` can adopt (operator ruling 20261116-r15 q48): a **human**
/// account that is allow-listed (`forge.trustedCommenters` or the fleet admin
/// roster) or for which `association_of(login)` reports write access or
/// better. An App or bot never adopts, the fleet's own included, whatever
/// [`TrustPolicy`] says about its comments.
fn maintainer(
    policy: &TrustPolicy,
    actor: &Value,
    association_of: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let login = human_login(actor)?;
    // Built by hand, never `Author::new`: the account kind is the forge's
    // `User` type checked above, so no App rule of the policy can match.
    let mut author = Author {
        login: Some(login.to_string()),
        association: None,
        app: false,
    };
    if !policy.trusts(&author) {
        author.association = association_of(login);
    }
    policy.trusts(&author).then(|| login.to_string())
}

/// The labels whose arrival, next to an `unlabeled loom:issue` by the same
/// actor, makes that removal a move of Loom's own state machine: a Builder
/// claim (`loom:issue` → `loom:building`) and the landed-route / rescope send
/// back to Curator (`loom:issue` → `loom:triage`).
const SUCCESSOR_LABELS: &[&str] = &["loom:building", "loom:triage"];

/// How far apart, in seconds, the removal and the successor label may be
/// recorded and still be one transition (one `gh issue edit` is two events,
/// observed 0-1 s apart).
const TRANSITION_WINDOW_SECS: i64 = 30;

fn is_label_event(e: &Value, event: &str, label: &str) -> bool {
    e.get("event").and_then(Value::as_str) == Some(event)
        && e.pointer("/label/name").and_then(Value::as_str) == Some(label)
}

fn actor_login(e: &Value) -> Option<&str> {
    e.pointer("/actor/login").and_then(Value::as_str)
}

/// Whether the `unlabeled loom:issue` event `removal` (recorded at `at`) is a
/// state-machine transition rather than a withdrawal: the **same actor** also
/// applied a [`SUCCESSOR_LABELS`] label within [`TRANSITION_WINDOW_SECS`] of
/// it, and that actor is trusted (a fleet identity or a maintainer). A bare
/// removal, one by an actor that cannot be read or is not trusted, or one
/// whose successor label came from someone else, is a withdrawal.
fn is_transition(
    policy: &TrustPolicy,
    events: &[Value],
    removal: &Value,
    at: chrono::DateTime<chrono::Utc>,
    association_of: &impl Fn(&str) -> Option<String>,
) -> bool {
    let Some(who) = actor_login(removal) else {
        return false;
    };
    let moved_on = events.iter().any(|e| {
        SUCCESSOR_LABELS
            .iter()
            .any(|l| is_label_event(e, "labeled", l))
            && actor_login(e).is_some_and(|a| a.eq_ignore_ascii_case(who))
            && instant(e.get("created_at"))
                .is_some_and(|t| (t - at).num_seconds().abs() <= TRANSITION_WINDOW_SECS)
    });
    moved_on
        && removal
            .get("actor")
            .and_then(|a| actor_trusted(policy, a, association_of))
            .is_some()
}

/// The login of the maintainer whose `loom:issue` adoption of the issue
/// stands, with the time they applied it, read from its REST timeline
/// (`events`, oldest first) (#13578). `None`: no standing adoption.
///
/// An operator's hand adoption is recorded by the forge with an unforgeable
/// actor and survives later re-curation, so it is read back rather than
/// marked. Operator ruling 20261116-r15 q48:
///
/// - **Who adopts.** Only a human maintainer ([`maintainer`]). Timeline actors
///   carry no `author_association`, so `association_of(login)` supplies one.
///   A label applied by an App or bot, or by a human without write access, is
///   not an adoption.
/// - **What ends it.** Any `unlabeled loom:issue` after the latest adoption
///   that is not a state-machine transition ([`is_transition`]). A later
///   adoption by a maintainer is a new latest adoption and restores it.
///
/// Fail closed: a `labeled` event whose time or actor cannot be read does not
/// count, and an `unlabeled loom:issue` whose time cannot be read ends every
/// adoption.
#[must_use]
pub fn adopted_by(
    policy: &TrustPolicy,
    events: &[Value],
    association_of: impl Fn(&str) -> Option<String>,
) -> Option<(String, chrono::DateTime<chrono::Utc>)> {
    let (at, index, who) = events
        .iter()
        .enumerate()
        .filter(|(_, e)| is_label_event(e, "labeled", "loom:issue"))
        .filter_map(|(i, e)| {
            let at = instant(e.get("created_at"))?;
            let who =
                maintainer(policy, e.get("actor").filter(|a| a.is_object())?, &association_of)?;
            Some((at, i, who))
        })
        .max_by_key(|(at, i, _)| (*at, *i))?;
    let withdrawn = events
        .iter()
        .enumerate()
        .filter(|(_, e)| is_label_event(e, "unlabeled", "loom:issue"))
        .any(|(i, e)| match instant(e.get("created_at")) {
            None => true,
            Some(t) if t < at || (t == at && i < index) => false,
            Some(t) => !is_transition(policy, events, e, t, &association_of),
        });
    (!withdrawn).then_some((who, at))
}

/// Whether the adoption at `adopted_at` still covers the issue body (#13578,
/// operator ruling r7-q6): no edit at or after it was made by an editor
/// outside the trust policy. `edits` are the issue's `userContentEdits` nodes
/// (`createdAt`, `editedAt`, `editor`). An edit whose time or editor cannot be
/// read counts as an untrusted one (fail closed); a later trusted adoption is
/// a later `adopted_at`, so it restores trust.
#[must_use]
pub fn body_unchanged_since(
    policy: &TrustPolicy,
    edits: &[Value],
    adopted_at: chrono::DateTime<chrono::Utc>,
    association_of: impl Fn(&str) -> Option<String>,
) -> bool {
    edits.iter().all(|edit| {
        let Some(at) = instant(
            edit.get("editedAt")
                .filter(|v| !v.is_null())
                .or(edit.get("createdAt")),
        ) else {
            return false;
        };
        at < adopted_at
            || edit
                .get("editor")
                .filter(|a| a.is_object())
                .and_then(|a| actor_trusted(policy, a, &association_of))
                .is_some()
    })
}

/// The adopter, when a maintainer's adoption stands ([`adopted_by`]) and no
/// untrusted editor has changed the body since it.
#[must_use]
pub fn standing_adoption(
    policy: &TrustPolicy,
    events: &[Value],
    edits: &[Value],
    association_of: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let (who, at) = adopted_by(policy, events, &association_of)?;
    body_unchanged_since(policy, edits, at, &association_of).then_some(who)
}

/// The one-time comment body explaining the hold, marker first. `stamp`
/// (`None`: the legacy marker) is what the verdict was reached from.
#[must_use]
pub fn notice_body(gate: &Gate, stamp: Option<&Stamp>) -> String {
    let why = match gate {
        Gate::Untrusted(Some(login)) => {
            format!("was filed by `{login}`, who is not a trusted author")
        }
        Gate::Untrusted(None) => "has no identifiable author".to_string(),
        Gate::Unknown(why) => format!("has an author that could not be verified ({why})"),
        Gate::Trusted => return String::new(),
    };
    let marker = stamp.map_or_else(|| NOTICE_MARKER.to_string(), Stamp::marker);
    format!(
        "{marker}\n**Champion: held at `loom:curated` — author not trusted**\n\n\
         This issue {why} (the comment-trust rules, `comment-trust.md`). It may be curated, \
         but an automated verdict does not approve it for building. A trusted maintainer \
         adopts it by applying `loom:issue` by hand, or by re-filing it.\n"
    )
}

#[cfg(test)]
mod tests;
