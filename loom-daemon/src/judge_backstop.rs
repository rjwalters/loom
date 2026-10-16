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
//! A Judge run the role runner terminates at its timeout ceiling never
//! reaches its release; the daemon reaps its lease instead (#11931,
//! [`crate::pr_lease_reap`]) and writes the same record with `reaped=timeout`
//! appended, whatever the head's CI is doing — a pass that burned the whole
//! ceiling counts toward [`MAX_PER_WINDOW`] like any other.
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
//! progress that started within [`PENDING_EXEMPT_HOURS`], or a legacy commit
//! status pending that long, is not recorded (CI settling changes neither
//! head nor labels); one stuck longer is. Every page of the head's check
//! runs counts, up to [`MAX_CHECK_RUN_PAGES`] (#11903).
//!
//! A pass that stood down *because* CI was pending says so on its release
//! (`--skip-reason` [`SKIP_CI_PENDING`], #11972), and is not recorded if CI
//! finished between its check and its release: re-reading CI at release time
//! once found a green head 12 s after the pass skipped, recorded
//! `verdict=none` and held the PR from Judge for an hour. Its release still
//! reads CI, for the age bound alone: if everything still pending is past
//! [`PENDING_EXEMPT_HOURS`] or has no start stamp (stuck, not settling, the
//! CI the PR planner serves, #12003) the pass is recorded as before. A
//! release without a skip reason (an older prompt) is unchanged.
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

/// The `lease pr release --skip-reason` of a Judge pass that stood down
/// because the head's CI was still pending (#11972).
pub const SKIP_CI_PENDING: &str = "ci-pending";

/// Whether a release's own skip reason says the pass stood down for pending
/// CI ([`SKIP_CI_PENDING`]), which also exempts it once CI has concluded (but
/// never on stuck CI; module docs). No reason, or one this binary does not
/// know, keeps the default.
#[must_use]
pub fn skipped_for_pending_ci(reason: Option<&str>) -> bool {
    reason.map(str::trim) == Some(SKIP_CI_PENDING)
}

/// The page size of the check-run and commit-status reads (GitHub's max).
pub const CI_PAGE: usize = 100;

/// The most check-run pages one `checks_pending` reads (#11903). A head with
/// more runs than this is read only this far; what is left unread counts as
/// not pending, the same best-effort answer as an unreadable page.
pub const MAX_CHECK_RUN_PAGES: usize = 5;

/// The `started_at` stamps (empty when absent) of the check runs in one
/// `commits/{sha}/check-runs` page that are not `completed`.
#[must_use]
pub fn pending_check_runs(page: &Value) -> Vec<String> {
    page.get("check_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("status").and_then(Value::as_str) != Some("completed"))
        .map(|r| stamp(r, "started_at"))
        .collect()
}

/// The `created_at` stamps of the `pending` legacy statuses in a
/// `commits/{sha}/status` (combined status) document. The combined `state`
/// alone cannot be used: GitHub reports `pending` for a commit with no
/// statuses at all, and it carries no age to bound the exemption by.
#[must_use]
pub fn pending_statuses(combined: &Value) -> Vec<String> {
    combined
        .get("statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("state").and_then(Value::as_str) == Some("pending"))
        .map(|s| stamp(s, "created_at"))
        .collect()
}

fn stamp(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// How many check runs one page lists.
#[must_use]
pub fn check_runs_listed(page: &Value) -> usize {
    page.get("check_runs")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// Whether another check-run page follows `page`, given `read` runs read so
/// far: this page was full and its `total_count` says more remain.
#[must_use]
pub fn more_check_runs(page: &Value, read: usize) -> bool {
    let total = page.get("total_count").and_then(Value::as_u64).unwrap_or(0);
    check_runs_listed(page) == CI_PAGE && (read as u64) < total
}

/// A head's CI as the release reads it (#11972).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiState {
    /// Something still pending started inside the exemption window
    /// ([`pending_exempts`]).
    Young,
    /// Something is still pending, but every pending run or status is past
    /// the window or has no start stamp: CI that is stuck, not settling.
    Stuck,
    /// Nothing pending was read: CI concluded (or could not be read).
    Settled,
}

impl CiState {
    /// Whether a Judge pass that ended without a verdict at this CI is
    /// exempt from the no-verdict record. By default only young pending CI
    /// exempts. A pass that said it stood down for pending CI
    /// (`--skip-reason ci-pending`) is also exempt once CI has settled since
    /// (the race #11972 fixes), but never on stuck CI: `forge wait-checks`
    /// has no age cap, so stuck CI would exempt every pass, and the PR
    /// planner serves exactly those PRs (#12003); recording them keeps the
    /// loop bounded.
    #[must_use]
    pub fn exempts(self, skipped_for_pending_ci: bool) -> bool {
        match self {
            Self::Young => true,
            Self::Settled => skipped_for_pending_ci,
            Self::Stuck => false,
        }
    }
}

/// Which read of [`ci_state`] a `get` call is, so each can carry its own
/// forge-call-stats op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiRead {
    /// One page of `commits/{sha}/check-runs`.
    CheckRuns,
    /// The legacy `commits/{sha}/status`.
    CommitStatus,
}

/// `sha`'s [`CiState`], read through `get(kind, path)` with `path` relative
/// to `repos/{o}/{r}/`: every page of its check runs, up to
/// [`MAX_CHECK_RUN_PAGES`], then its legacy commit statuses (#11903). Stops at
/// the first read that is [`CiState::Young`], so the usual cost is one call;
/// an unreadable read contributes nothing.
pub fn ci_state(
    sha: &str,
    now: DateTime<Utc>,
    mut get: impl FnMut(CiRead, &str) -> Option<Value>,
) -> CiState {
    let base = format!("commits/{sha}/check-runs?per_page={CI_PAGE}");
    let mut stuck = false;
    let mut read = 0;
    for page in 1..=MAX_CHECK_RUN_PAGES {
        let path = if page == 1 {
            base.clone()
        } else {
            format!("{base}&page={page}")
        };
        let Some(v) = get(CiRead::CheckRuns, &path) else {
            break;
        };
        let pending = pending_check_runs(&v);
        if pending_exempts(&pending, now) {
            return CiState::Young;
        }
        stuck |= !pending.is_empty();
        read += check_runs_listed(&v);
        if !more_check_runs(&v, read) {
            break;
        }
    }
    let status = format!("commits/{sha}/status?per_page={CI_PAGE}");
    if let Some(v) = get(CiRead::CommitStatus, &status) {
        let pending = pending_statuses(&v);
        if pending_exempts(&pending, now) {
            return CiState::Young;
        }
        stuck |= !pending.is_empty();
    }
    if stuck {
        CiState::Stuck
    } else {
        CiState::Settled
    }
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

/// The fields the daemon's reap of a terminated Judge appends (#11931): the
/// no-verdict record plus `reaped=<reason>`. `reason` is reduced to
/// `[a-z0-9]` so it can never break the marker.
#[must_use]
pub fn reaped_fields<S: AsRef<str>>(head: &str, labels: &[S], reason: &str) -> String {
    let reason: String = reason
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    let reason = if reason.is_empty() {
        "unknown"
    } else {
        &reason
    };
    format!("{} reaped={reason}", released_fields(head, labels))
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
