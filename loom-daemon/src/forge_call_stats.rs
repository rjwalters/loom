//! Per-caller forge call accounting (Issue #9251, ADR-0021 amendment step 0).
//!
//! Before this, the only evidence of whether the ETag caches earn their free
//! `304`s was a `debug!` line on a hit; nothing counted the `200`s. This
//! module counts every instrumented forge call by **caller**, **pool** and
//! **outcome** so `loom-daemon status` can answer "is the cache not earning
//! its 304s, or do we just make too many calls?".
//!
//! # Two stores, because most callers are not the daemon
//!
//! An in-daemon counter alone would read zero for `loom-daemon serve`, the
//! short-lived `status`/`health` CLIs and agent `forge … --cached` processes.
//! So each [`record`] does two things:
//!
//! 1. bumps this process's since-start totals (bounded: one row per
//!    caller × pool), and
//! 2. appends ONE short JSON line to a **per-host append-only sink**
//!    (`${TMPDIR:-/tmp}/loom-forge-call-stats/calls-<epoch-hour>.jsonl`;
//!    `LOOM_FORGE_CALL_STATS_DIR` overrides, `off`/`0` disables). Each line is
//!    a single `O_APPEND` write far under `PIPE_BUF`, so concurrent writers
//!    never interleave. Files rotate hourly and anything older than
//!    [`RETAIN_HOURS`] is pruned when a new hour's file is created.
//!
//! `status` aggregates the sink's last [`WINDOW_SECS`] into the host-wide
//! window. Both stores are local file/memory writes: the accounting itself
//! makes **no forge call**, and a failure in it never fails the forge call.
//!
//! # Pool and the free budget reading
//!
//! GitHub sends `x-ratelimit-resource` / `-remaining` / `-reset` on every REST
//! response, `304`s included, and `gh api --include` exposes them. The pool is
//! taken from `x-ratelimit-resource` (defaulting to `core` for REST), and the
//! remaining/reset pair is kept as the latest **free** budget reading — unlike
//! [`crate::rate_limit_breaker`]'s budget, which is only probed after a trip.
//! A GraphQL caller (`gh issue/pr list` without `--cached`) prints no headers
//! and records its pool statically as [`Pool::Graphql`].
//!
//! # Call identity (Issue #9777)
//!
//! `caller` alone cannot answer "which inventoried *operation* did we spend
//! that budget on, against which forge, in which repository?" — and on a mixed
//! fleet it cannot even tell two forges apart. So each record may carry a
//! [`CallIdentity`]: the operation ID from
//! [`crate::forge_inventory`], the provider, the **origin host**, and the
//! `owner/repo` slug. The origin is what makes `github.com/acme/app#12` and
//! `gitea.example.com/acme/app#12` two different things rather than one — see
//! [`CallIdentity::qualified_key`].
//!
//! Three rules hold that layer to the epic's evidence constraints:
//!
//! 1. **Bounded and sanitized.** Every field goes through [`sanitize`]: one
//!    line, ASCII-printable, length-capped. Nothing a forge or an operator can
//!    make arbitrarily long reaches the sink.
//! 2. **No credentials, no private bodies.** The identity carries an operation
//!    ID, a provider, a host and a repository slug. Authentication headers,
//!    tokens and response bodies are never arguments to [`record`], so they
//!    cannot be recorded by mistake; [`sanitize`] additionally refuses a value
//!    carrying a credential shape.
//! 3. **Unknown operations stay visible.** A call whose operation the manifest
//!    does not know records `operation = "unknown"` rather than nothing, so an
//!    unmapped caller shows up in the accounting instead of disappearing from
//!    it. Runtime traces *supplement* the source inventory; they never
//!    establish exhaustiveness on their own.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::forge_listing::HttpResponse;

#[path = "forge_call_stats_counters.rs"]
pub mod counters;
#[path = "forge_call_stats_ops.rs"]
pub mod ops;
use crate::types::{ForgeBudgetReading, ForgeCallCounts, ForgeCallsStatus, ForgeOperationCounts};
pub use ops::ForgeOp;

/// The rolling window `status` reports (the last hour).
pub const WINDOW_SECS: i64 = 3600;
/// Sink files older than this many hours are pruned.
const RETAIN_HOURS: i64 = 3;

/// How one forge call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A `2xx` answer — a full, budget-costing response.
    Ok,
    /// `304 Not Modified` — a free ETag hit.
    NotModified,
    /// Rate-limited (classified like the breaker does).
    RateLimited,
    /// Any other failure.
    Error,
}

/// The rate-limit pool a call spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pool {
    Core,
    Graphql,
    Search,
    Other,
}

impl Pool {
    /// Classify an `x-ratelimit-resource` value.
    #[must_use]
    pub fn from_resource(resource: &str) -> Self {
        match resource.trim().to_ascii_lowercase().as_str() {
            "core" => Pool::Core,
            "graphql" => Pool::Graphql,
            "search" => Pool::Search,
            _ => Pool::Other,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Pool::Core => "core",
            Pool::Graphql => "graphql",
            Pool::Search => "search",
            Pool::Other => "other",
        }
    }
}

/// What is recorded for an operation the inventory does not know. A visible
/// `unknown` is the point: a silently-dropped row is an invisible bypass.
pub const UNKNOWN_OPERATION: &str = "unknown";

/// Longest value any identity field may contribute to a sink line.
const MAX_FIELD_LEN: usize = 96;

/// Reduce an identity field to a bounded, single-line, printable-ASCII token,
/// or `None` when nothing safe is left.
///
/// Rejected outright (not truncated): anything carrying a credential shape.
/// None of the call sites pass a secret, and that is the real defence — this
/// is the belt to that braces, so a future caller that wires a header or a URL
/// with embedded basic-auth into an identity field records nothing rather than
/// a secret.
/// The shape check runs **before** any normalization, on every form the
/// normalization can produce — see [`is_credential_in_any_form`].
#[must_use]
pub fn sanitize(value: &str) -> Option<String> {
    if is_credential_in_any_form(value) {
        return None;
    }
    let one_line: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = one_line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let kept: String = trimmed
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(MAX_FIELD_LEN)
        .collect();
    let kept = kept.trim();
    if kept.is_empty() {
        None
    } else {
        Some(kept.to_string())
    }
}

/// [`looks_like_credential`], evaluated on the raw value **and** on each form
/// [`sanitize`]'s own normalization can turn it into.
///
/// A check that only ran after normalization was a real bypass (PR #9832
/// review): the normalization moves a value across the check in both
/// directions.
///
/// - Folding a control character to a space **splits** a marker:
///   `ghp\u{0}_…` becomes `ghp _…`, which contains no `ghp_`. The
///   *graphic-only* form rejoins the halves.
/// - Dropping a non-graphic character **joins** one: `ghp\u{e9}_…` becomes
///   `ghp_…`. The raw form contains neither, so the *folded* form is what sees
///   it.
/// - A marker that itself contains a space (`bearer `, `private key`) needs the
///   whitespace a graphic-only form deletes, which is why the raw and folded
///   forms are checked too.
///
/// The length cap is deliberately not applied here: checking the untruncated
/// forms is strictly stronger than checking what gets recorded.
fn is_credential_in_any_form(value: &str) -> bool {
    if looks_like_credential(value) {
        return true;
    }
    let folded: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    if looks_like_credential(&folded) {
        return true;
    }
    let graphic_only: String = value.chars().filter(char::is_ascii_graphic).collect();
    looks_like_credential(&graphic_only)
}

/// Credential shapes an identity field must never carry. Deliberately coarse:
/// a false positive costs one unrecorded label, a false negative writes a
/// secret to a file on disk.
fn looks_like_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "authorization",
        "bearer ",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "sk-ant-",
        "xoxb-",
        "xoxp-",
        "aws_secret",
        "akia",
        "private key",
        "begin rsa",
        "begin openssh",
        "token=",
        "access_token",
        "//:@",
    ];
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // `user:secret@host` in a URL-ish value.
    value.contains("://") && value.split("://").nth(1).is_some_and(|r| r.contains('@'))
}

/// Who and what a forge call was for, beyond its `caller`.
///
/// Every field is optional because the layer lands incrementally: an
/// un-migrated call site records what it knows and nothing more, which is
/// strictly better accounting than the `caller`-only row it had before. A
/// field that cannot be sanitized is dropped, never recorded raw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallIdentity {
    /// Inventoried operation ID (`issue.create`), or [`UNKNOWN_OPERATION`].
    pub operation: Option<String>,
    /// Provider family: `github`, `gitea`, …
    pub provider: Option<String>,
    /// Origin host (`github.com`, `gitea.example.com`) — what distinguishes
    /// two forges that use the same repository slug.
    pub origin: Option<String>,
    /// `owner/repo` slug.
    pub repo: Option<String>,
    /// Which identity served the call (#9872): `reader`, `writer` or
    /// `writer-fallback` — the rate-limit pool it spent. Set by the
    /// `GhInvocation` facade; `None` for a caller recording outside it.
    pub role: Option<String>,
    /// The rate-limit bucket a *reader* call spent (#10232): the reader
    /// App's id plus the owner whose installation it ran under — public,
    /// non-secret labels. Two readers share the `reader` role but not a
    /// budget, so budget readings are kept per bucket. `None` for the
    /// writer and for a caller that does not know its reader.
    pub bucket: Option<String>,
}

impl CallIdentity {
    /// An identity naming only the operation.
    #[must_use]
    pub fn operation(operation: &str) -> Self {
        Self {
            operation: sanitize(operation),
            ..Self::default()
        }
    }

    /// An identity naming a typed [`ForgeOp`]; a deliberate `unknown`
    /// leaves the operation empty, so it records as [`UNKNOWN_OPERATION`].
    #[must_use]
    pub fn for_op(op: ForgeOp) -> Self {
        op.id().map_or_else(Self::default, Self::operation)
    }

    #[must_use]
    pub fn with_provider(mut self, provider: &str) -> Self {
        self.provider = sanitize(provider);
        self
    }

    #[must_use]
    pub fn with_origin(mut self, origin: &str) -> Self {
        self.origin = sanitize(origin);
        self
    }

    #[must_use]
    pub fn with_repo(mut self, repo: &str) -> Self {
        self.repo = sanitize(repo);
        self
    }

    #[must_use]
    pub fn with_role(mut self, role: &str) -> Self {
        self.role = sanitize(role);
        self
    }

    #[must_use]
    pub fn with_bucket(mut self, bucket: &str) -> Self {
        self.bucket = sanitize(bucket);
        self
    }

    /// Fully-qualified identity of a repository-scoped artifact:
    /// `<provider>:<origin>/<owner>/<repo>` (plus `#<number>` when given).
    ///
    /// Two origins carrying the same slug and the same issue/PR number produce
    /// two different keys — which is the whole point. An unqualified `#12` is
    /// ambiguous the moment a second forge exists, and an ambiguous key is how
    /// a trust or claim decision gets made against the wrong artifact.
    #[must_use]
    pub fn qualified_key(&self, number: Option<u64>) -> String {
        let provider = self.provider.as_deref().unwrap_or("unknown");
        let origin = self.origin.as_deref().unwrap_or("unknown");
        let repo = self.repo.as_deref().unwrap_or("unknown");
        match number {
            Some(n) => format!("{provider}:{origin}/{repo}#{n}"),
            None => format!("{provider}:{origin}/{repo}"),
        }
    }

    /// Is every field empty? Such an identity contributes nothing to a line.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operation.is_none()
            && self.provider.is_none()
            && self.origin.is_none()
            && self.repo.is_none()
    }
}

/// The free `x-ratelimit-*` headers of one REST response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitHeaders {
    pub resource: Option<String>,
    pub remaining: Option<u64>,
    /// The pool's total spend this GitHub window by every client of the
    /// credential (Issue #9855) — with the host ledger this splits into own
    /// vs external consumption.
    pub used: Option<u64>,
    pub reset_epoch: Option<i64>,
}

impl RateLimitHeaders {
    /// Absorb one `name: value` header line if it is a rate-limit header.
    pub fn absorb(&mut self, name: &str, value: &str) {
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "x-ratelimit-resource" => self.resource = Some(value.to_string()),
            "x-ratelimit-remaining" => self.remaining = value.parse().ok(),
            "x-ratelimit-used" => self.used = value.parse().ok(),
            "x-ratelimit-reset" => self.reset_epoch = value.parse().ok(),
            _ => {}
        }
    }
}

/// Classify one `gh api --include` result into `(pool, outcome)`. `gh` exits
/// non-zero on a `304`, so the status line — not the exit code — decides.
#[must_use]
pub fn classify(response: Option<&HttpResponse>, exit_ok: bool, stderr: &str) -> (Pool, Outcome) {
    let pool = response
        .and_then(|r| r.ratelimit.resource.as_deref())
        .map_or(Pool::Core, Pool::from_resource);
    let status = response.map(|r| r.status);
    let exhausted = response.is_some_and(|r| r.ratelimit.remaining == Some(0));
    let outcome = match status {
        Some(304) => Outcome::NotModified,
        Some(200..=299) if exit_ok => Outcome::Ok,
        _ if crate::rate_limit_breaker::indicates_rate_limit(stderr) => Outcome::RateLimited,
        Some(429) => Outcome::RateLimited,
        Some(403) if exhausted => Outcome::RateLimited,
        _ => Outcome::Error,
    };
    (pool, outcome)
}

/// Record one `gh api --include` call by `caller` (see [`classify`]) with the
/// #9777 call identity. Plain `gh` spawns are recorded by the `GhInvocation`
/// facade (#10089); this is for a caller that carries an explicit identity.
pub fn record_gh_api_with_identity(
    caller: &'static str,
    identity: &CallIdentity,
    response: Option<&HttpResponse>,
    exit_ok: bool,
    stderr: &str,
) {
    let (pool, outcome) = classify(response, exit_ok, stderr);
    record_with_identity(caller, identity, pool, outcome, response.map(|r| &r.ratelimit));
}

/// Record one forge call. Never blocks or fails the caller: a poisoned lock
/// or an unwritable sink is silently skipped.
pub fn record(
    caller: &'static str,
    pool: Pool,
    outcome: Outcome,
    headers: Option<&RateLimitHeaders>,
) {
    record_with_identity(caller, &CallIdentity::default(), pool, outcome, headers);
}

/// [`record`] plus the #9777 call identity. An identity with no operation is
/// recorded as [`UNKNOWN_OPERATION`] rather than as an absent field, so an
/// un-migrated call site is *visible* in the accounting instead of silently
/// indistinguishable from one that has no operation at all.
pub fn record_with_identity(
    caller: &'static str,
    identity: &CallIdentity,
    pool: Pool,
    outcome: Outcome,
    headers: Option<&RateLimitHeaders>,
) {
    let line = SinkLine {
        t: Utc::now().timestamp(),
        c: caller.to_string(),
        p: pool,
        o: outcome,
        rem: headers.and_then(|h| h.remaining),
        usd: headers.and_then(|h| h.used),
        rst: headers.and_then(|h| h.reset_epoch),
        op: Some(
            identity
                .operation
                .clone()
                .unwrap_or_else(|| UNKNOWN_OPERATION.to_string()),
        ),
        pv: identity.provider.clone(),
        og: identity.origin.clone(),
        rp: identity.repo.clone(),
        ir: identity.role.clone(),
        ib: identity.bucket.clone(),
    };
    if let Ok(mut state) = process_state().lock() {
        state.add(&line);
    }
    if let Some(dir) = sink_dir() {
        if let Err(e) = append(&dir, &line) {
            log::debug!("forge_call_stats: sink append to {} failed: {e}", dir.display());
        }
    }
}

// ============================================================================
// Aggregation
// ============================================================================

/// One sink line (short keys: ~7k lines/hour on a busy host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SinkLine {
    t: i64,
    c: String,
    p: Pool,
    o: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rem: Option<u64>,
    /// `x-ratelimit-used` of that response (Issue #9855). Absent on lines
    /// written by a pre-#9855 binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rst: Option<i64>,
    /// Inventoried operation ID (#9777); `unknown` for an un-migrated caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    op: Option<String>,
    /// Provider family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pv: Option<String>,
    /// Origin host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    og: Option<String>,
    /// `owner/repo` slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rp: Option<String>,
    /// Identity role (#9872); absent on pre-#9872 lines and non-facade calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ir: Option<String>,
    /// Reader rate-limit bucket (#10232); absent for the writer and on
    /// older lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ib: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    ok: u64,
    not_modified: u64,
    rate_limited: u64,
    error: u64,
}

/// Add one outcome to a counter bucket.
fn bump(c: &mut Counts, outcome: Outcome) {
    match outcome {
        Outcome::Ok => c.ok += 1,
        Outcome::NotModified => c.not_modified += 1,
        Outcome::RateLimited => c.rate_limited += 1,
        Outcome::Error => c.error += 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    remaining: u64,
    /// The pool's total spend (`x-ratelimit-used`), when the line carried it.
    /// A newer line without it (a pre-#9855 binary sharing the sink) keeps
    /// the last known value — staleness is bounded by the reading's age.
    used: Option<u64>,
    reset_epoch: Option<i64>,
    observed_at: i64,
}

/// The #9777 call-identity key: operation × provider × origin × repo. Bounded
/// by the inventory's size times the number of distinct repositories a host
/// touches, never by call volume.
type IdentityKey = (String, Option<String>, Option<String>, Option<String>);

/// Counts per `(caller, pool)` plus the newest header reading per pool.
/// Bounded by the number of distinct callers × pools, never by call volume.
#[derive(Debug, Default)]
struct Aggregate {
    started_at: i64,
    counts: BTreeMap<(String, Pool), Counts>,
    identities: BTreeMap<IdentityKey, Counts>,
    /// Counts per identity role (#9872); a line without one is `unknown`.
    roles: BTreeMap<String, Counts>,
    latest: BTreeMap<Pool, Reading>,
    /// The newest reading per `(pool, reader bucket)` (#10232): `latest`
    /// above collapses every identity, but each reader App installation owns
    /// a separate budget (two readers share the `reader` role, so the role
    /// is not a key). Only lines that name a bucket land here.
    latest_by_bucket: BTreeMap<(Pool, String), Reading>,
}

impl Aggregate {
    fn add(&mut self, line: &SinkLine) {
        let c = self.counts.entry((line.c.clone(), line.p)).or_default();
        bump(c, line.o);
        let role = line.ir.as_deref().unwrap_or(UNKNOWN_OPERATION);
        bump(self.roles.entry(role.to_string()).or_default(), line.o);
        if let Some(operation) = line.op.clone() {
            let key: IdentityKey = (operation, line.pv.clone(), line.og.clone(), line.rp.clone());
            let i = self.identities.entry(key).or_default();
            bump(i, line.o);
        }
        if let Some(remaining) = line.rem {
            let newer = self
                .latest
                .get(&line.p)
                .is_none_or(|r| r.observed_at <= line.t);
            if newer {
                let used = line
                    .usd
                    .or_else(|| self.latest.get(&line.p).and_then(|r| r.used));
                let reading = Reading {
                    remaining,
                    used,
                    reset_epoch: line.rst,
                    observed_at: line.t,
                };
                self.latest.insert(line.p, reading);
            }
            if let Some(bucket) = line.ib.clone() {
                let key = (line.p, bucket);
                if self
                    .latest_by_bucket
                    .get(&key)
                    .is_none_or(|r| r.observed_at <= line.t)
                {
                    let reading = Reading {
                        remaining,
                        used: line.usd,
                        reset_epoch: line.rst,
                        observed_at: line.t,
                    };
                    self.latest_by_bucket.insert(key, reading);
                }
            }
        }
    }

    /// Budget-costing calls per pool over the aggregated lines: `ok` and
    /// `error` outcomes (a `304` and a rate-limited call cost nothing).
    fn consumed_per_pool(&self) -> BTreeMap<Pool, u64> {
        let mut spent: BTreeMap<Pool, u64> = BTreeMap::new();
        for ((_, pool), c) in &self.counts {
            *spent.entry(*pool).or_default() += c.ok + c.error;
        }
        spent
    }

    fn rows(&self) -> Vec<ForgeCallCounts> {
        self.counts
            .iter()
            .map(|((caller, pool), c)| ForgeCallCounts {
                caller: caller.clone(),
                pool: pool.as_str().to_string(),
                ok: c.ok,
                not_modified: c.not_modified,
                rate_limited: c.rate_limited,
                error: c.error,
            })
            .collect()
    }

    /// The #9872 per-identity-role rows.
    fn role_rows(&self) -> Vec<crate::types::ForgeIdentityRoleCounts> {
        self.roles
            .iter()
            .map(|(role, c)| crate::types::ForgeIdentityRoleCounts {
                role: role.clone(),
                ok: c.ok,
                not_modified: c.not_modified,
                rate_limited: c.rate_limited,
                error: c.error,
            })
            .collect()
    }

    /// The #9777 identity rows (operation × provider × origin × repo).
    fn identity_rows(&self) -> Vec<ForgeOperationCounts> {
        self.identities
            .iter()
            .map(|((operation, provider, origin, repo), c)| ForgeOperationCounts {
                operation: operation.clone(),
                provider: provider.clone(),
                origin: origin.clone(),
                repo: repo.clone(),
                ok: c.ok,
                not_modified: c.not_modified,
                rate_limited: c.rate_limited,
                error: c.error,
            })
            .collect()
    }
}

/// Aggregate sink `lines` with `t >= since`; unparseable lines are skipped.
fn aggregate_lines<'a>(lines: impl Iterator<Item = &'a str>, since: i64) -> Aggregate {
    let mut agg = Aggregate {
        started_at: since,
        ..Aggregate::default()
    };
    for line in lines {
        if let Ok(parsed) = serde_json::from_str::<SinkLine>(line) {
            if parsed.t >= since {
                agg.add(&parsed);
            }
        }
    }
    agg
}

fn process_state() -> &'static Mutex<Aggregate> {
    static STATE: OnceLock<Mutex<Aggregate>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(Aggregate {
            started_at: Utc::now().timestamp(),
            ..Aggregate::default()
        })
    })
}

// ============================================================================
// Host sink
// ============================================================================

#[cfg(not(test))]
fn sink_dir() -> Option<PathBuf> {
    match std::env::var("LOOM_FORGE_CALL_STATS_DIR") {
        Ok(d) if d == "off" || d == "0" => None,
        Ok(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => Some(crate::forge_etag_store::host_tmp_base().join("loom-forge-call-stats")),
    }
}

/// Test builds: no sink unless the current test thread opts in, so the many
/// fake-`gh` tests never write into a real host directory.
#[cfg(test)]
fn sink_dir() -> Option<PathBuf> {
    TEST_SINK_DIR.with(|d| d.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_SINK_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point THIS test thread's sink at `dir` (`None` = off).
#[cfg(test)]
pub(crate) fn set_test_sink_dir(dir: Option<PathBuf>) {
    TEST_SINK_DIR.with(|d| *d.borrow_mut() = dir);
}

fn sink_file(dir: &Path, hour: i64) -> PathBuf {
    dir.join(format!("calls-{hour}.jsonl"))
}

fn append(dir: &Path, line: &SinkLine) -> std::io::Result<()> {
    // Same owner-only rules as the ETag store: a 0700 dir we own, 0600 files.
    if !crate::forge_etag_store::private_dir(dir, true) {
        return Err(std::io::Error::other("untrusted sink dir"));
    }
    let mut buf = serde_json::to_vec(line)?;
    buf.push(b'\n');
    let hour = line.t.div_euclid(3600);
    let path = sink_file(dir, hour);
    let mut create = std::fs::OpenOptions::new();
    create.append(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut create, 0o600);
    let (mut file, fresh) = match create.open(&path) {
        Ok(f) => (f, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            (std::fs::OpenOptions::new().append(true).open(&path)?, false)
        }
        Err(e) => return Err(e),
    };
    // One write of one short line: atomic under O_APPEND.
    file.write_all(&buf)?;
    if fresh {
        prune(dir, hour);
    }
    Ok(())
}

/// Remove sink files more than [`RETAIN_HOURS`] older than `current_hour`.
fn prune(dir: &Path, current_hour: i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let hour = name
            .to_str()
            .and_then(|n| n.strip_prefix("calls-"))
            .and_then(|n| n.strip_suffix(".jsonl"))
            .and_then(|n| n.parse::<i64>().ok());
        if hour.is_some_and(|h| h < current_hour - RETAIN_HOURS) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Aggregate the sink's last [`WINDOW_SECS`] as of `now` (missing hour files
/// simply contribute nothing).
fn read_window(dir: &Path, now: i64) -> Aggregate {
    let since = now - WINDOW_SECS;
    aggregate_lines(read_since(dir, since, now).lines(), since)
}

/// The raw sink text of every hour file covering `since..=now`.
fn read_since(dir: &Path, since: i64, now: i64) -> String {
    let mut raw = String::new();
    for hour in since.div_euclid(3600)..=now.div_euclid(3600) {
        if let Ok(text) = std::fs::read_to_string(sink_file(dir, hour)) {
            raw.push_str(&text);
        }
    }
    raw
}

/// One operation ID as the sink observed it (Issue #9831).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ObservedOperation {
    /// Calls recorded under this operation, every outcome included.
    pub calls: u64,
    /// The `caller` labels that recorded it — for an `unknown` row, the list
    /// of sites still to map.
    pub callers: std::collections::BTreeSet<String>,
}

/// Operation IDs observed in sink `lines` with `t >= since`. A line written by
/// a pre-#9777 binary carries no operation and counts as
/// [`UNKNOWN_OPERATION`], exactly as an un-migrated caller would.
fn observed_in_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    since: i64,
) -> BTreeMap<String, ObservedOperation> {
    let mut seen: BTreeMap<String, ObservedOperation> = BTreeMap::new();
    for line in lines {
        let Ok(parsed) = serde_json::from_str::<SinkLine>(line) else {
            continue;
        };
        if parsed.t < since {
            continue;
        }
        let op = parsed.op.unwrap_or_else(|| UNKNOWN_OPERATION.to_string());
        let entry = seen.entry(op).or_default();
        entry.calls += 1;
        entry.callers.insert(parsed.c);
    }
    seen
}

/// Every operation ID this host's sink still retains (the last
/// [`RETAIN_HOURS`] hours, not just the status window), as of `now`. `None`
/// when the sink is disabled — nothing observed is unknown, not empty.
///
/// This is runtime evidence only: it shows what the daemon *did* call, never
/// what it *can* call. It supplements the source inventory and can never
/// establish exhaustiveness on its own (#9777).
#[must_use]
pub fn observed_operations(now: DateTime<Utc>) -> Option<BTreeMap<String, ObservedOperation>> {
    Some(observed_operations_in(&sink_dir()?, now))
}

/// [`observed_operations`] over an explicit sink directory (the CLI's
/// `--sink-dir`). Reads only; a missing directory observes nothing.
#[must_use]
pub fn observed_operations_in(
    dir: &Path,
    now: DateTime<Utc>,
) -> BTreeMap<String, ObservedOperation> {
    let now = now.timestamp();
    let since = now - RETAIN_HOURS * 3600;
    observed_in_lines(read_since(dir, since, now).lines(), since)
}

// ============================================================================
// Status
// ============================================================================

fn epoch(t: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(t, 0).single()
}

/// Build the `status` view as of `now`: host-wide window from the sink, this
/// process's since-start totals, and the newest budget reading per pool —
/// header-derived, or `breaker`'s probe when that is newer.
#[must_use]
pub fn status_report(
    now: DateTime<Utc>,
    breaker: Option<&crate::rate_limit_breaker::RateLimitSnapshot>,
) -> ForgeCallsStatus {
    let now_ts = now.timestamp();
    let window = sink_dir().map(|d| read_window(&d, now_ts));
    let own_window = window.as_ref().map(|w| {
        w.consumed_per_pool()
            .into_iter()
            .map(|(pool, consumed)| crate::types::ForgePoolSpend {
                pool: pool.as_str().to_string(),
                consumed,
            })
            .collect()
    });
    let (since_start, since, process_latest) = match process_state().lock() {
        Ok(s) => (s.rows(), epoch(s.started_at), s.latest.clone()),
        Err(_) => (Vec::new(), None, BTreeMap::new()),
    };
    let mut latest = process_latest;
    for (pool, r) in window.iter().flat_map(|w| w.latest.iter()) {
        if latest
            .get(pool)
            .is_none_or(|l| l.observed_at < r.observed_at)
        {
            latest.insert(*pool, *r);
        }
    }
    let mut budget: BTreeMap<Pool, ForgeBudgetReading> = latest
        .iter()
        .filter_map(|(pool, r)| {
            Some((
                *pool,
                ForgeBudgetReading {
                    pool: pool.as_str().to_string(),
                    remaining: r.remaining,
                    used: r.used,
                    reset_at: r.reset_epoch.and_then(epoch),
                    observed_at: epoch(r.observed_at)?,
                    source: "headers".to_string(),
                },
            ))
        })
        .collect();
    if let Some(b) = breaker {
        if let Some(probed_at) = b.budget_probed_at {
            for (pool, remaining, used) in [
                (Pool::Core, b.core_remaining, b.core_used),
                (Pool::Graphql, b.graphql_remaining, b.graphql_used),
            ] {
                let Some(remaining) = remaining else { continue };
                if budget.get(&pool).is_none_or(|r| r.observed_at < probed_at) {
                    let reading = ForgeBudgetReading {
                        pool: pool.as_str().to_string(),
                        remaining,
                        used,
                        reset_at: None,
                        observed_at: probed_at,
                        source: "breaker_probe".to_string(),
                    };
                    budget.insert(pool, reading);
                }
            }
        }
    }
    ForgeCallsStatus {
        window_secs: WINDOW_SECS.unsigned_abs(),
        operations: window.as_ref().map(Aggregate::identity_rows),
        identity_roles: window.as_ref().map(Aggregate::role_rows),
        host_window: window.map(|w| w.rows()),
        since_start,
        since,
        budget: budget.into_values().collect(),
        own_window,
    }
}

/// The rate-limit pools this process last read at **zero remaining** and
/// that have not reset yet at `now`, each with its reset instant when the
/// header carried one (#10210, the ETA's `rate_limit_quota` stall).
///
/// Read-only and in-process: the newest free `x-ratelimit-*` reading per pool
/// this daemon already absorbed — no file read, no forge call. A reading
/// older than [`WINDOW_SECS`] with no reset is too stale to call a stall.
#[must_use]
pub fn exhausted_pools(now: DateTime<Utc>) -> Vec<(Pool, Option<DateTime<Utc>>)> {
    match process_state().lock() {
        Ok(state) => exhausted_in(&state.latest, now.timestamp()),
        Err(_) => Vec::new(),
    }
}

/// [`exhausted_pools`] over an explicit reading map. Pure.
fn exhausted_in(latest: &BTreeMap<Pool, Reading>, now: i64) -> Vec<(Pool, Option<DateTime<Utc>>)> {
    latest
        .iter()
        .filter(|(_, r)| r.remaining == 0)
        .filter(|(_, r)| match r.reset_epoch {
            Some(reset) => reset > now,
            None => now - r.observed_at <= WINDOW_SECS,
        })
        .map(|(pool, r)| (*pool, r.reset_epoch.and_then(epoch)))
        .collect()
}

/// The newest header budget reading of each pool, per **reader rate-limit
/// bucket** (#10232), keyed by the public bucket label
/// ([`crate::forge_identity::reader_bucket`]). Unlike [`status_report`]'s
/// `budget` this never mixes identities: two readers of the same role are
/// separate buckets, and a line that named no bucket (the writer, an
/// unattributed call) is not returned at all. Never a credential.
#[must_use]
pub fn bucket_readings(now: DateTime<Utc>) -> BTreeMap<String, Vec<ForgeBudgetReading>> {
    let window = sink_dir().map(|d| read_window(&d, now.timestamp()));
    let process = process_state()
        .lock()
        .map(|s| s.latest_by_bucket.clone())
        .unwrap_or_default();
    let mut latest = process;
    for (key, r) in window.iter().flat_map(|w| w.latest_by_bucket.iter()) {
        if latest
            .get(key)
            .is_none_or(|l| l.observed_at < r.observed_at)
        {
            latest.insert(key.clone(), *r);
        }
    }
    let mut out: BTreeMap<String, Vec<ForgeBudgetReading>> = BTreeMap::new();
    for ((pool, bucket), r) in latest {
        let Some(observed_at) = epoch(r.observed_at) else {
            continue;
        };
        out.entry(bucket).or_default().push(ForgeBudgetReading {
            pool: pool.as_str().to_string(),
            remaining: r.remaining,
            used: r.used,
            reset_at: r.reset_epoch.and_then(epoch),
            observed_at,
            source: "headers".to_string(),
        });
    }
    out
}

/// This host's budget-costing forge calls per pool over the last window —
/// `ok` + `error` outcomes, host-wide from the sink (Issue #9855). The
/// breaker's trip-time attribution log divides a pool's `used` by this to
/// estimate the external share. `None` when the sink is disabled — own
/// consumption is unknown, not zero.
#[must_use]
pub fn consumed_in_window(now: DateTime<Utc>) -> Option<BTreeMap<Pool, u64>> {
    let dir = sink_dir()?;
    Some(read_window(&dir, now.timestamp()).consumed_per_pool())
}

#[cfg(test)]
#[path = "forge_call_stats_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "forge_call_stats_callsite_tests.rs"]
mod callsite_tests;
