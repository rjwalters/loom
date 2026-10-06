//! The chain-head merge lock (#10167, split from #10163).
//!
//! # The livelock this breaks
//!
//! A sequenced chain head blocked by the #8248 freshness guard is re-dated
//! (#8508/#9590): a tree-identical push that re-runs every required check
//! against the current base. On a busy `main`, unrelated PRs keep merging
//! while those checks run, so the head's fresh evidence is stale again the
//! moment it lands, and the next attempt re-dates again. #10163 PR 1 proved
//! the residual moves were real coupled-input changes, not restamps, so no
//! discount can fix it: something has to stop `main` moving for the few
//! minutes the head's checks need.
//!
//! # The lock
//!
//! When `merge-pr redate-checks` pushes a re-date (a fresh-verdict re-date,
//! [`crate::merge_pr::redate::RemedyOutcome::Pushed`]), it posts one comment
//! on the chain-head PR carrying:
//!
//! ```text
//! <!-- loom:chain-head-lock base=<ref> head=<40-hex> acquired=<rfc3339> cap=<secs> -->
//! ```
//!
//! Durable forge state, like the `loom:sequence` marker and the #9590 attempt
//! marker: no process owns it, so a restart cannot lose it, and every host
//! reads the same answer. Every other PR's `merge-pr.sh --auto` asks
//! `merge-pr chain-lock` before writing anything, and defers (exit 6) while a
//! lock on the same base is live.
//!
//! # Liveness (all must hold, see [`evaluate`])
//!
//! - the marker is the newest valid one in a TRUSTED author's comment (#9548:
//!   an outsider's marker is prose and never holds a merge);
//! - the holder PR is still open, still targets the marker's base, and is not
//!   escalated to `loom:operator` (budget exhaustion releases the lock);
//! - the holder's head is still the marker's `head` (any push voids it);
//! - `now` is before the comment's forge-assigned `created_at` plus the cap.
//!   The embedded `acquired=` is informational only: a forged or skewed
//!   timestamp cannot extend a lock, and the effective cap is the smaller of
//!   the marker's and the reader's, both clamped to [`MAX_CAP_SECS`];
//! - the holder's required checks have not all reported.
//!
//! # Bounded by construction
//!
//! The lock can never wedge `main`: the cap ends it, whatever else happens.
//! It does not touch the #9590 budget — it is a separate marker that
//! `budget::chain_position` never matches, so acquiring it neither spends nor
//! refunds a re-date. Two heads that both hold a lock are ordered oldest
//! first, so they cannot hold each other.
//!
//! # Unreadable state
//!
//! If the lock state cannot be read (API error, quota), the guard does not
//! guess "no lock" — that would merge straight through the window the lock
//! exists for. It defers, and records the first unreadable read locally
//! (`.loom/state/chain-lock/`, ignored); once a whole cap has passed since
//! then, no lock acquired before the outage can still be live, so it fails
//! open. A successful read clears the record.

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::claim_reconciliation::gh_call;
use crate::comment_trust::TrustPolicy;
use crate::merge_pr::sequence::{html_comment_spans, is_full_sha};
use crate::merge_pr::stale_checks::CheckRun;

/// The marker namespace.
pub const MARKER_PREFIX: &str = "loom:chain-head-lock";
/// Default cap: long enough for this repo's slowest required suite (~15 min).
pub const DEFAULT_CAP_SECS: u64 = 1200;
/// Upper clamp on a configured cap. The bound is the point of the mechanism,
/// so a typo like `120000` must not quietly turn it into a `main` freeze.
pub const MAX_CAP_SECS: u64 = 3600;
/// Env override for the cap (beats config).
pub const CAP_ENV: &str = "LOOM_CHAIN_LOCK_CAP_SECS";
/// Config key for the cap.
pub const CAP_CONFIG_KEY: &str = "champion.chainLockCapSecs";
/// Env opt-out: an operator asserting "merge anyway".
pub const OVERRIDE_ENV: &str = "LOOM_CHAIN_LOCK_OVERRIDE";
/// The `merge-pr.sh` exit code (and this verb's) for a held merge.
pub const DEFER_EXIT: i32 = 6;

/// No live lock holds this merge.
pub const CLEAR: &str = "LOOM-CHAIN-LOCK-CLEAR";
/// A live lock on another PR holds this merge (exit 6).
pub const HELD: &str = "LOOM-CHAIN-LOCK-HELD";
/// The lock state could not be read inside the cap window (exit 6).
pub const UNREADABLE: &str = "LOOM-CHAIN-LOCK-UNREADABLE";
/// The lock state has been unreadable for a whole cap: proceed (exit 0).
pub const FAIL_OPEN: &str = "LOOM-CHAIN-LOCK-FAIL-OPEN";
/// [`OVERRIDE_ENV`] is set: the guard was skipped (exit 0).
pub const OVERRIDDEN: &str = "LOOM-CHAIN-LOCK-OVERRIDDEN";

/// The escalation label that releases a lock (the #8508/#9590 hold).
const ESCALATED_LABEL: &str = "loom:operator";

fn valid_cap(n: u64) -> Option<u64> {
    (n >= 1).then(|| n.min(MAX_CAP_SECS))
}

/// env > config > default, the same tiering as `LOOM_REDATE_BUDGET`: an
/// unparseable or zero value at any tier falls through to the next, and a
/// huge one is clamped to [`MAX_CAP_SECS`].
#[must_use]
pub fn resolve_cap(env: Option<&str>, config: &Value) -> u64 {
    env.and_then(|s| s.trim().parse::<u64>().ok())
        .and_then(valid_cap)
        .or_else(|| {
            crate::config_resolver::get_path(config, CAP_CONFIG_KEY)
                .and_then(Value::as_u64)
                .and_then(valid_cap)
        })
        .unwrap_or(DEFAULT_CAP_SECS)
}

/// The cap for the workspace at `root`, from the process env and its config.
#[must_use]
pub fn cap_for_root(root: &Path) -> u64 {
    let effective = crate::config_resolver::resolve_effective_config(root);
    resolve_cap(std::env::var(CAP_ENV).ok().as_deref(), &effective)
}

/// Is the operator override set? Falsey values (`0`/`false`/`no`/`off`/
/// empty) are off, anything else on — the parse clap's `FalseyValueParser`
/// applies to `LOOM_REDATE_ALLOW_PROCEED`.
#[must_use]
pub fn override_set(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no" | "off" | "n")
    })
}

/// A parsed, validated lock marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockMarker {
    pub base: String,
    pub head: String,
    pub acquired: DateTime<Utc>,
    pub cap_secs: u64,
}

/// The canonical marker line. Single producer, single format.
#[must_use]
pub fn marker_text(m: &LockMarker) -> String {
    format!(
        "<!-- {MARKER_PREFIX} base={} head={} acquired={} cap={} -->",
        m.base,
        m.head,
        m.acquired.to_rfc3339_opts(SecondsFormat::Secs, true),
        m.cap_secs
    )
}

/// The comment the re-date path posts: the marker plus one line of prose.
#[must_use]
pub fn lock_comment_body(m: &LockMarker) -> String {
    let short = &m.head[..m.head.len().min(7)];
    format!(
        "{}\n**Chain-head merge lock (#10167)**: this PR was just re-dated at `{short}`. Other \
PRs targeting `{}` defer their merge (`merge-pr.sh` exit 6) until its required checks report, its \
head moves, it lands or closes, or {} s pass, whichever is first.",
        marker_text(m),
        m.base,
        m.cap_secs
    )
}

/// A base ref token: non-empty, bounded. Whitespace and `-->` are already
/// excluded by the tokenizer.
fn is_ref(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255
}

fn parse_span(span: &str) -> Option<LockMarker> {
    let rest = span.trim().strip_prefix(MARKER_PREFIX)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (mut base, mut head, mut acquired, mut cap) = (None, None, None, None);
    for field in rest.split_whitespace() {
        let (key, value) = field.split_once('=')?;
        match key {
            "base" => base = is_ref(value).then(|| value.to_string()),
            "head" => head = is_full_sha(value).then(|| value.to_string()),
            "acquired" => {
                acquired = DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|d| d.with_timezone(&Utc));
            }
            "cap" => cap = value.parse::<u64>().ok().and_then(valid_cap),
            _ => return None,
        }
    }
    Some(LockMarker {
        base: base?,
        head: head?,
        acquired: acquired?,
        cap_secs: cap?,
    })
}

/// A marker together with the forge-assigned time its comment was posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedLock {
    pub marker: LockMarker,
    pub created_at: DateTime<Utc>,
}

/// The newest valid lock marker in a TRUSTED REST comment listing
/// (oldest-first, as the forge returns it). A comment with no parseable
/// `created_at` is skipped: without it the lock's age is unknowable, and the
/// embedded `acquired=` is never trusted to stand in for it.
#[must_use]
pub fn newest_lock(trusted: &[Value]) -> Option<ObservedLock> {
    let mut newest = None;
    for c in trusted {
        let Some(body) = c.get("body").and_then(Value::as_str) else {
            continue;
        };
        let Some(created_at) = c
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
        else {
            continue;
        };
        for line in body.lines() {
            for span in html_comment_spans(line) {
                if let Some(marker) = parse_span(span) {
                    newest = Some(ObservedLock { marker, created_at });
                }
            }
        }
    }
    newest
}

/// The live state of a PR that may hold a lock, from the pulls API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub number: u32,
    pub open: bool,
    pub head_sha: String,
    pub base_ref: String,
    pub labels: Vec<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Why a lock no longer holds anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// The cap elapsed since the lock comment was posted.
    Cap,
    /// The holder's head moved (a push voids the lock).
    HeadMoved,
    /// Every required check on the holder's head has reported.
    ChecksReported,
    /// The holder landed or closed.
    Closed,
    /// The holder no longer targets the marker's base.
    OtherBase,
    /// The holder was escalated to `loom:operator` (budget exhausted).
    Escalated,
}

/// A lock's state at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live { expires_at: DateTime<Utc> },
    Expired(Expiry),
}

/// When `lock` expires by the cap alone: `created_at` plus the smaller of
/// the marker's cap and the reader's, so neither side can stretch it.
#[must_use]
pub fn cap_expiry(lock: &ObservedLock, reader_cap: u64) -> DateTime<Utc> {
    let secs = lock.marker.cap_secs.min(reader_cap).min(MAX_CAP_SECS);
    let secs = i64::try_from(secs).unwrap_or(0);
    lock.created_at + TimeDelta::seconds(secs)
}

/// Evaluate one lock. The cheap, already-fetched conditions are checked
/// first; `checks_reported` (a forge read) runs only for a lock that would
/// otherwise be live, and its error is the caller's "unreadable".
pub fn evaluate(
    lock: &ObservedLock,
    holder: &Holder,
    reader_cap: u64,
    now: DateTime<Utc>,
    checks_reported: impl FnOnce() -> Result<bool, String>,
) -> Result<Liveness, String> {
    if !holder.open {
        return Ok(Liveness::Expired(Expiry::Closed));
    }
    if holder.base_ref != lock.marker.base {
        return Ok(Liveness::Expired(Expiry::OtherBase));
    }
    if holder.head_sha != lock.marker.head {
        return Ok(Liveness::Expired(Expiry::HeadMoved));
    }
    if holder.labels.iter().any(|l| l == ESCALATED_LABEL) {
        return Ok(Liveness::Expired(Expiry::Escalated));
    }
    let expires_at = cap_expiry(lock, reader_cap);
    if now >= expires_at {
        return Ok(Liveness::Expired(Expiry::Cap));
    }
    if checks_reported()? {
        return Ok(Liveness::Expired(Expiry::ChecksReported));
    }
    Ok(Liveness::Live { expires_at })
}

/// Have the head's required checks all reported (completed, any conclusion)?
///
/// Each required context is judged by its latest run, as branch protection
/// does. A context with no run has not reported. With no required contexts
/// at all, every check run on the head stands in, and a head with no runs
/// yet has not reported. A required context that is a commit status rather
/// than a check run never reports here; the cap still ends that lock.
#[must_use]
pub fn checks_reported(required: &[String], runs: &[CheckRun]) -> bool {
    let done = |r: &CheckRun| r.status == "completed";
    if required.is_empty() {
        return !runs.is_empty() && runs.iter().all(done);
    }
    required.iter().all(|ctx| {
        runs.iter()
            .filter(|r| &r.name == ctx)
            .max_by_key(|r| r.started_at)
            .is_some_and(done)
    })
}

/// One live lock found on the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLock {
    pub holder: u32,
    pub head: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The guard's answer for one merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    Clear,
    Held(LiveLock),
}

/// Decide whether `pr` is held by any of `live` (all on the same base).
///
/// A PR is never held by its own lock. When `pr` holds a live lock itself,
/// only OLDER locks (by `created_at`, then PR number) hold it, so two heads
/// that both re-dated cannot hold each other until their caps run out. The
/// oldest holder is named (it is the one that will land first).
#[must_use]
pub fn decide(pr: u32, live: &[LiveLock]) -> Guard {
    let key = |l: &LiveLock| (l.created_at, l.holder);
    let own = live.iter().find(|l| l.holder == pr).map(key);
    live.iter()
        .filter(|l| l.holder != pr)
        .filter(|l| own.is_none_or(|o| key(l) < o))
        .min_by_key(|l| key(l))
        .map_or(Guard::Clear, |l| Guard::Held(l.clone()))
}

/// What to do when the lock state could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// Still inside a cap of the first unreadable read: defer.
    Defer { until: DateTime<Utc> },
    /// A whole cap has passed since the first unreadable read: any lock that
    /// could have been live then has expired, so proceed.
    FailOpen { since: DateTime<Utc> },
}

/// Pure unreadable-state decision.
#[must_use]
pub fn decide_unreadable(first: DateTime<Utc>, cap: u64, now: DateTime<Utc>) -> Unreadable {
    let until = first + TimeDelta::seconds(i64::try_from(cap.min(MAX_CAP_SECS)).unwrap_or(0));
    if now < until {
        Unreadable::Defer { until }
    } else {
        Unreadable::FailOpen { since: first }
    }
}

/// The local record of the first unreadable read for `base`.
#[must_use]
pub fn unreadable_state_path(root: &Path, base: &str) -> PathBuf {
    let safe: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join(".loom/state/chain-lock")
        .join(format!("unreadable-{safe}.json"))
}

/// The first unreadable read on record at `path`, recording `now` when there
/// is none. `None` when the record cannot be written: with nothing durable to
/// measure the cap from, the caller fails open rather than defer forever.
pub fn note_unreadable(path: &Path, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let existing = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("first_unreadable")?.as_str().map(str::to_string))
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
        .filter(|d| *d <= now);
    if existing.is_some() {
        return existing;
    }
    let body = serde_json::json!({ "first_unreadable": now.to_rfc3339() }).to_string();
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(path, body).ok()?;
    Some(now)
}

/// Forget the unreadable record after a successful read.
pub fn clear_unreadable(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// --- Forge I/O ------------------------------------------------------------
//
// Reads go through the counted `gh` facade with the binary as a parameter
// (the same injection seam `sequence` uses), so tests pass a stub path.

/// Percent-encode a query value (branch names may carry `/`, kept as-is).
fn encode_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Every element of a `--paginate`d listing (concatenated JSON arrays).
fn concat_arrays(stdout: &[u8]) -> Option<Vec<Value>> {
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_slice(stdout).into_iter::<Value>() {
        match page.ok()? {
            Value::Array(a) => items.extend(a),
            _ => return None,
        }
    }
    Some(items)
}

/// Project one pulls-API object; `None` when a field the decision needs is
/// missing (a guessed head or base must never compare equal to a marker).
#[must_use]
pub fn holder_from_json(v: &Value) -> Option<Holder> {
    Some(Holder {
        number: u32::try_from(v.get("number")?.as_u64()?).ok()?,
        open: v.get("state")?.as_str()? == "open",
        head_sha: v.pointer("/head/sha")?.as_str()?.to_string(),
        base_ref: v.pointer("/base/ref")?.as_str()?.to_string(),
        labels: v
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        updated_at: v
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc)),
    })
}

fn gh_read(op: &'static str, gh: &str, root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    gh_call::ok_stdout(gh_call::read(op, Path::new(gh), root).args(args))
        .ok_or_else(|| format!("the {op} read failed"))
}

/// Everything the guard reads, for one merge of `pr` onto `base`.
pub struct GuardInputs<'a> {
    pub gh: &'a str,
    pub root: &'a Path,
    pub nwo: &'a str,
    pub pr: u32,
    pub base: &'a str,
    pub cap_secs: u64,
    pub now: DateTime<Utc>,
    pub policy: &'a TrustPolicy,
}

/// Read the lock state for `pr`'s base and decide. `Err` is "unreadable".
///
/// Only open PRs on the same base updated inside the cap window are read
/// further (posting the lock comment bumps `updated_at`, so an older PR
/// cannot carry a live lock), and only their comments since the window
/// opened. Checks are read only for a lock that is otherwise live.
pub fn read_guard(i: &GuardInputs<'_>) -> Result<Guard, String> {
    let window = i.now - TimeDelta::seconds(i64::try_from(i.cap_secs).unwrap_or(0));
    let pulls_path =
        format!("repos/{}/pulls?state=open&base={}&per_page=100", i.nwo, encode_query(i.base));
    let listing =
        gh_read("chain_lock.open_prs", i.gh, i.root, &["api", &pulls_path, "--paginate"])?;
    let pulls = concat_arrays(&listing).ok_or("the open-PR listing did not parse")?;
    let since = window.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut required: Option<Vec<String>> = None;
    let mut live = Vec::new();
    for holder in pulls.iter().filter_map(holder_from_json) {
        if holder.base_ref != i.base || holder.updated_at.is_some_and(|u| u < window) {
            continue;
        }
        let path = format!(
            "repos/{}/issues/{}/comments?since={}&per_page=100",
            i.nwo,
            holder.number,
            encode_query(&since)
        );
        let raw = gh_read("chain_lock.comments", i.gh, i.root, &["api", &path, "--paginate"])?;
        let trusted = i
            .policy
            .trusted_listing(&raw)
            .ok_or_else(|| format!("PR #{}'s comment listing did not parse", holder.number))?;
        let Some(lock) = newest_lock(&trusted) else {
            continue;
        };
        let liveness = evaluate(&lock, &holder, i.cap_secs, i.now, || {
            if required.is_none() {
                let (ctx, _notices) = crate::merge_pr::stale_checks::fetch::required_contexts_with(
                    i.gh, i.nwo, i.base,
                )?;
                required = Some(ctx);
            }
            let runs = crate::merge_pr::stale_checks::fetch::fetch_check_runs(
                i.gh,
                i.nwo,
                &holder.head_sha,
            )?;
            Ok(checks_reported(required.as_deref().unwrap_or_default(), &runs))
        })?;
        if let Liveness::Live { expires_at } = liveness {
            live.push(LiveLock {
                holder: holder.number,
                head: holder.head_sha.clone(),
                created_at: lock.created_at,
                expires_at,
            });
        }
    }
    Ok(decide(i.pr, &live))
}

/// Post the lock comment for a just-pushed re-date of `pr` at `head`. The
/// base is read from the PR itself, so the caller passes nothing new.
pub fn record_lock(
    gh: &str,
    root: &Path,
    nwo: &str,
    pr: &str,
    head: &str,
    cap_secs: u64,
    now: DateTime<Utc>,
) -> Result<LockMarker, String> {
    let path = format!("repos/{nwo}/pulls/{pr}");
    let raw = gh_read("chain_lock.pr_base", gh, root, &["api", &path])?;
    let base = serde_json::from_slice::<Value>(&raw)
        .ok()
        .and_then(|v| v.pointer("/base/ref")?.as_str().map(str::to_string))
        .filter(|b| is_ref(b) && !b.contains(char::is_whitespace))
        .ok_or_else(|| format!("PR #{pr}'s base ref did not parse"))?;
    if !is_full_sha(head) {
        return Err(format!("re-date head {head} is not a full SHA"));
    }
    let marker = LockMarker {
        base,
        head: head.to_string(),
        acquired: now,
        cap_secs: cap_secs.clamp(1, MAX_CAP_SECS),
    };
    crate::forge_comment::post_comment(gh, Some(root), nwo, pr, true, &lock_comment_body(&marker))?;
    Ok(marker)
}

#[cfg(test)]
mod tests;
