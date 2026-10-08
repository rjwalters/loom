//! The author gate on automatic promotion (#10827).
//!
//! The comment-trust rule (#9548) decides which *markers* count; it says
//! nothing about who wrote the issue being promoted. This is the one shared
//! predicate every automatic `loom:curated` → `loom:issue` write asks first
//! (Champion's Step 3b and its Pass 0c backstop, `check-promotion-landed.sh`):
//!
//! 1. a **trusted body author** ([`super::TrustPolicy`]) passes;
//! 2. otherwise a **direct operator star** with trusted provenance passes:
//!    `loom:operator-priority` or `loom:operator-high-priority` on the issue,
//!    whose newest `labeled` event's actor is trusted: a trusted author
//!    ([`super::TrustPolicy`], e.g. a fleet App or admin) or a user whose
//!    repository role is `triage` or better. The label alone is not enough:
//!    an App with issues-write (the very App that files bot issues) can
//!    apply it. The daemon-written `*-inherited` stars never count here;
//! 3. otherwise a **verified signed decision** ([`super::decision`]) whose
//!    newest record is `approve` passes; a newest `reject`/`defer` holds;
//! 4. anything else holds, and a failed read is [`Gate::Unavailable`], which
//!    never promotes either.
//!
//! Passing the gate is not approval: every other promotion criterion still
//! applies. A held issue is left exactly as it was.

use std::path::Path;

use serde_json::Value;

use super::decision::{self, Context, Evaluation, Location, OperatorDecision};
use super::{Author, TrustPolicy};

/// The direct, human-applied star labels (level 1 and level 2).
pub const STAR_LABELS: &[&str] = &["loom:operator-priority", "loom:operator-high-priority"];

/// The dedup marker of the one explanatory comment a hold may post.
pub const NOTICE_MARKER: &str = "<!-- loom:promotion-author-gate -->";

/// The gate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// May be promoted (on the other criteria); the reason names the signal.
    Eligible(String),
    /// Must not be promoted; the reason says what would change that.
    Hold(String),
    /// The inputs could not be read; must not be promoted this pass.
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

    /// The reason text.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Eligible(r) | Self::Hold(r) | Self::Unavailable(r) => r,
        }
    }
}

/// Repository roles that can apply labels, i.e. place a star.
pub const STAR_ROLES: &[&str] = &["admin", "maintain", "write", "triage"];

/// Everything the predicate reads.
pub struct Inputs<'a> {
    /// The REST issue object (`user`, `author_association`, `labels`,
    /// `repository_url`, `number`).
    pub issue: &'a Value,
    /// The REST comment listing, `None` when it could not be read.
    pub comments: Option<&'a [Value]>,
    /// The REST issue events listing (who applied which label), `None` when
    /// it could not be read. Consulted only for a star on an issue whose
    /// author is untrusted.
    pub events: Option<&'a [Value]>,
    /// A user's repository role (`role_name`: `admin`, `maintain`, `write`,
    /// `triage`, `read`, ...), `None` when it could not be read.
    pub role_of: &'a dyn Fn(&str) -> Option<String>,
    /// The workspace's author-trust rules.
    pub policy: &'a TrustPolicy,
    /// Signer keys, admin roster, clock and window.
    pub ctx: Context<'a>,
}

fn labels(issue: &Value) -> Vec<&str> {
    issue
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str).or_else(|| l.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// Who applied `label` last: the actor of the newest `labeled` event for it
/// (events are listed oldest first). `None` when no such event is listed.
fn star_applier<'v>(events: &'v [Value], label: &str) -> Option<&'v Value> {
    events
        .iter()
        .rev()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("labeled"))
        .find(|e| e.pointer("/label/name").and_then(Value::as_str) == Some(label))
        .and_then(|e| e.get("actor"))
        .filter(|a| a.is_object())
}

/// Provenance of one star.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provenance {
    /// Applied by this trusted actor.
    Trusted(String),
    /// Applied by an untrusted (or unknown) actor.
    Untrusted,
    /// The applier's standing could not be read.
    Unknown,
}

fn star_provenance(events: &[Value], label: &str, inputs: &Inputs<'_>) -> Provenance {
    let Some(actor) = star_applier(events, label) else {
        return Provenance::Untrusted;
    };
    let Some(login) = actor
        .get("login")
        .and_then(Value::as_str)
        .filter(|l| !l.trim().is_empty())
    else {
        return Provenance::Untrusted;
    };
    let typed_bot = actor.get("type").and_then(Value::as_str) == Some("Bot");
    let mut who = Author::new(Some(login), None);
    who.app = who.app || typed_bot;
    if inputs.policy.trusts(&who) {
        return Provenance::Trusted(login.to_string());
    }
    // A user account with triage or better is a repo insider; an App's role
    // is never asked (its trust is the fleet roster, above).
    if who.app {
        return Provenance::Untrusted;
    }
    match (inputs.role_of)(login) {
        Some(role) if STAR_ROLES.contains(&role.as_str()) => Provenance::Trusted(login.to_string()),
        Some(_) => Provenance::Untrusted,
        None => Provenance::Unknown,
    }
}

fn describe(author: &Author) -> String {
    format!(
        "{} ({})",
        author.login.as_deref().unwrap_or("<unknown>"),
        author.association.as_deref().unwrap_or("no association")
    )
}

/// The decision (see the module docs), plus a local-only diagnostic line
/// (ignored-marker reason counts, signer/roster state). The diagnostic may
/// name the fleet store, so it is printed for the operator and never posted;
/// every [`Gate`] reason is safe to post.
#[must_use]
pub fn decide(inputs: &Inputs<'_>) -> (Gate, String) {
    let issue = inputs.issue;
    if !issue.is_object() || issue.get("number").and_then(Value::as_u64).is_none() {
        return (
            Gate::Unavailable("the issue object could not be read".to_string()),
            String::new(),
        );
    }
    let author = Author::from_json(issue);
    if inputs.policy.trusts(&author) {
        return (
            Gate::Eligible(format!("trusted body author {}", describe(&author))),
            String::new(),
        );
    }
    let labels = labels(issue);
    // A star whose provenance could not be read must not become a HOLD
    // notice ("no star"): the answer is UNAVAILABLE unless something else
    // already decides.
    let mut star_unknown = None;
    for star in STAR_LABELS.iter().filter(|s| labels.contains(s)) {
        let Some(events) = inputs.events else {
            star_unknown = Some(format!("who applied {star} could not be read"));
            continue;
        };
        match star_provenance(events, star, inputs) {
            Provenance::Trusted(by) => {
                let why = format!(
                    "operator star {star} applied by {by} (body author {} is untrusted)",
                    describe(&author)
                );
                return (Gate::Eligible(why), String::new());
            }
            Provenance::Unknown => {
                star_unknown = Some(format!(
                    "the repository role of whoever applied {star} could not be read"
                ));
            }
            Provenance::Untrusted => {}
        }
    }
    let Some(location) = Location::from_issue_object(issue) else {
        let why = "the issue object names no repository_url/number".to_string();
        return (Gate::Unavailable(why), String::new());
    };
    let Some(comments) = inputs.comments else {
        let why = "the issue's comments could not be read".to_string();
        return (Gate::Unavailable(why), String::new());
    };
    let eval: Evaluation = decision::newest(comments, &location, &inputs.ctx);
    let detail = format!(
        "ignored decision markers: {}; signer keys: {}; admin roster: {}{}",
        eval.rejected_summary(),
        inputs.ctx.signers.state,
        inputs.ctx.admins.state,
        star_unknown
            .as_deref()
            .map(|w| format!("; star: {w}"))
            .unwrap_or_default(),
    );
    let gate = match eval.newest {
        Some(d) if d.decision == OperatorDecision::Approve => Gate::Eligible(format!(
            "signed decision=approve by={} at={} key={}",
            d.by,
            decision::format_at(d.at),
            d.key
        )),
        Some(d) => Gate::Hold(format!(
            "the newest signed operator decision is {} (by {} at {}), not approve",
            d.decision.as_str(),
            d.by,
            decision::format_at(d.at),
        )),
        None => match star_unknown {
            Some(why) => Gate::Unavailable(why),
            None => Gate::Hold(format!(
                "the body author {} is not a trusted author, and the issue has no operator star \
                 applied by a trusted account and no verified signed operator decision",
                describe(&author),
            )),
        },
    };
    (gate, detail)
}

/// Whether a hold's explanatory comment is already on the issue: a
/// [`NOTICE_MARKER`] comment by a trusted author (so nobody else's quote can
/// suppress it).
#[must_use]
pub fn notice_posted(comments: &[Value], policy: &TrustPolicy) -> bool {
    comments.iter().any(|c| {
        policy.trusts_json(c)
            && c.get("body")
                .and_then(Value::as_str)
                .is_some_and(|b| b.trim_start().starts_with(NOTICE_MARKER))
    })
}

/// The one-line explanatory comment for a hold.
#[must_use]
pub fn notice_body(gate: &Gate) -> String {
    format!(
        "{NOTICE_MARKER} **Not promoted automatically**: {}. An operator can approve it by \
         starring it (`loom:operator-priority`) or recording a signed decision; see \
         `.loom/docs/comment-trust.md` (#10827).",
        gate.reason()
    )
}

/// One evaluation: the gate, its local-only diagnostic, and whether the hold
/// notice is already posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The answer.
    pub gate: Gate,
    /// Local diagnostics (never posted).
    pub detail: String,
    /// A trusted [`NOTICE_MARKER`] comment is already on the issue.
    pub notice_posted: bool,
}

/// Resolve every input for issue `number` of `repo` (`owner/name`, or
/// `{owner}/{repo}` for the checkout's) and decide.
#[must_use]
pub fn evaluate(root: &Path, repo: &str, number: u64) -> Outcome {
    let n = number.to_string();
    let Some(issue) = super::records::fetch_issue_object(repo, &n, root, false) else {
        return Outcome {
            gate: Gate::Unavailable(format!("could not read issue #{number}")),
            detail: String::new(),
            notice_posted: false,
        };
    };
    let listing = super::records::fetch_comment_listing(repo, &n, root, false);
    let comments = listing.as_deref().and_then(super::parse_listing);
    let events = super::records::fetch_issue_events(repo, &n, root)
        .as_deref()
        .and_then(super::parse_listing);
    let role_of = |login: &str| fetch_role(repo, login, root);
    let policy = TrustPolicy::for_root(root);
    let effective = crate::config_resolver::resolve_effective_config(root);
    let signers = crate::fleet_store::decision_signers::resolve(root);
    let admins = crate::fleet_store::admins::resolve(root);
    let ctx = Context {
        signers: &signers,
        admins: &admins,
        now: chrono::Utc::now(),
        max_age: decision::max_age_from_config(&effective),
    };
    let (gate, detail) = decide(&Inputs {
        issue: &issue,
        comments: comments.as_deref(),
        events: events.as_deref(),
        role_of: &role_of,
        policy: &policy,
        ctx,
    });
    Outcome {
        notice_posted: comments
            .as_deref()
            .is_some_and(|c| notice_posted(c, &policy)),
        gate,
        detail,
    }
}

/// A user's repository `role_name`. `None` on any failure (a non-collaborator
/// is reported by the forge as `read`/`none`, not as a failure).
fn fetch_role(repo: &str, login: &str, root: &Path) -> Option<String> {
    let valid = !login.is_empty()
        && login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-');
    if !valid {
        return None;
    }
    let path = format!("repos/{repo}/collaborators/{login}/permission");
    let out = crate::script_helpers::run_gh(&["api", &path], root, false);
    let v: Value = serde_json::from_slice(&out.ok_output()?.stdout).ok()?;
    v.get("role_name")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests;
