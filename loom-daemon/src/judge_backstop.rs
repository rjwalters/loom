//! The Judge no-verdict backstop (#11855).
//!
//! ## The loop this bounds
//!
//! A Judge pass that ends without a verdict leaves its PR exactly where it
//! found it: `loom:review-requested`, same head, same labels. Nothing in the
//! PR's state records that a Judge already tried, so `pr-queue --role judge`
//! serves it again on the next tick, on every host, and the next Judge hits
//! the same wall. When the wall never moves (a verdict gate that cannot read
//! what it needs, say) the PR is re-judged without end: one incident logged
//! hundreds of Judge runs in a day on a handful of PRs across three hosts,
//! and thousands of runs fleet-wide for a few hundred verdicts.
//!
//! ## The record
//!
//! Every Judge pass that claimed the PR ends with `lease pr release`, which
//! already rewrites its lease comment's first line to the
//! `loom:pr-lease-released` marker. When the release finds the PR still open,
//! still `loom:review-requested`, and carrying neither verdict label, it
//! appends what it saw:
//!
//! ```text
//! <!-- loom:pr-lease-released role=judge host=<h> sweep=<s> head=<sha> verdict=none labels=<digest> -->
//! ```
//!
//! The comment's own forge `updated_at` is when. No extra comment is posted,
//! and older readers of the marker read only its first three tokens, so they
//! are unaffected. The record lives on the PR, so every host sees it.
//!
//! ## The hold
//!
//! A PR is held from Judge while its no-verdict records **at its current head
//! and label digest** inside the trailing [`WINDOW_HOURS`] say so
//! ([`hold_until`]): one hour after the first, four after the second, and
//! after the third until the oldest of the last three ages out of the window.
//! An unchanged PR therefore gets at most [`MAX_PER_WINDOW`] Judge runs per
//! window. A push (new head), or a change to the PR's `loom:*` label *set*
//! (new digest), releases the hold at once, because the old records no longer
//! match. Removing and re-adding the same label leaves the set — and the hold —
//! unchanged: to force a re-review of an unchanged PR, push, or run the Judge
//! with [`DISABLE_ENV`]`=0`. A pass whose head still has a check run in
//! progress that started within [`PENDING_EXEMPT_HOURS`] is not recorded (CI
//! settling changes neither head nor labels); a run stuck longer is.
//!
//! Only trusted authors' records count (the caller filters): a record
//! suppresses review, so an outsider must not be able to post one.
//!
//! The backstop is an efficiency guard, not a safety gate: every failure to
//! read the records fails open (the PR is served), and
//! [`DISABLE_ENV`]`=0` turns it off.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

/// First-line prefix of a released Judge lease record (the
/// `loom:pr-lease-released` family, `cli/pr_lease.rs`).
pub const RELEASED_JUDGE_PREFIX: &str = "<!-- loom:pr-lease-released role=judge ";

/// `0` / `false` / `no` / `off` disables the backstop (hold and refusal);
/// anything else, or unset, leaves it on.
pub const DISABLE_ENV: &str = "LOOM_JUDGE_BACKSTOP";

/// The trailing window records are counted in.
pub const WINDOW_HOURS: i64 = 24;

/// The most Judge runs an unchanged PR gets per [`WINDOW_HOURS`].
pub const MAX_PER_WINDOW: usize = 3;

/// The hold after the first and the second no-verdict record, in hours.
const COOLDOWN_HOURS: [i64; MAX_PER_WINDOW - 1] = [1, 4];

/// How long a pending check run exempts a no-verdict pass from the record
/// (`lease pr release`): CI that is merely running should not hold the PR,
/// but a run stuck queued forever (no runner) must not exempt every pass,
/// or the loop this module bounds comes back.
pub const PENDING_EXEMPT_HOURS: i64 = 6;

/// Whether pending check runs, given their `started_at` stamps (empty when
/// GitHub sent none), still exempt a pass: at least one started inside
/// [`PENDING_EXEMPT_HOURS`]. A stamp that is missing or unparseable counts
/// as stale, so the exemption stays bounded.
#[must_use]
pub fn pending_exempts(started: &[String], now: DateTime<Utc>) -> bool {
    started.iter().any(|s| {
        DateTime::parse_from_rfc3339(s.trim())
            .is_ok_and(|t| now - t.with_timezone(&Utc) < Duration::hours(PENDING_EXEMPT_HOURS))
    })
}

/// The label whose presence means "still waiting for a Judge".
pub const REVIEW_LABEL: &str = "loom:review-requested";

/// Verdict labels: either present means the pass produced a verdict.
const VERDICT_LABELS: [&str; 2] = ["loom:pr", "loom:changes-requested"];

/// Live-claim labels, left out of the digest: they come and go within a pass.
const CLAIM_LABELS: [&str; 2] = ["loom:reviewing", "loom:treating"];

/// Whether the backstop is on, given [`DISABLE_ENV`]'s value.
#[must_use]
pub fn enabled_from(value: Option<&str>) -> bool {
    !value.is_some_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
    })
}

/// Whether the backstop is on in this process.
#[must_use]
pub fn enabled() -> bool {
    enabled_from(std::env::var(DISABLE_ENV).ok().as_deref())
}

/// The PR's `loom:*` labels, claim labels left out, sorted, `loom:` dropped,
/// comma-joined (`-` when there are none). Space-free by construction.
#[must_use]
pub fn label_digest<S: AsRef<str>>(labels: &[S]) -> String {
    let mut names: Vec<&str> = labels
        .iter()
        .map(AsRef::as_ref)
        .filter(|l| l.starts_with("loom:") && !CLAIM_LABELS.contains(l))
        .filter(|l| !l.contains(char::is_whitespace))
        .map(|l| l.trim_start_matches("loom:"))
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        "-".into()
    } else {
        names.join(",")
    }
}

/// Whether a Judge pass releasing now left the PR without a verdict: open,
/// still asking for review, and carrying neither verdict label.
#[must_use]
pub fn ended_without_verdict<S: AsRef<str>>(open: bool, labels: &[S]) -> bool {
    let has = |name: &str| labels.iter().any(|l| l.as_ref() == name);
    open && has(REVIEW_LABEL) && !VERDICT_LABELS.iter().any(|v| has(v))
}

/// The fields a no-verdict release appends to its marker (leading space).
#[must_use]
pub fn released_fields<S: AsRef<str>>(head: &str, labels: &[S]) -> String {
    format!(" head={head} verdict=none labels={}", label_digest(labels))
}

/// One no-verdict record: the head and label digest the pass left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoVerdict {
    pub head: String,
    pub labels: String,
}

/// Parse a comment body's first line as a no-verdict Judge release record.
#[must_use]
pub fn parse(body: &str) -> Option<NoVerdict> {
    let rest = body
        .lines()
        .next()?
        .strip_prefix(RELEASED_JUDGE_PREFIX)?
        .trim_end()
        .strip_suffix("-->")?;
    let field = |key: &str| {
        rest.split_whitespace()
            .find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
            .filter(|v| !v.is_empty())
    };
    if field("verdict")? != "none" {
        return None;
    }
    let head = field("head")?;
    if !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(NoVerdict {
        head: head.to_ascii_lowercase(),
        labels: field("labels")?.to_string(),
    })
}

/// When the hold on a PR at `head` with label digest `digest` ends, given its
/// `(when, record)` pairs; `None` when it is not held at `now`.
#[must_use]
pub fn hold_until(
    records: &[(DateTime<Utc>, NoVerdict)],
    head: &str,
    digest: &str,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let window = Duration::hours(WINDOW_HOURS);
    let head = head.to_ascii_lowercase();
    let mut at: Vec<DateTime<Utc>> = records
        .iter()
        .filter(|(when, r)| r.head == head && r.labels == digest && now - *when < window)
        .map(|(when, _)| *when)
        .collect();
    at.sort_unstable_by(|a, b| b.cmp(a));
    let until = match at.len() {
        0 => return None,
        n if n >= MAX_PER_WINDOW => at[MAX_PER_WINDOW - 1] + window,
        n => at[0] + Duration::hours(COOLDOWN_HOURS[n - 1]),
    };
    (until > now).then_some(until)
}

/// [`hold_until`] over trusted REST comment objects (`body`, `updated_at`).
#[must_use]
pub fn hold_from_comments(
    comments: &[Value],
    head: &str,
    labels: &[&str],
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let records: Vec<(DateTime<Utc>, NoVerdict)> = comments
        .iter()
        .filter_map(|c| {
            let record = parse(c.get("body")?.as_str()?)?;
            let when = DateTime::parse_from_rfc3339(c.get("updated_at")?.as_str()?).ok()?;
            Some((when.with_timezone(&Utc), record))
        })
        .collect();
    hold_until(&records, head, &label_digest(labels), now)
}

#[cfg(test)]
#[path = "judge_backstop/tests.rs"]
mod tests;
