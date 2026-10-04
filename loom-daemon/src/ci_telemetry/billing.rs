//! Billing / spending-limit "job was not started" detection (Issue #10113).
//!
//! When an org's GitHub Actions billing fails or its spending limit is hit,
//! GitHub stops starting hosted-runner jobs org-wide. Each affected job
//! concludes `failure` with **no runner and no steps**, and its check-run
//! annotation reads "The job was not started because recent account payments
//! have failed or your spending limit needs to be increased ...". To the fleet
//! that is indistinguishable from a red `main`, so dispatch halts quietly and
//! only a human org owner can fix it. This module:
//!
//! 1. classifies such a job ([`classify_job`]) so the job span carries
//!    `loom.ci.not_started_reason=billing` instead of reading as a plain
//!    failure;
//! 2. tracks which owners are billing-blocked ([`BillingRegistry`]) so the
//!    dispatch halt cause can name it ([`any_blocked`], [`owner_blocked`]);
//! 3. raises ONE deduplicated operator alert per owner per outage
//!    ([`BillingRegistry::observe`] + [`AlertSink`]).
//!
//! The match is deliberately specific to the billing message: any other
//! `failure` with no steps is NOT classified (a cancelled-before-start or
//! infrastructure failure must stay a plain failure).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use super::api::{ApiError, GithubApi};
use super::records::JobJson;

/// Span attribute naming why a job never started.
pub const NOT_STARTED_REASON_ATTR: &str = "loom.ci.not_started_reason";

/// How long after the last blocked job an owner still counts as blocked. The
/// alert dedup resets only once this lapses, so a continuing outage never
/// re-alerts, while the next distinct outage does.
pub const BLOCK_TTL_HOURS: i64 = 2;

/// Why a job never started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotStartedReason {
    /// Org billing failure or spending limit.
    Billing,
}

impl NotStartedReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NotStartedReason::Billing => "billing",
        }
    }
}

/// Whether a check-run annotation message is GitHub's billing / spending-limit
/// "not started" text.
#[must_use]
pub fn is_billing_message(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("spending limit")
        && (lowered.contains("job was not started")
            || lowered.contains("recent account payments have failed"))
}

#[derive(Debug, Deserialize)]
struct AnnotationJson {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// Classify one job. `None` for anything but a completed, failed,
/// runner-less, step-less job whose annotation is the billing message. Costs
/// one API call, and only for a job already shaped like a not-started one.
///
/// # Errors
/// Only a rate limit propagates (the cycle must back the whole org off); any
/// other failure reads the job as an ordinary failure.
pub fn classify_job(
    api: &dyn GithubApi,
    repo: &str,
    job: &JobJson,
    requests: &mut usize,
) -> Result<Option<NotStartedReason>, ApiError> {
    if !job.looks_not_started() {
        return Ok(None);
    }
    *requests += 1;
    let path = format!("repos/{repo}/check-runs/{}/annotations?per_page=10", job.id);
    let response = match api.get(&path, None) {
        Ok(response) => response,
        Err(error @ ApiError::RateLimited { .. }) => return Err(error),
        Err(_) => return Ok(None),
    };
    let annotations: Vec<AnnotationJson> = serde_json::from_str(&response.body).unwrap_or_default();
    let billing = annotations.iter().any(|a| {
        a.message.as_deref().is_some_and(is_billing_message)
            || a.title.as_deref().is_some_and(is_billing_message)
    });
    Ok(billing.then_some(NotStartedReason::Billing))
}

/// The first blocked job seen for an owner in the current outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInfo {
    pub owner: String,
    pub repo: String,
    pub run_id: u64,
    pub job_id: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    alerted: bool,
}

impl BlockInfo {
    #[must_use]
    pub fn run_url(&self) -> String {
        format!("https://github.com/{}/actions/runs/{}", self.repo, self.run_id)
    }

    /// Issue title; stable per owner so the duplicate backstop dedups it
    /// across daemon restarts.
    #[must_use]
    pub fn alert_title(&self) -> String {
        format!("GitHub Actions blocked by billing/spending limit for {}", self.owner)
    }

    #[must_use]
    pub fn alert_body(&self) -> String {
        format!(
            "GitHub is not starting hosted-runner jobs for the **{owner}** org: the job was not \
             started because recent account payments have failed or the spending limit needs to be \
             increased. Every job fails with no runner and no steps, so the fleet sees an ordinary \
             red `main` and halts dispatch (halt cause `ci_billing`).\n\n\
             - First blocked run: {url} (repo `{repo}`, job {job})\n\
             - First seen: {seen}\n\n\
             **Fix (org owner only):** GitHub -> {owner} -> Settings -> Billing & plans -> raise \
             the Actions spending limit / fix the payment method.\n\n\
             Filed once per outage by the ci-telemetry poller (#10113); it re-arms after \
             {ttl}h with no blocked jobs.",
            owner = self.owner,
            url = self.run_url(),
            repo = self.repo,
            job = self.job_id,
            seen = self.first_seen.to_rfc3339(),
            ttl = BLOCK_TTL_HOURS,
        )
    }
}

/// Where an operator alert goes. `true` = delivered (the dedup latch closes);
/// `false` = retry on the next observation.
pub trait AlertSink {
    fn alert(&self, info: &BlockInfo) -> bool;
}

/// Per-owner blocked state with alert dedup.
#[derive(Default)]
pub struct BillingRegistry {
    owners: Mutex<HashMap<String, BlockInfo>>,
}

impl BillingRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one blocked job. Fires `sink` at most once per outage per owner;
    /// returns `true` when this call delivered the alert.
    pub fn observe(
        &self,
        now: DateTime<Utc>,
        repo: &str,
        run_id: u64,
        job_id: u64,
        sink: &dyn AlertSink,
    ) -> bool {
        let owner = repo.split('/').next().unwrap_or(repo).to_ascii_lowercase();
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        if owners
            .get(&owner)
            .is_some_and(|b| now - b.last_seen > Duration::hours(BLOCK_TTL_HOURS))
        {
            owners.remove(&owner);
        }
        let info = owners.entry(owner.clone()).or_insert_with(|| BlockInfo {
            owner,
            repo: repo.to_string(),
            run_id,
            job_id,
            first_seen: now,
            last_seen: now,
            alerted: false,
        });
        info.last_seen = info.last_seen.max(now);
        if info.alerted {
            return false;
        }
        info.alerted = sink.alert(info);
        info.alerted
    }

    /// Whether `owner` (any case) has an active billing block at `now`.
    #[must_use]
    pub fn owner_blocked(&self, owner: &str, now: DateTime<Utc>) -> bool {
        self.owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&owner.to_ascii_lowercase())
            .is_some_and(|b| now - b.last_seen <= Duration::hours(BLOCK_TTL_HOURS))
    }

    /// Whether any owner has an active billing block at `now`.
    #[must_use]
    pub fn any_blocked(&self, now: DateTime<Utc>) -> bool {
        self.owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|b| now - b.last_seen <= Duration::hours(BLOCK_TTL_HOURS))
    }
}

fn global() -> &'static BillingRegistry {
    static REGISTRY: OnceLock<BillingRegistry> = OnceLock::new();
    REGISTRY.get_or_init(BillingRegistry::new)
}

/// Record a blocked job in the process-wide registry, alerting via a
/// `loom:operator` issue filed from `root`.
pub fn observe_global(root: &Path, repo: &str, run_id: u64, job_id: u64) {
    global().observe(Utc::now(), repo, run_id, job_id, &IssueSink { root });
}

/// Process-wide: is any owner billing-blocked right now?
#[must_use]
pub fn any_blocked() -> bool {
    global().any_blocked(Utc::now())
}

/// Process-wide: is `owner` billing-blocked right now?
#[must_use]
pub fn owner_blocked(owner: &str) -> bool {
    global().owner_blocked(owner, Utc::now())
}

/// Production sink: file a `loom:operator` issue via `create-issue.sh`. No
/// `--force`, so the script's duplicate backstop (exit 3: a similar open issue
/// exists) dedups across daemon restarts; that counts as delivered.
struct IssueSink<'a> {
    root: &'a Path,
}

impl AlertSink for IssueSink<'_> {
    fn alert(&self, info: &BlockInfo) -> bool {
        let Some(script) =
            crate::watchdog::escalate::resolve_issue_script(Some(self.root), self.root, None)
        else {
            log::error!(
                "ci_telemetry: GitHub Actions billing block for {} (first run {}) but no \
                 create-issue.sh to alert with",
                info.owner,
                info.run_url()
            );
            return false;
        };
        let mut cmd = std::process::Command::new(script);
        cmd.current_dir(self.root)
            .arg("--title")
            .arg(info.alert_title())
            .arg("--body")
            .arg(info.alert_body())
            .arg("--label")
            .arg("loom:operator");
        log::error!(
            "ci_telemetry: GitHub Actions jobs for {} are blocked by billing/spending limit \
             (first run {}); alerting operator",
            info.owner,
            info.run_url()
        );
        crate::sweep_registry::output_with_timeout(cmd, StdDuration::from_secs(60))
            .ok()
            .flatten()
            .is_some_and(|o| o.status.success() || o.status.code() == Some(3))
    }
}
