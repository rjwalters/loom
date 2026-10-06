//! What a failed reader read says, and how long the reader is withdrawn for
//! it (W4-A).
//!
//! GitHub meters each App **installation** separately, per resource: reader
//! App 1's installation on owner `acme` has its own `core` and `graphql`
//! pools, independent of the same App's installation on `other`. Before
//! W4-A a rate limit on any one of those pools withdrew the reader App-wide
//! for a flat 300 s, so one owner's dry `core` pool took the reader off every
//! owner and every resource, and pushed all of them onto the writer.
//!
//! Now a withdrawal is scoped to what failed:
//!
//! | failure | scope | until |
//! |---|---|---|
//! | primary rate limit | `(app, owner, resource)` | the bucket's reset: the refusal's `x-ratelimit-reset`, else an on-demand `rate_limit` probe showing the bucket empty, else now + 300 s; clamped to [30 s, 3660 s] |
//! | secondary rate limit | `(app, owner, all)` | now + `Retry-After`, else now + 60 s — never the hourly reset |
//! | bad credentials / 401 | `(app, owner, all)` | now + 300 s |
//! | coverage (403/404) | `(app, owner/repo)` | now + 3600 s (unchanged) |
//!
//! The App-wide [`forge_read_pool::withdraw`] stays for mint and key failures
//! ([`super::withdraw_reader`]).
//!
//! # Kill switch
//!
//! `LOOM_READ_ROUTING=legacy` ([`READ_ROUTING_ENV`]) restores the pre-W4-A
//! behaviour exactly: the old classifier, an App-wide withdrawal for any rate
//! limit or credential failure (until the caller's reset, else 300 s), and no
//! scoped withdrawal consulted. It is read on every call — a plain env read,
//! no `OnceLock` — so it needs no release: a daemon (or any process) started
//! with it set behaves as before W4-A, and no cached value outlives it.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::forge_bucket_book::{self, BucketKey, Resource};
use crate::forge_call_stats::RateLimitHeaders;
use crate::forge_read_pool::{self, ResourceScope};

/// The read-routing kill switch: `legacy` restores the pre-W4-A withdrawal.
pub const READ_ROUTING_ENV: &str = "LOOM_READ_ROUTING";

/// The shortest scoped withdrawal for a primary rate limit.
pub const MIN_SCOPED_WITHDRAWAL: Duration = Duration::from_secs(30);
/// The longest: one GitHub window plus a minute of clock skew.
pub const MAX_SCOPED_WITHDRAWAL: Duration = Duration::from_secs(3660);
/// A secondary limit without `Retry-After`.
pub const SECONDARY_WITHDRAWAL: Duration = Duration::from_secs(60);
/// A credential failure (401, bad credentials).
pub const CREDENTIAL_WITHDRAWAL: Duration = Duration::from_secs(300);

/// Which withdrawal behaviour applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingMode {
    /// Scoped withdrawal (the default).
    Scoped,
    /// The pre-W4-A behaviour (`LOOM_READ_ROUTING=legacy`).
    Legacy,
}

impl RoutingMode {
    /// The mode a [`READ_ROUTING_ENV`] value selects; anything but `legacy`
    /// (any case, surrounding space ignored) is [`RoutingMode::Scoped`].
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some(v) if v.trim().eq_ignore_ascii_case("legacy") => Self::Legacy,
            _ => Self::Scoped,
        }
    }

    /// The mode in effect now (read on every call).
    #[must_use]
    pub fn current() -> Self {
        Self::parse(std::env::var(READ_ROUTING_ENV).ok().as_deref())
    }
}

/// What a failed read says about the reader that served it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A rate limit of the reader's installation for this owner.
    RateLimited {
        /// The refused pool: the response's `x-ratelimit-resource`, else the
        /// pool the call statically spends.
        resource: Resource,
        /// The pool's reset, from the refusal's own `x-ratelimit-reset`.
        /// A secondary limit never withdraws until it (scoped mode).
        reset: Option<SystemTime>,
        /// A secondary (abuse/concurrency) limit, not an empty pool.
        secondary: bool,
        /// The refusal's `Retry-After`.
        retry_after: Option<Duration>,
    },
    /// The reader's token was refused (401, bad credentials).
    Credential,
    /// This repo is outside the reader's installation (404, "not accessible
    /// by integration"): withdraw it for this repo only.
    Coverage,
}

impl Failure {
    /// A primary rate limit of `resource` with no reset known.
    #[must_use]
    pub fn rate_limited(resource: Resource) -> Self {
        Self::RateLimited {
            resource,
            reset: None,
            secondary: false,
            retry_after: None,
        }
    }

    /// Whether the pre-W4-A classifier called this an App-wide failure
    /// (`Failure::App`): any rate limit or credential failure. Legacy mode
    /// withdraws App-wide exactly when this holds; the writer fallback in
    /// [`super::reader_then_writer`] still keys on it.
    #[must_use]
    pub fn is_app_wide(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Credential)
    }

    /// For a rate limit with no reset of its own, take `reset` (a caller
    /// that parsed the refusal's headers itself).
    #[must_use]
    pub fn with_reset(self, reset: Option<SystemTime>) -> Self {
        match self {
            Self::RateLimited {
                resource,
                reset: None,
                secondary,
                retry_after,
            } => Self::RateLimited {
                resource,
                reset,
                secondary,
                retry_after,
            },
            other => other,
        }
    }
}

/// `epoch` seconds as a [`SystemTime`] (`None` before the epoch).
#[must_use]
pub fn epoch_time(epoch: i64) -> Option<SystemTime> {
    u64::try_from(epoch)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

fn epoch_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Classify a failed read, or `None` when the failure is not the
/// credential's (a 5xx, a network error): retrying on the writer would not
/// help and withdrawing the reader would be wrong.
///
/// `headers` are the failed response's free `x-ratelimit-*` / `Retry-After`
/// headers when the call printed them (`--include`); `resource` is the pool
/// the call statically spends ([`crate::gh_invocation::accounting::static_pool`]),
/// used when the response did not name one.
#[must_use]
pub fn classify_failure(
    stderr: &str,
    http_status: Option<u16>,
    headers: Option<&RateLimitHeaders>,
    resource: Resource,
) -> Option<Failure> {
    classify_failure_in(RoutingMode::current(), stderr, http_status, headers, resource)
}

/// [`classify_failure`] under an explicit [`RoutingMode`].
#[must_use]
pub fn classify_failure_in(
    mode: RoutingMode,
    stderr: &str,
    http_status: Option<u16>,
    headers: Option<&RateLimitHeaders>,
    resource: Resource,
) -> Option<Failure> {
    let s = stderr.to_ascii_lowercase();
    let credential =
        s.contains("bad credentials") || http_status == Some(401) || s.contains("http 401");
    if mode == RoutingMode::Legacy {
        // The pre-W4-A rules, byte for byte: text and status only.
        if s.contains("rate limit")
            || credential
            || http_status == Some(429)
            || s.contains("http 429")
        {
            return Some(if credential {
                Failure::Credential
            } else {
                Failure::rate_limited(resource)
            });
        }
        return coverage(&s, http_status);
    }
    let retry_after = headers
        .and_then(|h| h.retry_after_secs)
        .map(Duration::from_secs);
    let secondary = crate::rate_limit_breaker::evidence::is_secondary_limit(stderr)
        || (matches!(http_status, Some(403 | 429)) && retry_after.is_some());
    let exhausted_403 = http_status == Some(403) && headers.is_some_and(|h| h.remaining == Some(0));
    // Rate limits come as 403 or 429 with a telling message, so check them
    // before the status: a rate-limited 403 is not coverage.
    if secondary
        || s.contains("rate limit")
        || http_status == Some(429)
        || s.contains("http 429")
        || exhausted_403
    {
        let resource = headers
            .and_then(|h| h.resource.as_deref())
            .and_then(Resource::parse)
            .unwrap_or(resource);
        return Some(Failure::RateLimited {
            resource,
            reset: headers.and_then(|h| h.reset_epoch).and_then(epoch_time),
            secondary,
            retry_after,
        });
    }
    if credential {
        return Some(Failure::Credential);
    }
    coverage(&s, http_status)
}

/// The coverage rule shared by both modes (`s` is lowercased stderr).
fn coverage(s: &str, http_status: Option<u16>) -> Option<Failure> {
    let uncovered = matches!(http_status, Some(403 | 404))
        || s.contains("resource not accessible by integration")
        // GraphQL (`gh pr view`, `gh issue view`) has no HTTP status for a
        // repo outside the installation: it says it cannot resolve the repo
        // (#9872 routes those reads too).
        || s.contains("could not resolve to a repository")
        || s.contains("http 403")
        || s.contains("http 404");
    uncovered.then_some(Failure::Coverage)
}

/// Where a scoped withdrawal's end came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetSource {
    /// The refusal's own headers (`x-ratelimit-reset` or `Retry-After`).
    Header,
    /// An on-demand `rate_limit` probe showing the bucket empty.
    Probe,
    /// No reset known: the fixed window for the failure kind.
    Default,
}

impl ResetSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Probe => "probe",
            Self::Default => "default",
        }
    }
}

/// The withdrawal a failure calls for ([`plan_withdrawal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Withdrawal {
    /// The whole App, every owner and resource (legacy mode, or a failure
    /// with no owner to scope to).
    AppWide { until: SystemTime },
    /// One `(app, owner, scope)` bucket.
    Scoped {
        /// Lowercased.
        owner: String,
        scope: ResourceScope,
        until: SystemTime,
        source: ResetSource,
        secondary: bool,
    },
    /// One repo outside the reader's installation.
    Repo { until: SystemTime },
}

/// A probe for `(app id, owner, resource)`'s reset: `Some(reset)` only when
/// the bucket reads empty with its window still open.
pub type ProbeReset<'a> = &'a dyn Fn(&str, &str, Resource) -> Option<SystemTime>;

/// The withdrawal `failure` on a read of `owner_repo` by reader `app_id`
/// calls for at `now` (pure but for `probe`, which is consulted only for a
/// primary rate limit whose refusal carried no reset).
#[must_use]
pub fn plan_withdrawal(
    owner_repo: &str,
    failure: Failure,
    now: SystemTime,
    mode: RoutingMode,
    app_id: &str,
    probe: ProbeReset<'_>,
) -> Withdrawal {
    let default_until = now + forge_read_pool::DEFAULT_WITHDRAWAL;
    if failure == Failure::Coverage {
        return Withdrawal::Repo {
            until: now + super::REPO_WITHDRAWAL,
        };
    }
    let owner = crate::credential_preflight::owner_of_nwo(owner_repo);
    if mode == RoutingMode::Legacy || owner.is_empty() {
        let until = match failure {
            Failure::RateLimited { reset: Some(t), .. } => t,
            _ => default_until,
        };
        return Withdrawal::AppWide { until };
    }
    let owner_lc = owner.to_ascii_lowercase();
    match failure {
        Failure::RateLimited {
            secondary: true,
            retry_after,
            ..
        } => Withdrawal::Scoped {
            owner: owner_lc,
            scope: ResourceScope::All,
            until: now + retry_after.map_or(SECONDARY_WITHDRAWAL, |d| d.min(MAX_SCOPED_WITHDRAWAL)),
            source: if retry_after.is_some() {
                ResetSource::Header
            } else {
                ResetSource::Default
            },
            secondary: true,
        },
        Failure::RateLimited {
            resource, reset, ..
        } => {
            let (raw, source) = match reset {
                Some(t) => (t, ResetSource::Header),
                None => match probe(app_id, owner, resource) {
                    Some(t) => (t, ResetSource::Probe),
                    None => (default_until, ResetSource::Default),
                },
            };
            let until = raw.clamp(now + MIN_SCOPED_WITHDRAWAL, now + MAX_SCOPED_WITHDRAWAL);
            Withdrawal::Scoped {
                owner: owner_lc,
                scope: ResourceScope::of(resource),
                until,
                source,
                secondary: false,
            }
        }
        Failure::Credential => Withdrawal::Scoped {
            owner: owner_lc,
            scope: ResourceScope::All,
            until: now + CREDENTIAL_WITHDRAWAL,
            source: ResetSource::Default,
            secondary: false,
        },
        Failure::Coverage => unreachable!("handled above"),
    }
}

/// The production [`ProbeReset`]: an on-demand probe of the reader's bucket
/// (throttled, [`forge_bucket_book::probe_one`]), then the book's reading.
#[must_use]
pub fn probed_reset(app_id: &str, owner: &str, resource: Resource) -> Option<SystemTime> {
    forge_bucket_book::probe_one(app_id, owner);
    book_reset(app_id, owner, resource, SystemTime::now())
}

/// [`probed_reset`] against an explicit workspace, `gh` and clock (tests).
#[must_use]
pub fn probed_reset_with(
    workspace_root: &Path,
    program: Option<&Path>,
    app_id: &str,
    owner: &str,
    resource: Resource,
    now: SystemTime,
) -> Option<SystemTime> {
    forge_bucket_book::probe_one_with(workspace_root, app_id, owner, program, now);
    book_reset(app_id, owner, resource, now)
}

/// The book's reset for reader `app_id`'s `(owner, resource)` bucket when it
/// reads empty. A bucket the book believes has budget is not the reason the
/// call was refused, so its reset is not trusted as the withdrawal's end.
fn book_reset(
    app_id: &str,
    owner: &str,
    resource: Resource,
    now: SystemTime,
) -> Option<SystemTime> {
    let account = crate::observability::ops::ratelimit::app_account_label(app_id);
    let reading =
        forge_bucket_book::reading(&BucketKey::new(&account, owner, resource), epoch_secs(now))?;
    (reading.remaining == Some(0))
        .then(|| epoch_time(reading.reset_epoch))
        .flatten()
}

/// Withdraw reader `app_id` after `failure` on a read of `owner_repo` (see
/// the module docs for the scope and end of each kind).
pub fn withdraw_after(app_id: &str, owner_repo: &str, failure: Failure, why: &str) {
    withdraw_after_in(
        RoutingMode::current(),
        app_id,
        owner_repo,
        failure,
        why,
        SystemTime::now(),
        &probed_reset,
    );
}

/// [`withdraw_after`] under an explicit mode, clock and probe; returns the
/// withdrawal it applied.
pub fn withdraw_after_in(
    mode: RoutingMode,
    app_id: &str,
    owner_repo: &str,
    failure: Failure,
    why: &str,
    now: SystemTime,
    probe: ProbeReset<'_>,
) -> Withdrawal {
    let plan = plan_withdrawal(owner_repo, failure, now, mode, app_id, probe);
    apply(app_id, owner_repo, failure, &plan, why);
    plan
}

/// Record `plan` in the routing tables, log it, and export it.
fn apply(app_id: &str, owner_repo: &str, failure: Failure, plan: &Withdrawal, why: &str) {
    let secs = |until: &SystemTime| {
        until
            .duration_since(SystemTime::now())
            .map_or(0, |d| d.as_secs())
    };
    match plan {
        Withdrawal::AppWide { until } => {
            // The pre-W4-A message, unchanged, so legacy mode logs as before.
            log::warn!(
                "forge_identity: reader app {app_id} withdrawn App-wide after a failed read \
                 ({why}); reads fall back to the next reader or the writer — #9537"
            );
            forge_read_pool::withdraw_until(app_id, *until);
            export(app_id, "-", "app", *until, ResetSource::Default, false);
        }
        Withdrawal::Scoped {
            owner,
            scope,
            until,
            source,
            secondary,
        } => {
            let held = forge_read_pool::withdraw_scoped_until(app_id, owner, *scope, *until);
            log::warn!(
                "forge_identity: reader app {app_id} withdrawn from {owner} {} for {}s \
                 (end from {}{}; {why}); its other owners and resources keep serving — W4-A",
                scope.as_str(),
                secs(&held),
                source.as_str(),
                if *secondary { ", secondary limit" } else { "" },
            );
            if let (
                Failure::RateLimited {
                    resource,
                    reset: Some(reset),
                    secondary: false,
                    ..
                },
                ResetSource::Header,
            ) = (failure, source)
            {
                let account = crate::observability::ops::ratelimit::app_account_label(app_id);
                forge_bucket_book::mark_exhausted(
                    BucketKey::new(&account, owner, resource),
                    epoch_secs(reset),
                );
            }
            export(app_id, owner, scope.as_str(), held, *source, *secondary);
        }
        Withdrawal::Repo { until } => {
            log::info!(
                "forge_identity: reader app {app_id} does not cover {owner_repo} ({why}); \
                 withdrawn for that repo for {}s, other repos unaffected — #9537",
                super::REPO_WITHDRAWAL.as_secs()
            );
            super::withdraw_reader_for_repo_until(app_id, owner_repo, *until);
        }
    }
}

fn export(
    app: &str,
    owner: &str,
    resource: &str,
    until: SystemTime,
    source: ResetSource,
    secondary: bool,
) {
    crate::observability::ops::reader_withdrawal::record_withdrawn(
        &crate::observability::ops::reader_withdrawal::Withdrawn {
            app,
            owner,
            resource,
            until: until.into(),
            source: source.as_str(),
            secondary,
        },
    );
}

#[cfg(test)]
#[path = "withdrawal_tests.rs"]
mod tests;
