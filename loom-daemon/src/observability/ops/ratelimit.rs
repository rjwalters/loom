//! GitHub rate-limit visibility (Issue #10022): breaker trips, own/external
//! attribution, quota gauges and breaker skips, exported to SigNoz.
//!
//! Before this module the only evidence of a starved host was the breaker's
//! `attribution:` line in the local `daemon.log`. Three signals now leave the
//! host through the shared ops path (OTLP only, a no-op without a sink):
//!
//! - **`loom.ratelimit.trip` span**, one per breaker trip (never per re-trip
//!   while cooling): the [`Job`] that tripped it, the cooldown end, and the
//!   probe's `used` per pool split into this host's own ledger share and the
//!   external share. `own`/`external` are omitted, not zeroed, when the probe
//!   carried no `used` or the forge-call ledger is off.
//! - **`github.ratelimit.{remaining,used,reset}` gauges** (W1, #10343), one
//!   series per billed bucket: every point is labelled `resource`,
//!   `account`, `owner` and `role` (and a bucket point `installation`). On
//!   an App host every reading in [`crate::forge_bucket_book`] is exported
//!   once per tick, and the 60 s `gh api rate_limit` probe (free — it does
//!   not count against the quota) is booked there under the primary
//!   directory's own key (the identity minted into it, #10571) first, so no
//!   point leaves without an `owner` and the legacy owner-less series no
//!   longer interleaves with a bucket's. An ambient-login host (an operator's
//!   own `gh`) has no bucket book entries of its own: it exports the probe
//!   (falling back to the breaker's trip-time budget) as `owner="-"`,
//!   `role="ambient"`.
//! - **`loom.forge.calls`** (W1): the facade's delta counter
//!   ([`super::forge_calls`]), flushed on this tick.
//! - **`github.ratelimit.breaker_skips{reason=<job>}`**, a delta counter: one
//!   per pass a job skipped because the breaker was suppressing. Skip sites
//!   call [`record_skip`] (via `rate_limit_breaker::global_skip_pass`), which
//!   bumps an in-process atomic; the gauge tick flushes the counts, so the
//!   point rate stays bounded however often a loop polls.
//!
//! **No secret leaves the host.** `account` is `app-<app id>` for the
//! daemon's GitHub App credential, a validated GitHub login for an ambient
//! `gh` credential, else `unknown` — never a token, token hash or path
//! ([`app_account_label`], [`login_account_label`]). `owner` is a validated
//! GitHub owner (lowercased), `unknown`, or [`AMBIENT_OWNER`]; `role` is one
//! of `writer`, `reader`, [`AMBIENT_ROLE`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::forge_call_stats::Pool;
use crate::rate_limit_breaker::BudgetSnapshot;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// The daemon job a trip or skip is attributed to — the `reason` label and
/// `loom.ratelimit.source` attribute's whole vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Job {
    WorkFinder,
    ClaimReconciliation,
    RoleRunner,
    EpicSupervisor,
    QuarantineReconciliation,
    CiTelemetry,
    OutcomeJournal,
    StarLiveness,
    Other,
}

impl Job {
    /// Every value, in label order.
    pub const ALL: [Self; 9] = [
        Self::WorkFinder,
        Self::ClaimReconciliation,
        Self::RoleRunner,
        Self::EpicSupervisor,
        Self::QuarantineReconciliation,
        Self::CiTelemetry,
        Self::OutcomeJournal,
        Self::StarLiveness,
        Self::Other,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkFinder => "work_finder",
            Self::ClaimReconciliation => "claim_reconciliation",
            Self::RoleRunner => "role_runner",
            Self::EpicSupervisor => "epic_supervisor",
            Self::QuarantineReconciliation => "quarantine_reconciliation",
            Self::CiTelemetry => "ci_telemetry",
            Self::OutcomeJournal => "outcome_journal",
            Self::StarLiveness => "star_liveness",
            Self::Other => "other",
        }
    }

    /// Classify a breaker `source` string (`work_finder_starred_at`,
    /// `sweep_outcome_points_signal`, …). Unknown text maps to
    /// [`Job::Other`], so free text never becomes a label value.
    #[must_use]
    pub fn from_source(source: &str) -> Self {
        let prefixed = |p: &str| source == p || source.starts_with(&format!("{p}_"));
        if prefixed("work_finder") {
            Self::WorkFinder
        } else if prefixed("claim_reconciliation") {
            Self::ClaimReconciliation
        } else if prefixed("role_runner") {
            Self::RoleRunner
        } else if prefixed("epic_supervisor") {
            Self::EpicSupervisor
        } else if prefixed("quarantine_reconciliation") || prefixed("quarantine_release") {
            Self::QuarantineReconciliation
        } else if prefixed("ci_telemetry") {
            Self::CiTelemetry
        } else if prefixed("sweep_outcome") || prefixed("outcome_journal") {
            Self::OutcomeJournal
        } else if prefixed("star_liveness") {
            Self::StarLiveness
        } else {
            Self::Other
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|j| *j == self)
            .unwrap_or(Self::ALL.len() - 1)
    }
}

// ---------------------------------------------------------------- trip span

/// One pool's share of a trip: the probe's pool-wide `used`, and this host's
/// own ledger count. Each is `None` when unknown — never reported as 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolShare {
    pub used: Option<u64>,
    pub own: Option<u64>,
}

impl PoolShare {
    /// `used − own`, when both are known.
    #[must_use]
    pub fn external(self) -> Option<u64> {
        Some(self.used?.saturating_sub(self.own?))
    }

    /// The breaker log line's rendering (Issue #9855), unchanged.
    #[must_use]
    pub fn log_text(self) -> String {
        match (self.used, self.own) {
            (Some(u), Some(o)) => format!("used={u} own≈{o} external≈{}", u.saturating_sub(o)),
            (Some(u), None) => format!("used={u} own=? (sink off) external=?"),
            (None, _) => "used=? (probe without used)".to_string(),
        }
    }
}

/// A trip's attribution across both pools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TripAttribution {
    pub core: PoolShare,
    pub graphql: PoolShare,
}

impl TripAttribution {
    /// From the trip-time probe and this host's forge-call ledger
    /// (`forge_call_stats::consumed_in_window`; `None` when the sink is off).
    /// `own` is only meaningful next to a known `used`, so it is dropped
    /// when `used` is unknown.
    #[must_use]
    pub fn from_budget(
        budget: Option<&BudgetSnapshot>,
        ledger: Option<&BTreeMap<Pool, u64>>,
    ) -> Self {
        let share = |used: Option<u64>, pool: Pool| PoolShare {
            used,
            own: used.and(ledger.map(|m| m.get(&pool).copied().unwrap_or(0))),
        };
        TripAttribution {
            core: share(budget.and_then(|b| b.core_used), Pool::Core),
            graphql: share(budget.and_then(|b| b.graphql_used), Pool::Graphql),
        }
    }
}

/// The `loom.ratelimit.trip` span for a trip by `job` at `tripped_at`: an
/// instant span, its own root trace, IDs derived from the job and the trip
/// instant (`trace-identity.md`).
#[must_use]
pub fn trip_span(
    job: Job,
    tripped_at: DateTime<Utc>,
    cooldown_until: Option<DateTime<Utc>>,
    attribution: &TripAttribution,
) -> SpanRecord {
    let mut attributes = TraceAttributes::new();
    attributes.insert("loom.ratelimit.source".into(), job.as_str().into());
    if let Some(until) = cooldown_until {
        attributes.insert(
            "loom.ratelimit.cooldown_until".into(),
            until.to_rfc3339_opts(SecondsFormat::Secs, true),
        );
    }
    for (pool, share) in [("core", attribution.core), ("graphql", attribution.graphql)] {
        for (field, value) in [
            ("used", share.used),
            ("own", share.own),
            ("external", share.external()),
        ] {
            if let Some(value) = value {
                attributes.insert(format!("github.ratelimit.{pool}.{field}"), value.to_string());
            }
        }
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: TraceContext::derived(
            SpanName::RateLimitTrip.as_str(),
            &[job.as_str(), &crate::telemetry::trace::instant(tripped_at)],
        ),
        parent_span_id: None,
        name: SpanName::RateLimitTrip,
        started_at: tripped_at,
        ended_at: tripped_at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one trip. A no-op when no ops sink is registered.
pub fn record_trip(
    source: &str,
    tripped_at: DateTime<Utc>,
    cooldown_until: Option<DateTime<Utc>>,
    attribution: &TripAttribution,
) {
    if !super::spans_exported() {
        return;
    }
    super::emit_span(trip_span(Job::from_source(source), tripped_at, cooldown_until, attribution));
}

// ---------------------------------------------------------------- skips

static SKIPS: [AtomicU64; Job::ALL.len()] = [const { AtomicU64::new(0) }; Job::ALL.len()];
static LAST_FLUSH: Mutex<Option<DateTime<Utc>>> = Mutex::new(None);

/// Count one pass `job` skipped because the breaker was suppressing. A
/// no-op (no atomic touched) when ops signals are not exported.
pub fn record_skip(job: Job) {
    if super::spans_exported() {
        SKIPS[job.index()].fetch_add(1, Ordering::Relaxed);
    }
}

/// Drain the skip counts into `breaker_skips` points (zero counts emit
/// nothing).
#[must_use]
pub fn drain_skip_points() -> Vec<MetricPoint> {
    Job::ALL
        .iter()
        .filter_map(|job| {
            let n = SKIPS[job.index()].swap(0, Ordering::Relaxed);
            (n > 0).then(|| {
                MetricPoint::int(MetricName::GithubRateLimitBreakerSkips, clamp(n))
                    .label("reason", job.as_str())
            })
        })
        .collect()
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------- quota gauge

/// `app-<app id>` for a GitHub App credential, or `unknown` when the id is
/// not a plain number.
#[must_use]
pub fn app_account_label(app_id: &str) -> String {
    let ok = !app_id.is_empty() && app_id.len() <= 20 && app_id.bytes().all(|b| b.is_ascii_digit());
    if ok {
        format!("app-{app_id}")
    } else {
        "unknown".to_string()
    }
}

/// A GitHub login as the `account` label: 1–39 ASCII alphanumerics or
/// hyphens, not starting with a hyphen — GitHub's own login rule. Anything
/// else (a token, which has `_`; a path; the App placeholder
/// `x-access-token`) becomes `unknown`.
#[must_use]
pub fn login_account_label(login: &str) -> String {
    let login = login.trim();
    let ok = (1..=39).contains(&login.len())
        && !login.starts_with('-')
        && login != "x-access-token"
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        login.to_string()
    } else {
        "unknown".to_string()
    }
}

/// The `owner` label of a point that is not one App installation's bucket
/// (the convention `forge.reader.owner` uses for "App-wide").
pub const AMBIENT_OWNER: &str = "-";

/// The `role` label of an ambient-login host's points.
pub const AMBIENT_ROLE: &str = "ambient";

/// Gauges for one budget reading, labelled `resource`, `account`, `owner`
/// and `role` — an ambient-login host's probe ([`AMBIENT_OWNER`],
/// [`AMBIENT_ROLE`]); an App host books its probe instead ([`book_budget`]).
#[must_use]
pub fn quota_points(
    budget: &BudgetSnapshot,
    account: &str,
    owner: &str,
    role: &str,
) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    for (resource, remaining, used, reset) in [
        ("core", budget.core_remaining, budget.core_used, budget.core_reset),
        ("graphql", budget.graphql_remaining, budget.graphql_used, budget.graphql_reset),
    ] {
        let point = |name, value| {
            MetricPoint::int(name, value)
                .label("resource", resource)
                .label("account", account)
                .label("owner", owner)
                .label("role", role)
        };
        points.push(point(MetricName::GithubRateLimitRemaining, clamp(remaining)));
        if let Some(used) = used {
            points.push(point(MetricName::GithubRateLimitUsed, clamp(used)));
        }
        points.push(point(MetricName::GithubRateLimitReset, reset.timestamp()));
    }
    points
}

/// Gauges for every believed bucket-book reading (W1), labelled
/// `resource`, `account`, `owner`, `installation` (#10571, `-` when no
/// sidecar named one) and `role` — one series per billed bucket. `role` is
/// `writer` for the workspace's writer App (`writer_account`: the App
/// minted into the primary credential directory, else the roster's —
/// [`writer_account_for`]) and `reader` for any other App.
#[must_use]
pub fn bucket_points(
    readings: &[(crate::forge_bucket_book::BucketKey, crate::forge_bucket_book::Reading)],
    writer_account: &str,
) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    for (key, reading) in readings {
        let role = if key.account == writer_account {
            "writer"
        } else {
            "reader"
        };
        let point = |name, value| {
            MetricPoint::int(name, value)
                .label("resource", key.resource.as_str())
                .label("account", key.account.as_str())
                .label("owner", key.owner.as_str())
                .label("installation", key.installation.as_str())
                .label("role", role)
        };
        if let Some(remaining) = reading.remaining {
            points.push(point(MetricName::GithubRateLimitRemaining, clamp(remaining)));
        }
        if let Some(used) = reading.used {
            points.push(point(MetricName::GithubRateLimitUsed, clamp(used)));
        }
        points.push(point(MetricName::GithubRateLimitReset, reading.reset_epoch));
    }
    points
}

/// Book one 60 s probe reading into the bucket book (#10343) under
/// `(account, owner, core|graphql)`, witnessed by `installation`, with
/// [`Source::Probe`], read at the
/// snapshot's `probed_at`. Only a live probe is booked: the breaker's
/// trip-time fallback would carry the wrong `observed_at`.
///
/// [`Source::Probe`]: crate::forge_bucket_book::Source::Probe
pub fn book_budget(budget: &BudgetSnapshot, account: &str, owner: &str, installation: Option<&str>) {
    use crate::forge_bucket_book::{insert, BucketKey, Reading, Resource, Source};
    for (resource, remaining, used, reset) in [
        (Resource::Core, budget.core_remaining, budget.core_used, budget.core_reset),
        (
            Resource::Graphql,
            budget.graphql_remaining,
            budget.graphql_used,
            budget.graphql_reset,
        ),
    ] {
        insert(
            BucketKey::new(account, owner, resource).with_installation(installation),
            Reading {
                limit: None,
                remaining: Some(remaining),
                used,
                reset_epoch: reset.timestamp(),
                observed_at: budget.probed_at.timestamp(),
                source: Source::Probe,
            },
        );
    }
}

/// Which kind of host a gauge tick runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeHost {
    /// The daemon runs under its GitHub App credential: the probe reads the
    /// writer App's bucket for the workspace's own owner.
    App {
        account: String,
        owner: String,
        installation: Option<String>,
    },
    /// An ambient `gh` login (an operator's machine).
    Ambient,
}

/// What one tick does with its probe: an App host books a live reading and
/// exports nothing here (its points come from [`bucket_points`]); an
/// ambient host exports the probe, or the breaker's still-open `fallback`,
/// labelled with `account()`.
pub fn probe_points(
    host: &ProbeHost,
    probed: Option<BudgetSnapshot>,
    fallback: impl FnOnce() -> Option<BudgetSnapshot>,
    account: impl FnOnce() -> String,
) -> Vec<MetricPoint> {
    match host {
        ProbeHost::App {
            account,
            owner,
            installation,
        } => {
            if let Some(budget) = &probed {
                book_budget(budget, account, owner, installation.as_deref());
            }
            Vec::new()
        }
        ProbeHost::Ambient => probed
            .or_else(fallback)
            .map(|b| quota_points(&b, &account(), AMBIENT_OWNER, AMBIENT_ROLE))
            .unwrap_or_default(),
    }
}

/// Emit `points` in batches no larger than one record carries, so a busy
/// interval's `loom.forge.calls` series are never truncated.
fn emit_chunked(sink: &super::OpsSink, points: Vec<MetricPoint>, since: Option<DateTime<Utc>>) {
    for chunk in points.chunks(crate::telemetry::ops::MAX_POINTS_PER_RECORD) {
        sink.emit_metrics_since(chunk.to_vec(), since);
    }
}

/// The collector's rate-limit tick: one free `gh api rate_limit` probe and
/// one skip-counter flush per interval.
pub const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// The credential identity the probe runs as: resolved once when known;
/// an `unknown` result is cached for [`UNKNOWN_RETRY`] (#10061 review) so a
/// credential that cannot read `/user` does not spend a core call per tick.
static ACCOUNT: Mutex<Option<AccountCache>> = Mutex::new(None);

/// How long an `unknown` account label is reused before re-resolving.
pub const UNKNOWN_RETRY: chrono::Duration = chrono::Duration::minutes(20);

/// The cached account label and when it was last resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountCache {
    pub label: String,
    pub resolved_at: DateTime<Utc>,
}

/// Whether the daemon runs under its GitHub App credential: its process
/// `GH_CONFIG_DIR` is the workspace's App config dir.
fn on_app_host(workspace_root: &Path) -> bool {
    let app_dir = crate::credential_preflight::github_app_gh_config_dir(workspace_root);
    std::env::var_os("GH_CONFIG_DIR").is_some_and(|d| Path::new(&d) == app_dir)
}

/// The tick's [`ProbeHost`] for the workspace at `workspace_root`.
fn probe_host(workspace_root: &Path) -> ProbeHost {
    probe_host_for(workspace_root, on_app_host(workspace_root))
}

/// [`probe_host`] with the App-host predicate supplied. The App key is the
/// identity [`crate::forge_bucket_book::probe_targets`] uses for the same
/// `.loom/gh-config` directory — its sidecar's, else the roster and remote
/// ([`crate::forge_bucket_book::dir_identity`], #10571) — so the 60 s probe
/// adds no key of its own.
fn probe_host_for(workspace_root: &Path, on_app: bool) -> ProbeHost {
    if !on_app {
        return ProbeHost::Ambient;
    }
    let id = primary_identity(workspace_root);
    ProbeHost::App {
        account: id.account,
        owner: id.owner.unwrap_or_else(|| "unknown".to_string()),
        installation: id.installation,
    }
}

/// The identity minted into the workspace's primary credential directory.
fn primary_identity(workspace_root: &Path) -> crate::forge_bucket_book::CredIdentity {
    let dir = crate::credential_preflight::github_app_gh_config_dir(workspace_root);
    let class = crate::forge_bucket_book::DirClass::PrimaryWriter {
        root: workspace_root.to_path_buf(),
    };
    crate::forge_bucket_book::dir_identity(&dir, &class).unwrap_or_else(|| {
        crate::forge_bucket_book::CredIdentity {
            account: crate::forge_bucket_book::writer_account(workspace_root),
            owner: crate::forge_bucket_book::primary_owner(workspace_root),
            installation: None,
            source: crate::forge_bucket_book::IdentitySource::Derived,
        }
    })
}

/// The writer App's account label for [`bucket_points`]' `role`: the App
/// minted into the primary credential directory, else the roster's.
#[must_use]
pub fn writer_account_for(workspace_root: &Path) -> String {
    primary_identity(workspace_root).account
}

/// An ambient host's account label: the `gh` login (one core call).
fn resolve_account() -> String {
    forge::viewer_login().map_or_else(|| "unknown".to_string(), |l| login_account_label(&l))
}

/// The account label from `cache`, resolving (via `resolve`, which may cost
/// one core call) only when nothing is cached, or when a cached `unknown`
/// is older than [`UNKNOWN_RETRY`] and the rate-limit breaker is not
/// `suppressed`. A known label is never re-resolved.
pub fn cached_account(
    cache: &mut Option<AccountCache>,
    now: DateTime<Utc>,
    suppressed: bool,
    resolve: impl FnOnce() -> String,
) -> String {
    if let Some(c) = cache.as_ref() {
        if c.label != "unknown" || now - c.resolved_at < UNKNOWN_RETRY {
            return c.label.clone();
        }
    }
    if suppressed {
        return cache
            .as_ref()
            .map_or_else(|| "unknown".to_string(), |c| c.label.clone());
    }
    let label = resolve();
    *cache = Some(AccountCache {
        label: label.clone(),
        resolved_at: now,
    });
    label
}

fn account(now: DateTime<Utc>) -> String {
    let mut guard = ACCOUNT.lock().unwrap_or_else(PoisonError::into_inner);
    let suppressed = crate::rate_limit_breaker::global_is_suppressed();
    cached_account(&mut guard, now, suppressed, resolve_account)
}

/// The breaker's trip-time reading as a gauge fallback, only while every
/// window it describes is still open — a passed reset means its
/// `remaining` (often 0) is stale and must not be re-exported.
#[must_use]
pub fn fresh_fallback(
    budget: Option<BudgetSnapshot>,
    now: DateTime<Utc>,
) -> Option<BudgetSnapshot> {
    budget.filter(|b| b.core_reset.min(b.graphql_reset) > now)
}

/// One rate-limit tick: flush the skip counter, probe the budget (booking it
/// on an App host, else falling back to the breaker's trip-time reading)
/// and export the gauges — the bucket book's once, after the probe. Returns
/// before any probe when no OTLP exporter is running.
pub async fn record(workspace_root: &Path) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let now = Utc::now();
    let since = LAST_FLUSH
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(now);
    sink.emit_metrics_since(drain_skip_points(), since);
    emit_chunked(sink, super::forge_calls::drain_points(), since);
    sink.emit_metrics_since(super::forge_calls::drain_event_points(), since);
    let root = workspace_root.to_path_buf();
    let sampled = tokio::task::spawn_blocking(move || {
        let host = probe_host(&root);
        let points = probe_points(
            &host,
            crate::rate_limit_breaker::forge::probe_budget(now),
            || {
                fresh_fallback(
                    crate::rate_limit_breaker::global().and_then(|b| b.last_budget()),
                    now,
                )
            },
            || account(now),
        );
        (points, writer_account_for(&root))
    })
    .await;
    let Ok((points, writer)) = sampled else {
        return;
    };
    let book = crate::forge_bucket_book::snapshot(Utc::now().timestamp());
    emit_chunked(sink, bucket_points(&book, &writer), None);
    if !points.is_empty() {
        sink.emit_metrics(points);
    }
}

mod forge {
    use crate::cmd_out::CmdOutcome;
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    use std::time::Duration;

    /// The ambient credential's login (`gh api user`). One core call, made
    /// only until it resolves; `None` for an App token (which cannot read
    /// `/user`) or any failure.
    pub(super) fn viewer_login() -> Option<String> {
        let outcome = GhInvocation::new(
            Operation::new("api.user"),
            AccessIntent::Read,
            GhTarget::None,
            Duration::from_secs(30),
        )
        // Asker-dependent: `/user` is whoever asks (W4-C writer_only).
        .writer_identity()
        .args(["api", "user", "--jq", ".login"])
        .run();
        let CmdOutcome::Ran(output) = outcome else {
            return None;
        };
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "ratelimit_tests.rs"]
mod tests;
