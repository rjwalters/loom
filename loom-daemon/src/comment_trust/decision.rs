//! Signed operator-decision records (#10827).
//!
//! A fleet may record an operator's decision on an issue through a tool (a
//! dashboard behind a narrow GitHub App) that is **not** a trusted author:
//! the same App files bot issues, which must stay untrusted. Its comments are
//! prose under the author rule ([`super::TrustPolicy`]). One line of such a
//! comment can still count, as exactly one **record**, when it is a signed
//! marker that verifies:
//!
//! ```text
//! <!-- loom:operator-decision v1 repo=<owner/name> issue=<n> decision=<word> by=<login> at=<YYYY-MM-DDTHH:MM:SSZ> key=<key-id> sig=ed25519:<base64> -->
//! ```
//!
//! **Scope.** Verification authorizes only the parsed record: that fleet admin
//! `by` decided `decision` on issue `issue` of `repo` at `at`. It never
//! trusts the comment's other text, its author, that author's other comments
//! or markers, or the issue body. [`super::TrustPolicy::trusts`] is unchanged
//! and knows nothing about this module.
//!
//! **Signed bytes** ([`canonical_bytes`]), UTF-8 (ASCII by construction),
//! seven `\n`-terminated lines in this order, terminal newline included:
//!
//! ```text
//! loom:operator-decision v1
//! repo=<repo>
//! issue=<issue>
//! decision=<decision>
//! by=<by>
//! at=<at>
//! key=<key>
//! ```
//!
//! **Strict parsing** ([`parse_marker`]): the marker is a whole line of the
//! comment (one trailing `\r` allowed), fields in exactly the order above,
//! one space apart, each named once. Anything else (a duplicate, unknown or
//! reordered field, extra whitespace, trailing data, upper-case repo/login,
//! a non-canonical timestamp, issue number or base64) is rejected, never
//! normalised. A comment with more than one marker line counts for nothing.
//!
//! **Verification** ([`verify`]): the record must sit where it claims (the
//! comment's own `issue_url`, else the caller's issue), name an **active** key
//! of [`crate::fleet_store::decision_signers`], carry a valid signature under
//! it, name a `by` on the fleet admin roster (checked independently: holding
//! a key adds no admin), and be fresh: `at` no older than the configured
//! window ([`MAX_AGE_KEY`]) and no more than [`FUTURE_SKEW`] ahead. Every
//! failure is a [`Rejection`] and leaves the line prose.
//!
//! **Replay** ([`newest`]): among verified, in-window records for the issue,
//! the greatest `at` wins; on equal `at` the more conservative decision wins
//! (`reject` > `defer` > `approve`), then the greater `(key, sig)`. Invalid
//! records take no part, so a newer forged marker cannot shadow an older
//! valid one, and once every record ages out there is no decision at all.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

use crate::fleet_store::admins::Admins;
use crate::fleet_store::decision_signers::{decode_canonical, valid_key_id, Signers};

/// Every marker line starts with this.
pub const DECISION_MARKER_PREFIX: &str = "<!-- loom:operator-decision ";
/// The domain-separation line that begins the signed bytes.
pub const SIGNED_DOMAIN: &str = "loom:operator-decision v1";
/// Config key for the past freshness window, in seconds.
pub const MAX_AGE_KEY: &str = "forge.operatorDecisionMaxAgeSecs";
/// Default past window: 7 days.
pub const DEFAULT_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;
/// Upper bound on a configured window: 30 days.
pub const MAX_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;
/// How far `at` may lie in the future (clock skew): 5 minutes.
pub const FUTURE_SKEW: chrono::Duration = chrono::Duration::minutes(5);

/// The operator-decision vocabulary. A cryptographically valid marker naming
/// any other word is prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperatorDecision {
    /// Go ahead: the operator approves the issue for work.
    Approve,
    /// Not now: leave the issue where it is.
    Defer,
    /// No: do not work the issue.
    Reject,
}

impl OperatorDecision {
    /// Parse one decision word (exact, lower case).
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "approve" => Some(Self::Approve),
            "defer" => Some(Self::Defer),
            "reject" => Some(Self::Reject),
            _ => None,
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Defer => "defer",
            Self::Reject => "reject",
        }
    }
}

/// Why a marker line is not a decision record (a reason category; never the
/// payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rejection {
    /// Not in the exact v1 shape.
    Malformed,
    /// A version other than `v1`.
    UnknownVersion,
    /// A word outside [`OperatorDecision`].
    UnknownDecision,
    /// More than one marker line in the comment.
    Ambiguous,
    /// `repo` is not where the comment sits.
    WrongRepo,
    /// `issue` is not where the comment sits.
    WrongIssue,
    /// The signer keys could not be read.
    SignersUnavailable,
    /// `key` is not an active key (unknown, revoked or removed).
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
    /// The admin roster could not be read.
    AdminsUnavailable,
    /// `by` is not a fleet admin.
    NotAdmin,
    /// `at` is older than the window.
    Expired,
    /// `at` is too far in the future.
    FutureSkew,
}

impl Rejection {
    /// Stable kebab-case name for diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnknownVersion => "unknown-version",
            Self::UnknownDecision => "unknown-decision",
            Self::Ambiguous => "ambiguous",
            Self::WrongRepo => "wrong-repo",
            Self::WrongIssue => "wrong-issue",
            Self::SignersUnavailable => "signers-unavailable",
            Self::UnknownKey => "unknown-key",
            Self::BadSignature => "bad-signature",
            Self::AdminsUnavailable => "admins-unavailable",
            Self::NotAdmin => "not-admin",
            Self::Expired => "expired",
            Self::FutureSkew => "future-skew",
        }
    }
}

/// A strictly parsed (not yet verified) marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// `owner/name`, lower case.
    pub repo: String,
    /// The issue number (positive).
    pub issue: u64,
    /// The decision.
    pub decision: OperatorDecision,
    /// The deciding admin's login, lower case.
    pub by: String,
    /// When the decision was made.
    pub at: DateTime<Utc>,
    /// The signing key's id.
    pub key: String,
    /// The 64-byte Ed25519 signature.
    pub sig: [u8; 64],
}

/// A verified decision record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDecision {
    /// `owner/name`, lower case.
    pub repo: String,
    /// The issue number.
    pub issue: u64,
    /// The decision.
    pub decision: OperatorDecision,
    /// The fleet admin who decided.
    pub by: String,
    /// When.
    pub at: DateTime<Utc>,
    /// Which key signed it.
    pub key: String,
    /// The signature (tie-break only).
    pub sig: [u8; 64],
}

/// The signed bytes for a record (see the module docs). `at` must already be
/// the canonical `YYYY-MM-DDTHH:MM:SSZ` spelling.
#[must_use]
pub fn canonical_bytes(
    repo: &str,
    issue: u64,
    decision: OperatorDecision,
    by: &str,
    at: &str,
    key: &str,
) -> String {
    format!(
        "{SIGNED_DOMAIN}\nrepo={repo}\nissue={issue}\ndecision={}\nby={by}\nat={at}\nkey={key}\n",
        decision.as_str()
    )
}

/// The canonical spelling of a timestamp.
#[must_use]
pub fn format_at(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn name_char(c: u8, extra: &[u8]) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(&c)
}

/// A canonical (lower-case) `owner/name`.
#[must_use]
pub fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && owner.bytes().all(|c| name_char(c, b"-"))
        && !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name.bytes().all(|c| name_char(c, b"-_."))
}

/// A canonical (lower-case) user login.
#[must_use]
pub fn valid_login(login: &str) -> bool {
    let b = login.as_bytes();
    !b.is_empty()
        && b.len() <= 39
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && b.iter().all(|c| name_char(*c, b"-"))
}

fn parse_issue(text: &str) -> Option<u64> {
    let ok = !text.is_empty()
        && text.len() <= 19
        && text.bytes().all(|c| c.is_ascii_digit())
        && !text.starts_with('0');
    ok.then(|| text.parse().ok()).flatten()
}

fn parse_at(text: &str) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    let at = naive.and_utc();
    (format_at(at) == text).then_some(at)
}

/// Strictly parse one marker line (see the module docs).
pub fn parse_marker(line: &str) -> Result<Marker, Rejection> {
    let inner = line
        .strip_prefix(DECISION_MARKER_PREFIX)
        .and_then(|r| r.strip_suffix(" -->"))
        .ok_or(Rejection::Malformed)?;
    let tokens: Vec<&str> = inner.split(' ').collect();
    match tokens.first() {
        Some(&"v1") => {}
        Some(v)
            if v.starts_with('v') && v.len() > 1 && v[1..].bytes().all(|c| c.is_ascii_digit()) =>
        {
            return Err(Rejection::UnknownVersion)
        }
        _ => return Err(Rejection::Malformed),
    }
    const NAMES: [&str; 7] = ["repo", "issue", "decision", "by", "at", "key", "sig"];
    if tokens.len() != 1 + NAMES.len() {
        return Err(Rejection::Malformed);
    }
    let mut values = [""; 7];
    for (i, name) in NAMES.iter().enumerate() {
        let value = tokens[i + 1]
            .strip_prefix(name)
            .and_then(|r| r.strip_prefix('='))
            .filter(|v| !v.is_empty())
            .ok_or(Rejection::Malformed)?;
        values[i] = value;
    }
    let [repo, issue, decision, by, at, key, sig] = values;
    if !valid_repo(repo) || !valid_login(by) || !valid_key_id(key) {
        return Err(Rejection::Malformed);
    }
    let issue = parse_issue(issue).ok_or(Rejection::Malformed)?;
    let at = parse_at(at).ok_or(Rejection::Malformed)?;
    let sig = sig
        .strip_prefix("ed25519:")
        .and_then(decode_canonical::<64>)
        .ok_or(Rejection::Malformed)?;
    let decision = OperatorDecision::parse(decision).ok_or(Rejection::UnknownDecision)?;
    Ok(Marker {
        repo: repo.to_string(),
        issue,
        decision,
        by: by.to_string(),
        at,
        key: key.to_string(),
        sig,
    })
}

/// Whether a line is a marker candidate (it claims to be one, well-formed or
/// not).
fn is_candidate(line: &str) -> bool {
    line.trim_start()
        .starts_with(DECISION_MARKER_PREFIX.trim_end())
}

/// The single marker line of a comment body: `None` when it has none,
/// `Err(Ambiguous)` when it has several.
pub fn marker_line(body: &str) -> Option<Result<&str, Rejection>> {
    let mut found = body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| is_candidate(l));
    let first = found.next()?;
    if found.next().is_some() {
        return Some(Err(Rejection::Ambiguous));
    }
    Some(Ok(first))
}

/// Where a decision must sit: `owner/name` (lower case) and issue number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// `owner/name`, lower case.
    pub repo: String,
    /// The issue number.
    pub issue: u64,
}

impl Location {
    /// From a REST `…/repos/<owner>/<name>/issues/<n>` URL (a comment's
    /// `issue_url`).
    #[must_use]
    pub fn from_issue_url(url: &str) -> Option<Self> {
        let rest = url.split_once("/repos/")?.1;
        let mut parts = rest.split('/');
        let (owner, name, kind, n) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if kind != "issues" || parts.next().is_some() {
            return None;
        }
        let repo = format!("{owner}/{name}").to_ascii_lowercase();
        Some(Self {
            issue: parse_issue(n)?,
            repo: valid_repo(&repo).then_some(repo)?,
        })
    }

    /// From a REST issue object (`repository_url` + `number`).
    #[must_use]
    pub fn from_issue_object(issue: &Value) -> Option<Self> {
        let repo_url = issue.get("repository_url")?.as_str()?;
        let n = issue.get("number")?.as_u64()?;
        Self::from_issue_url(&format!("{repo_url}/issues/{n}"))
    }
}

/// What verification needs besides the marker.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The signer keys.
    pub signers: &'a Signers,
    /// The fleet admin roster.
    pub admins: &'a Admins,
    /// Now.
    pub now: DateTime<Utc>,
    /// The past freshness window.
    pub max_age: chrono::Duration,
}

/// The configured past window, clamped to `0..=`[`MAX_MAX_AGE_SECS`].
/// Missing or non-integer config is the default.
#[must_use]
pub fn max_age_from_config(effective: &Value) -> chrono::Duration {
    let secs = crate::config_resolver::get_path(effective, MAX_AGE_KEY)
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_MAX_AGE_SECS)
        .clamp(0, MAX_MAX_AGE_SECS);
    chrono::Duration::seconds(secs)
}

fn signature_ok(public_key: &[u8; 32], message: &[u8], sig: &[u8; 64]) -> bool {
    use aws_lc_rs::signature::{UnparsedPublicKey, ED25519};
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(message, sig)
        .is_ok()
}

/// Verify one marker line found at `location`.
pub fn verify(
    line: &str,
    location: &Location,
    ctx: &Context<'_>,
) -> Result<VerifiedDecision, Rejection> {
    let m = parse_marker(line)?;
    if m.repo != location.repo {
        return Err(Rejection::WrongRepo);
    }
    if m.issue != location.issue {
        return Err(Rejection::WrongIssue);
    }
    if !ctx.signers.is_loaded() {
        return Err(Rejection::SignersUnavailable);
    }
    let public_key = ctx
        .signers
        .active_key(&m.key)
        .ok_or(Rejection::UnknownKey)?;
    let message = canonical_bytes(&m.repo, m.issue, m.decision, &m.by, &format_at(m.at), &m.key);
    if !signature_ok(public_key, message.as_bytes(), &m.sig) {
        return Err(Rejection::BadSignature);
    }
    if !ctx.admins.is_loaded() {
        return Err(Rejection::AdminsUnavailable);
    }
    if !ctx
        .admins
        .logins
        .iter()
        .any(|a| a.to_ascii_lowercase() == m.by)
    {
        return Err(Rejection::NotAdmin);
    }
    if m.at > ctx.now + FUTURE_SKEW {
        return Err(Rejection::FutureSkew);
    }
    if ctx.now - m.at > ctx.max_age {
        return Err(Rejection::Expired);
    }
    Ok(VerifiedDecision {
        repo: m.repo,
        issue: m.issue,
        decision: m.decision,
        by: m.by,
        at: m.at,
        key: m.key,
        sig: m.sig,
    })
}

/// The outcome of reading every comment of one issue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// The winning verified record, if any.
    pub newest: Option<VerifiedDecision>,
    /// How many marker lines were rejected, per reason.
    pub rejected: BTreeMap<Rejection, usize>,
}

impl Evaluation {
    /// `reason:count,…` for diagnostics (`none` when nothing was rejected).
    #[must_use]
    pub fn rejected_summary(&self) -> String {
        if self.rejected.is_empty() {
            return "none".to_string();
        }
        self.rejected
            .iter()
            .map(|(r, n)| format!("{}:{n}", r.as_str()))
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn wins(a: &VerifiedDecision, b: &VerifiedDecision) -> bool {
    (a.at, a.decision, &a.key, a.sig) > (b.at, b.decision, &b.key, b.sig)
}

/// The newest verified record among `comments` (REST comment objects, any
/// author) for `issue` (the issue they were listed under). A comment whose
/// own `issue_url` names somewhere else counts for nothing.
#[must_use]
pub fn newest(comments: &[Value], issue: &Location, ctx: &Context<'_>) -> Evaluation {
    let mut out = Evaluation::default();
    for c in comments {
        let Some(body) = c.get("body").and_then(Value::as_str) else {
            continue;
        };
        let Some(line) = marker_line(body) else {
            continue;
        };
        // The forge's own statement of where the comment sits must agree
        // with where it was listed; a listing that disagrees is not trusted
        // to place the record at all.
        let placed = c
            .get("issue_url")
            .and_then(Value::as_str)
            .is_none_or(|url| Location::from_issue_url(url).as_ref() == Some(issue));
        let verdict = match line {
            Err(r) => Err(r),
            Ok(_) if !placed => Err(Rejection::WrongIssue),
            Ok(line) => verify(line, issue, ctx),
        };
        match verdict {
            Ok(v) if out.newest.as_ref().is_none_or(|cur| wins(&v, cur)) => out.newest = Some(v),
            Ok(_) => {}
            Err(r) => *out.rejected.entry(r).or_default() += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests;
