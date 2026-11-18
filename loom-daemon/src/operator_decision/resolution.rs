//! The operator's recorded resolution of a ranked decision, and the one
//! verifier the premise gate reads it through (#14399).
//!
//! # Why this exists
//!
//! `deliberate=yes reversal=yes verdict=operator-decision` routes a proposal
//! to a human (#8396, #11795), and [`super::cli::apply`] publishes the ranked
//! options (#9344). Nothing carried the human's answer *back*: after the
//! operator chose an option the gate kept returning 11, and the only way past
//! it, rewriting the record to `verdict=clear`, is exactly what the gate
//! refuses (exit 12). This module is the missing return path.
//!
//! # The record
//!
//! One issue comment, written by `loom-daemon operator-decision resolve` and
//! anchored at the comment's start:
//!
//! ```text
//! <!-- loom:operator-resolution v=1 repo=o/r issue=7 decision=<sha256> scope=<sha256> option=keep option-digest=<sha256> effect=proceed -->
//! ```
//!
//! It binds the approval to:
//!
//! - the **repository and issue** (a copy on another issue or repo is inert);
//! - the **decision digest**, a hash of the whole validated decision (question,
//!   context, every option's id, label and why, ranking, recommendation,
//!   deadline, links). Keeping an option ID while changing what it means, or
//!   superseding the decision, changes it;
//! - the **option digest** of the approved option's full contents;
//! - the **scope digest**, a hash of the proposal (title and body) with the
//!   managed decision section and routine loom bookkeeping markers removed,
//!   so editing the proposal needs renewed approval while a label change, a
//!   new comment, or a `loom:estimate` marker does not;
//! - the approved **option ID** and its **effect**.
//!
//! # Who may resolve
//!
//! Only a comment whose forge-reported author is an *operator*
//! ([`crate::comment_trust::TrustPolicy::is_operator`]) counts. A fleet App,
//! this daemon's own identity, a `forge.trustedCommenters` entry and a
//! `CONTRIBUTOR` do not, so an agent that recommended an option cannot also
//! approve it. A marker from anyone else is prose.
//!
//! **Residual limit, stated rather than hidden:** the forge reports an
//! account, not a person. When automation authenticates as the operator's own
//! user account, the verifier cannot tell the two apart. The `resolve` command
//! narrows that to an explicit attended action (TTY, typed confirmation,
//! refused under a dispatched sweep), and nothing else in the pipeline can
//! mint the record.
//!
//! # What the verifier answers
//!
//! [`verify`] returns a [`Status`]. Only [`Status::Resolved`] with
//! [`Effect::Proceed`] opens the gate. Everything else — no record, a stale
//! one, conflicting ones, a malformed one, an unreadable decision, no
//! authoritative read — leaves the reversal pending, with a diagnostic.

use super::render::{extract_block, strip_prior_decision};
use super::validate::{parse, validate};
use super::{Decision, DecisionOption};
use sha2::{Digest, Sha256};

/// The anchored start of a resolution comment.
pub const MARKER: &str = "<!-- loom:operator-resolution";
/// The record format version.
pub const VERSION: &str = "1";

/// What the operator's choice means for the proposal as filed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Go ahead with the selected option; the only effect that opens the gate.
    Proceed,
    /// The operator declined the proposal.
    Reject,
    /// Not now.
    Defer,
    /// The choice needs the proposal rewritten first; the current text is not
    /// approved.
    Rescope,
}

impl Effect {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Proceed => "proceed",
            Effect::Reject => "reject",
            Effect::Defer => "defer",
            Effect::Rescope => "rescope",
        }
    }

    /// # Errors
    /// The unknown value.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "proceed" => Ok(Effect::Proceed),
            "reject" => Ok(Effect::Reject),
            "defer" => Ok(Effect::Defer),
            "rescope" => Ok(Effect::Rescope),
            other => Err(format!("effect={other} (want proceed|reject|defer|rescope)")),
        }
    }
}

/// One parsed resolution record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Lower-cased `owner/repo`.
    pub repo: String,
    pub issue: u64,
    pub decision: String,
    pub scope: String,
    pub option: String,
    pub option_digest: String,
    pub effect: Effect,
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn is_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An option ID the marker can carry verbatim.
#[must_use]
pub fn id_is_markable(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn trimmed_option(o: &DecisionOption) -> DecisionOption {
    DecisionOption {
        id: o.id.trim().to_string(),
        label: o.label.trim().to_string(),
        why: o.why.as_ref().map(|w| w.trim().to_string()),
    }
}

/// Hash of one option's full contents.
#[must_use]
pub fn digest_option(o: &DecisionOption) -> String {
    sha256_hex(&serde_json::to_string(&trimmed_option(o)).unwrap_or_default())
}

/// Hash of the whole decision: the question, context and every option, in
/// ranked order, plus the recommendation, deadline and links.
#[must_use]
pub fn digest_decision(d: &Decision) -> String {
    let canon = Decision {
        question: d.question.trim().to_string(),
        context: d.context.trim().to_string(),
        options: d.options.iter().map(trimmed_option).collect(),
        recommended: d.recommended.as_ref().map(|r| r.trim().to_string()),
        deadline: d.deadline.as_ref().map(|r| r.trim().to_string()),
        context_links: d
            .context_links
            .iter()
            .map(|l| l.trim().to_string())
            .collect(),
    };
    sha256_hex(&serde_json::to_string(&canon).unwrap_or_default())
}

/// A standalone `<!-- loom:… -->` bookkeeping line other than the premise
/// record itself (estimate, points, dashboard link, …): routine machinery
/// that a queue transition rewrites, not part of the proposal.
fn is_bookkeeping_marker(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("<!-- loom:") && t.ends_with("-->") && !t.contains("loom:premise-check")
}

/// Hash of the proposal: title plus body with the managed decision section,
/// the `## Original report` wrapper heading and bookkeeping markers removed.
/// Labels and comments are not inputs.
#[must_use]
pub fn digest_scope(title: &str, body: &str) -> String {
    let stripped = strip_prior_decision(body);
    let kept: Vec<&str> = stripped
        .lines()
        .filter(|l| !is_bookkeeping_marker(l) && l.trim() != super::render::ORIGINAL_REPORT_HEADING)
        .map(str::trim_end)
        .collect();
    sha256_hex(&format!("{}\n{}", title.trim(), kept.join("\n").trim()))
}

/// Render the marker line for `r`.
#[must_use]
pub fn render_marker(r: &Resolution) -> String {
    format!(
        "{MARKER} v={VERSION} repo={} issue={} decision={} scope={} option={} option-digest={} \
         effect={} -->",
        r.repo,
        r.issue,
        r.decision,
        r.scope,
        r.option,
        r.option_digest,
        r.effect.as_str()
    )
}

/// The comment body `resolve` posts: the marker first, then readable text.
#[must_use]
pub fn render_comment(r: &Resolution, label: &str) -> String {
    format!(
        "{}\n**Operator resolution recorded** on #{}: option `{}` ({}), effect `{}`.\n\n\
         Bound to decision `{}` and proposal scope `{}`. A change to either, or a different \
         issue, needs a new resolution. This comment is the record `premise-check` verifies \
         (`defaults/docs/premise-gate.md`).\n",
        render_marker(r),
        r.issue,
        r.option,
        label.replace(['\r', '\n'], " ").trim(),
        r.effect.as_str(),
        &r.decision[..12],
        &r.scope[..12],
    )
}

/// Parse a comment that starts with the marker.
///
/// # Errors
///
/// A human-readable reason. Unknown keys are errors, as in the premise
/// record: a typo must not silently drop a binding.
pub fn parse_marker(body: &str) -> Result<Resolution, String> {
    let rest = body
        .trim_start()
        .strip_prefix(MARKER)
        .ok_or("comment does not start with the resolution marker")?;
    let span = &rest[..rest.find("-->").ok_or("marker is not closed with -->")?];
    let (mut v, mut repo, mut issue, mut decision) = (None, None, None, None);
    let (mut scope, mut option, mut option_digest, mut effect) = (None, None, None, None);
    for field in span.split_whitespace() {
        let (k, val) = field
            .split_once('=')
            .ok_or_else(|| format!("field `{field}` is not key=value"))?;
        let slot = match k {
            "v" => &mut v,
            "repo" => &mut repo,
            "issue" => &mut issue,
            "decision" => &mut decision,
            "scope" => &mut scope,
            "option" => &mut option,
            "option-digest" => &mut option_digest,
            "effect" => &mut effect,
            other => return Err(format!("unknown key `{other}`")),
        };
        if slot.replace(val.to_string()).is_some() {
            return Err(format!("duplicate key `{k}`"));
        }
    }
    let req = |o: Option<String>, k: &str| o.ok_or_else(|| format!("missing {k}="));
    if req(v, "v")? != VERSION {
        return Err(format!("unsupported record version (want v={VERSION})"));
    }
    let repo = req(repo, "repo")?.to_ascii_lowercase();
    if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
        return Err(format!("repo={repo} (want owner/repo)"));
    }
    let issue = req(issue, "issue")?
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("issue= is not a positive number")?;
    let digest = |o: Option<String>, k: &str| -> Result<String, String> {
        let s = req(o, k)?;
        if is_digest(&s) {
            Ok(s)
        } else {
            Err(format!("{k}= is not a lowercase sha256"))
        }
    };
    let option = req(option, "option")?;
    if !id_is_markable(&option) {
        return Err("option= is not a valid option id".to_string());
    }
    Ok(Resolution {
        repo,
        issue,
        decision: digest(decision, "decision")?,
        scope: digest(scope, "scope")?,
        option,
        option_digest: digest(option_digest, "option-digest")?,
        effect: Effect::parse(&req(effect, "effect")?)?,
    })
}

/// The decision instance and proposal as they stand right now, read from the
/// authoritative forge body.
#[derive(Debug, Clone)]
pub struct Current {
    pub repo: String,
    pub issue: u64,
    pub decision: Decision,
    pub decision_digest: String,
    pub scope_digest: String,
}

/// Read the current decision with the existing ranked-option parser.
///
/// `body` must be the issue's **raw** body: the digests are over exactly what
/// the operator reviewed.
///
/// # Errors
///
/// Why the decision cannot be read (absent, not JSON, or failing the
/// contract). Never "no decision, proceed".
pub fn current(repo: &str, issue: u64, title: &str, body: &str) -> Result<Current, String> {
    let block = extract_block(body).ok_or("the issue body carries no ```decision block")?;
    let decision = parse(&block).map_err(|r| r.to_string())?;
    let reasons = validate(&decision);
    if !reasons.is_empty() {
        return Err(format!(
            "the published decision fails the ranked-options contract: {}",
            reasons
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(Current {
        repo: repo.to_ascii_lowercase(),
        issue,
        decision_digest: digest_decision(&decision),
        scope_digest: digest_scope(title, body),
        decision,
    })
}

/// One issue comment, with the forge's verdict on its author already applied.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub body: String,
    /// The author is an operator ([`crate::comment_trust::TrustPolicy::is_operator`]).
    pub operator: bool,
}

/// The candidates in a comment listing (REST or GraphQL shape), each with the
/// forge's verdict on its author. One function for every read path, so the
/// authorization decision is the same on all of them.
#[must_use]
pub fn candidates(
    policy: &crate::comment_trust::TrustPolicy,
    items: &[serde_json::Value],
) -> Vec<Candidate> {
    items
        .iter()
        .filter_map(|v| {
            Some(Candidate {
                body: v.get("body")?.as_str()?.to_string(),
                operator: policy.is_operator_json(v),
            })
        })
        .collect()
}

/// Everything [`verify`] reads. `current` is `Err` when the decision could
/// not be read from the authoritative body.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub current: Result<Current, String>,
    pub candidates: Vec<Candidate>,
}

/// The verifier's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// A verified operator resolution of the current decision and scope.
    Resolved(Resolution),
    /// No operator resolution exists.
    Pending,
    /// Operator resolutions exist, none for this decision, scope and option
    /// contents.
    Stale(String),
    /// Operator resolutions for the current decision disagree.
    Conflict(String),
    /// An operator-authored resolution comment does not parse.
    Malformed(String),
    /// The decision could not be read.
    Unreadable(String),
    /// No authoritative forge read was available (offline reproduction).
    Unavailable(String),
}

impl Status {
    /// Stable token for the `RESOLUTION=` line.
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self {
            Status::Resolved(r) => match r.effect {
                Effect::Proceed => "approved",
                Effect::Reject => "rejected",
                Effect::Defer => "deferred",
                Effect::Rescope => "rescope",
            },
            Status::Pending => "pending",
            Status::Stale(_) => "stale",
            Status::Conflict(_) => "conflict",
            Status::Malformed(_) => "malformed",
            Status::Unreadable(_) => "unreadable",
            Status::Unavailable(_) => "unavailable",
        }
    }

    /// The diagnostic for a non-resolving status.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Status::Resolved(r) if r.effect == Effect::Proceed => None,
            Status::Resolved(r) => Some(format!(
                "the operator chose option `{}` with effect `{}`, which is not approval to \
                 proceed with this proposal",
                r.option,
                r.effect.as_str()
            )),
            Status::Pending => Some(
                "no operator resolution on this issue; record one with `loom-daemon \
                 operator-decision resolve`"
                    .to_string(),
            ),
            Status::Stale(why)
            | Status::Conflict(why)
            | Status::Malformed(why)
            | Status::Unreadable(why)
            | Status::Unavailable(why) => Some(why.clone()),
        }
    }

    /// Does this open the premise gate?
    #[must_use]
    pub fn approves(&self) -> bool {
        matches!(self, Status::Resolved(r) if r.effect == Effect::Proceed)
    }
}

/// [`verify`]'s answer and how many resolution-shaped comments were ignored
/// for lack of operator authority (a diagnostic, never an input to the gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub status: Status,
    pub ignored_unauthorized: usize,
}

impl Assessment {
    #[must_use]
    pub fn unavailable(why: &str) -> Self {
        Self {
            status: Status::Unavailable(why.to_string()),
            ignored_unauthorized: 0,
        }
    }
}

/// Verify the operator's resolution of the current decision.
#[must_use]
pub fn verify(ev: &Evidence) -> Assessment {
    let cur = match &ev.current {
        Ok(c) => c,
        Err(why) => {
            return Assessment {
                status: Status::Unreadable(why.clone()),
                ignored_unauthorized: 0,
            }
        }
    };
    let mut ignored = 0;
    let mut malformed: Option<String> = None;
    let mut records = Vec::new();
    for c in &ev.candidates {
        if !c.body.trim_start().starts_with(MARKER) {
            continue;
        }
        if !c.operator {
            ignored += 1;
            continue;
        }
        match parse_marker(&c.body) {
            Ok(r) => records.push(r),
            Err(e) => {
                malformed.get_or_insert(e);
            }
        }
    }
    let done = |status| Assessment {
        status,
        ignored_unauthorized: ignored,
    };
    if let Some(e) = malformed {
        return done(Status::Malformed(format!(
            "an operator resolution comment does not parse ({e}); fix or delete it"
        )));
    }
    if records.is_empty() {
        return done(Status::Pending);
    }
    let mut live: Vec<&Resolution> = Vec::new();
    let mut stale = String::new();
    for r in &records {
        let why = if r.repo != cur.repo || r.issue != cur.issue {
            Some(format!(
                "a resolution is bound to {}#{}, not {}#{}",
                r.repo, r.issue, cur.repo, cur.issue
            ))
        } else if r.decision != cur.decision_digest {
            Some(
                "a resolution is bound to a different decision (it was edited or superseded)"
                    .to_string(),
            )
        } else if r.scope != cur.scope_digest {
            Some(
                "a resolution is bound to a different proposal scope (the issue was edited)"
                    .to_string(),
            )
        } else {
            match cur
                .decision
                .options
                .iter()
                .find(|o| o.id.trim() == r.option)
            {
                None => Some(format!("resolution option `{}` names no current option", r.option)),
                Some(o) if digest_option(o) != r.option_digest => Some(format!(
                    "option `{}` has different contents than the one approved",
                    r.option
                )),
                Some(_) => None,
            }
        };
        match why {
            None => live.push(r),
            Some(w) => stale = w,
        }
    }
    let Some(first) = live.first() else {
        return done(Status::Stale(format!("{stale}; a new resolution is needed")));
    };
    if live
        .iter()
        .any(|r| r.option != first.option || r.effect != first.effect)
    {
        return done(Status::Conflict(
            "operator resolutions for this decision disagree; delete the superseded comment"
                .to_string(),
        ));
    }
    done(Status::Resolved((*first).clone()))
}

#[cfg(test)]
#[path = "resolution_tests.rs"]
pub(crate) mod tests;
