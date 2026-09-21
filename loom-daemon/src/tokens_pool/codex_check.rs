//! Per-subscription **availability** probe for Codex accounts (issue #8407) —
//! the `tokens check --ranking` analogue for the ChatGPT-plan pool.
//!
//! # The gap this closes
//!
//! Two Codex-side signals already existed, and neither answers the
//! selection-time question:
//!
//! * `codex login status` (#6927, [`super::account_lifecycle`]) is **auth
//!   validity only** — it cannot tell a fresh subscription from one prompt
//!   away from its weekly ceiling.
//! * [`super::health`]'s cooldowns are **failure-driven** — an account is only
//!   known-exhausted *after* a dispatch has already died on it.
//!
//! `tokens check --ranking` answers "which accounts can take work right now,
//! and for how long" for Claude by reading Anthropic's `x-ratelimit-*`
//! response headers. This module answers it for Codex.
//!
//! # The signal (research finding, AC1)
//!
//! A ChatGPT-plan Codex subscription **does** expose its two rate-limit
//! windows. The backend returns them on every `codex exec` turn, and the CLI
//! surfaces them as a `rate_limits` object carrying a `primary` (short,
//! ~5h) and a `secondary` (long, ~weekly) window, each with:
//!
//! ```text
//! used_percent      0..100, how much of that window is consumed
//! window_minutes    the window's length (300 ≈ 5h, 10080 ≈ 7d)
//! resets_in_seconds seconds from the observation until the window rolls over
//! ```
//!
//! Crucially, **the CLI persists every one of those snapshots to disk**: each
//! turn's event is appended to that profile's session rollout log under
//! `$CODEX_HOME/sessions/**/rollout-*.jsonl`. So the freshest reading of a
//! subscription's real headroom is already sitting in the account's own
//! `CODEX_HOME` — no extra billed API call, no `codex` process against the
//! profile, and no dependency on the account's session container being up.
//! That is what this module reads.
//!
//! Reading it is deliberately **not** an ADR-0017 ownership violation for a
//! session-managed (adopted) profile: this never runs `codex`, never opens
//! `auth.json`, and never writes inside the profile. It reads one append-only
//! log file the owning container itself wrote. The full reproduction recipe
//! and the verification status of this finding live in
//! `defaults/docs/token-pool.md` § "Codex availability probe".
//!
//! # Fail-open, always
//!
//! Every unknown is reported as "no evidence", never as a refusal. A profile
//! with no rollout log, an unparseable line, a schema this parser does not
//! recognize, or a snapshot whose own window has already rolled over all
//! produce `available` with **no** utilization — the account keeps its place
//! in selection. Proactive availability may only ever *add* information; a
//! probe that cannot read the signal must never take a working subscription
//! out of rotation.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};

use super::account_registry::{AccountDescriptor, AccountProvider};
use super::check::{self, AccountResult, ProbeReport, EXHAUSTED_THRESHOLD};
use super::health::{self, AccountHealth, AvailabilityOutcome, HealthReason};

/// Provider-namespaced ranking file name, written next to the provider-aware
/// `account-health.json` this pool already keys on. The **contents** are the
/// byte-identical `name|status|5h_util|limit_reset` lines Claude's `.ranking`
/// uses (see [`check::format_ranking_lines`]) so one reader — a dashboard, a
/// selector, the API-key pool of #8401 — parses every provider's ranking with
/// one parser.
pub const CODEX_RANKING_FILENAME: &str = "account-ranking.codex";

/// How much of a rollout log's tail is scanned for the newest `rate_limits`
/// snapshot. Rollout lines are small; 256 KiB covers many turns and bounds
/// the read on a long-lived session that has grown to tens of megabytes.
const MAX_TAIL_BYTES: u64 = 256 * 1024;

/// Bounds on the `sessions/` walk, so a pathological profile directory cannot
/// turn a probe into an unbounded filesystem crawl.
const MAX_WALK_ENTRIES: usize = 20_000;
const MAX_WALK_DEPTH: usize = 8;

/// Provenance recorded on any account-health record this module writes.
pub const AVAILABILITY_PROVENANCE: &str = "codex-availability-probe";

/// Where this workspace's Codex ranking file lives.
#[must_use]
pub fn ranking_path(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join(CODEX_RANKING_FILENAME)
}

// ---------------------------------------------------------------------------
// Signal extraction
// ---------------------------------------------------------------------------

/// One rate-limit window as the ChatGPT backend reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitWindow {
    /// `used_percent / 100` — the same 0..1 fraction Claude's `.ranking`
    /// carries, so [`EXHAUSTED_THRESHOLD`] means the same thing on both
    /// providers.
    pub used_fraction: f64,
    /// The window's own length, when reported (300 ≈ 5h, 10080 ≈ 7d).
    pub window_minutes: Option<u64>,
    /// Absolute instant this window rolls over, derived from the
    /// observation instant plus `resets_in_seconds`.
    pub resets_at: Option<DateTime<Utc>>,
}

impl RateLimitWindow {
    /// Whether this reading still describes the *current* window at `now`.
    ///
    /// A reading whose own reset instant has already passed is not evidence
    /// about today: the window it measured has since rolled over, so its
    /// utilization is discarded rather than carried forward. This is the one
    /// rule that keeps a week-old `100%` from pinning a healthy subscription
    /// out of rotation forever — the Codex analogue of #7420's overdue-reset
    /// trap, handled at read time instead of needing a re-probe.
    #[must_use]
    pub fn is_live_at(&self, now: DateTime<Utc>) -> bool {
        self.resets_at.is_none_or(|reset| reset > now)
    }
}

/// The freshest `rate_limits` reading found for one account.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    /// When the CLI recorded this reading (the rollout line's own timestamp,
    /// else the log file's mtime).
    pub observed_at: DateTime<Utc>,
    /// The short (~5h) window.
    pub primary: Option<RateLimitWindow>,
    /// The long (~weekly) window.
    pub secondary: Option<RateLimitWindow>,
}

/// What a measurement says is currently constraining an account, and until
/// when.
///
/// The long window dominates deliberately: a weekly ceiling is a days-long
/// outage (`exhausted`) while a full 5h window clears within hours
/// (`rate_limited`). This is exactly the distinction Claude's `.ranking`
/// already draws — `status_from_utilization` promotes a 429 to `exhausted`
/// only when the *7d* utilization clears the threshold — reused rather than
/// re-invented, so `check::limit_reset` picks the right horizon for each
/// status without a Codex-specific rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasuredConstraint {
    /// The long (~weekly) window is at or over the ceiling.
    Exhausted,
    /// The short (~5h) window is at or over the ceiling, but the weekly one
    /// is not.
    RateLimited,
}

impl MeasuredConstraint {
    /// The `.ranking` status word for this constraint.
    #[must_use]
    pub fn status(self) -> &'static str {
        match self {
            Self::Exhausted => "exhausted",
            Self::RateLimited => "rate_limited",
        }
    }
}

impl UsageSnapshot {
    /// The constraint this reading establishes at `now`, plus the instant it
    /// lifts — or `None` when both live windows have headroom.
    ///
    /// The single derivation both the reported status and the health hold go
    /// through, so a row can never claim one horizon while the hold it armed
    /// uses another.
    #[must_use]
    pub fn constraint_at(
        &self,
        now: DateTime<Utc>,
    ) -> Option<(MeasuredConstraint, Option<DateTime<Utc>>)> {
        let live = |window: Option<RateLimitWindow>| window.filter(|w| w.is_live_at(now));
        let at_ceiling = |window: Option<RateLimitWindow>| {
            live(window).filter(|w| w.used_fraction >= EXHAUSTED_THRESHOLD)
        };
        if let Some(secondary) = at_ceiling(self.secondary) {
            return Some((MeasuredConstraint::Exhausted, secondary.resets_at));
        }
        at_ceiling(self.primary).map(|primary| (MeasuredConstraint::RateLimited, primary.resets_at))
    }

    /// Whether either live window reports measurable headroom — the positive
    /// evidence that may release a hold.
    #[must_use]
    pub fn has_headroom_at(&self, now: DateTime<Utc>) -> bool {
        [self.primary, self.secondary]
            .into_iter()
            .flatten()
            .any(|window| window.is_live_at(now))
            && self.constraint_at(now).is_none()
    }
}

/// Recursively find the first `rate_limits` object in `value`.
///
/// Deliberately a search rather than a fixed path: the CLI has carried this
/// object at more than one nesting depth across versions (directly on the
/// `token_count` event, and inside its `info` payload), and a probe that
/// hard-codes one shape goes silently blind on the next CLI bump. Searching
/// for the key cannot misread a *different* object as this one — the parse
/// below still requires a recognizable window shape.
fn find_rate_limits(
    value: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Object(limits)) = map.get("rate_limits") {
                return Some(limits);
            }
            map.values().find_map(find_rate_limits)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_rate_limits),
        _ => None,
    }
}

/// Parse one window object. Returns `None` unless a usable `used_percent` is
/// present — a window with no utilization number is no evidence at all.
fn parse_window(value: &serde_json::Value, observed_at: DateTime<Utc>) -> Option<RateLimitWindow> {
    let map = value.as_object()?;
    let used_percent = map
        .get("used_percent")
        .and_then(serde_json::Value::as_f64)?;
    if !used_percent.is_finite() || used_percent < 0.0 {
        return None;
    }
    let window_minutes = map
        .get("window_minutes")
        .and_then(serde_json::Value::as_u64);
    let resets_at = map
        .get("resets_in_seconds")
        .and_then(serde_json::Value::as_i64)
        .and_then(|secs| chrono::Duration::try_seconds(secs).map(|d| observed_at + d))
        .or_else(|| {
            // Some vintages report the absolute instant instead of a
            // countdown. Accept either; never synthesize one from the other.
            map.get("resets_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                .map(|dt| dt.with_timezone(&Utc))
        });
    Some(RateLimitWindow {
        used_fraction: (used_percent / 100.0).min(1.0),
        window_minutes,
        resets_at,
    })
}

/// A rollout line's own timestamp, when it carries one.
fn line_timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    value
        .as_object()?
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Extract the **last** usable `rate_limits` reading from a rollout log's
/// text, or `None` when it carries none.
///
/// `fallback_observed_at` (the file's mtime) is used only when the winning
/// line has no parseable timestamp of its own. Unparseable lines are skipped
/// silently: a rollout log legitimately interleaves many event shapes, and a
/// JSON error on one line says nothing about the others.
#[must_use]
pub fn extract_usage_snapshot(
    text: &str,
    fallback_observed_at: DateTime<Utc>,
) -> Option<UsageSnapshot> {
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.contains("rate_limits") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(limits) = find_rate_limits(&value) else {
            continue;
        };
        let observed_at = line_timestamp(&value).unwrap_or(fallback_observed_at);
        let primary = limits
            .get("primary")
            .and_then(|w| parse_window(w, observed_at));
        let secondary = limits
            .get("secondary")
            .and_then(|w| parse_window(w, observed_at));
        if primary.is_none() && secondary.is_none() {
            continue;
        }
        return Some(UsageSnapshot {
            observed_at,
            primary,
            secondary,
        });
    }
    None
}

/// The newest `rollout-*.jsonl` under `<profile>/sessions/`, as
/// `(path, mtime)`.
fn newest_rollout_log(profile: &Path) -> Option<(PathBuf, DateTime<Utc>)> {
    let sessions = profile.join("sessions");
    let mut stack = vec![(sessions, 0usize)];
    let mut visited = 0usize;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_WALK_DEPTH || visited >= MAX_WALK_ENTRIES {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited >= MAX_WALK_ENTRIES {
                break;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(_, best_at)| modified > *best_at) {
                best = Some((path, modified));
            }
        }
    }
    best.map(|(path, modified)| (path, system_time_to_utc(modified)))
}

fn system_time_to_utc(at: std::time::SystemTime) -> DateTime<Utc> {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    Utc.timestamp_opt(secs, 0).single().unwrap_or_else(Utc::now)
}

/// Read the tail of `path` (at most [`MAX_TAIL_BYTES`]), dropping a leading
/// partial line so the caller only ever parses whole records.
fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.take(MAX_TAIL_BYTES).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start == 0 {
        return Some(text);
    }
    Some(
        text.split_once('\n')
            .map_or(String::new(), |(_, rest)| rest.to_string()),
    )
}

/// The freshest availability reading for one account's `CODEX_HOME`, or
/// `None` when the profile carries no usable one.
#[must_use]
pub fn latest_usage_snapshot(profile: &Path) -> Option<UsageSnapshot> {
    let (path, mtime) = newest_rollout_log(profile)?;
    let text = read_tail(&path)?;
    extract_usage_snapshot(&text, mtime)
}

// ---------------------------------------------------------------------------
// Assessment
// ---------------------------------------------------------------------------

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn epoch_to_utc(secs: u64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(i64::try_from(secs).ok()?, 0).single()
}

/// Map one account's inventory entry, stored health, and freshest usage
/// snapshot onto the shared [`AccountResult`] row.
///
/// Precedence is deliberately "known refusals first, measurement last": a
/// recorded hold is a fact about what selection will *do*, whereas a usage
/// reading is a fact about the subscription. When both exist the hold wins,
/// because reporting `available` for an account the selector will skip is the
/// dishonest direction.
#[must_use]
pub fn assess_account(
    account: &AccountDescriptor,
    health: Option<&AccountHealth>,
    snapshot: Option<&UsageSnapshot>,
    now: DateTime<Utc>,
) -> AccountResult {
    let name = account.id.name.as_str();
    if !account.enabled {
        let mut row = AccountResult::new(name, "skipped");
        row.error = Some("disabled".to_string());
        return row;
    }
    if health.is_some_and(|h| h.reason == HealthReason::ReauthRequired) {
        let mut row = AccountResult::new(name, "blocked");
        row.error = Some("reauth_required".to_string());
        return row;
    }

    let now_epoch = u64::try_from(now.timestamp()).unwrap_or(0);
    let mut row = AccountResult::new(name, "available");

    // Measurement first, so the utilization fields are populated even when a
    // hold below overrides the status.
    if let Some(snapshot) = snapshot {
        if let Some(primary) = snapshot.primary.filter(|w| w.is_live_at(now)) {
            row.s5h_utilization = Some(primary.used_fraction);
            row.s5h_reset = primary.resets_at.map(iso);
        }
        if let Some(secondary) = snapshot.secondary.filter(|w| w.is_live_at(now)) {
            row.s7d_utilization = Some(secondary.used_fraction);
            row.s7d_reset = secondary.resets_at.map(iso);
        }
        if let Some((constraint, _)) = snapshot.constraint_at(now) {
            row.status = constraint.status().to_string();
        }
    }

    // A live account-wide hold outranks the measurement in both directions:
    // it can promote `available` to a refusal, and it never lets a stale
    // measurement claim the account is resting longer than it is.
    if let Some(entry) = health {
        let held = entry
            .cooldown_until
            .filter(|until| *until > now_epoch)
            .or_else(|| {
                entry
                    .blocking_class_cooldown_at(now_epoch, None)
                    .map(|(_, until)| until)
            });
        if let Some(until) = held {
            let reset = epoch_to_utc(until).map(iso);
            match entry.reason {
                HealthReason::TransientFailure | HealthReason::SessionLimit => {
                    row.status = "rate_limited".to_string();
                    row.s5h_reset = reset;
                }
                _ => {
                    row.status = "exhausted".to_string();
                    row.s7d_reset = reset;
                }
            }
            row.error = Some(health_hold_label(entry.reason).to_string());
        }
    }
    row
}

/// Stable, secret-free label for a recorded hold. Never the stored
/// `signal_provenance` string, which is free-form caller text.
fn health_hold_label(reason: HealthReason) -> &'static str {
    match reason {
        HealthReason::Healthy => "cooldown",
        HealthReason::ReauthRequired => "reauth_required",
        HealthReason::PlanExhausted => "plan_exhausted",
        HealthReason::TransientFailure => "transient_backoff",
        HealthReason::SessionLimit => "session_limit",
        HealthReason::ModelCreditsExhausted => "model_credits_exhausted",
    }
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// What [`run_check`] did, beyond the report itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckEffects {
    /// Accounts newly held from selection by this probe.
    pub marked_exhausted: Vec<String>,
    /// Accounts whose exhaustion hold this probe released.
    pub cleared: Vec<String>,
    /// The ranking file written, when `--ranking` was requested.
    pub ranking_written: Option<PathBuf>,
}

/// Options for [`run_check`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions {
    /// Persist what the probe learned: write the provider-namespaced ranking
    /// file **and** feed each conclusive reading into account health, where
    /// selection already consults it. Off by default so a bare
    /// `accounts check` is a pure read.
    pub write_ranking: bool,
}

/// Probe every Codex account in `workspace`'s inventory.
///
/// Returns the shared [`ProbeReport`] shape `tokens check` produces, so
/// `--json`, the table renderer, and the `.ranking` writer are all reused
/// rather than re-implemented per provider.
pub fn run_check(
    workspace: &Path,
    options: CheckOptions,
    now: DateTime<Utc>,
) -> Result<(ProbeReport, CheckEffects)> {
    let inventory = super::account_registry::account_inventory(workspace, AccountProvider::Codex)?;
    let mut rows = Vec::with_capacity(inventory.len());
    let mut snapshots = Vec::with_capacity(inventory.len());
    for account in &inventory {
        let stored = health::account_health(workspace, &account.id)?;
        let snapshot = account
            .enabled
            .then(|| latest_usage_snapshot(&account.credential_reference))
            .flatten();
        rows.push(assess_account(account, stored.as_ref(), snapshot.as_ref(), now));
        snapshots.push(snapshot);
    }
    let report = check::build_report(rows);

    let mut effects = CheckEffects::default();
    if options.write_ranking {
        let path = ranking_path(workspace);
        check::write_ranking_atomic(&report, &path)?;
        effects.ranking_written = Some(path);
        apply_health_feedback(workspace, &inventory, &snapshots, now, &mut effects)?;
    }
    Ok((report, effects))
}

/// Feed each account's *measured* availability into the account-health state
/// selection already filters on (issue #8407 AC3).
///
/// Only a live measurement is conclusive, and only in the two directions a
/// measurement can actually establish: at/over the ceiling (hold until the
/// window rolls over) and demonstrably below it (release an exhaustion hold
/// this measurement post-dates). Everything else — no snapshot, a snapshot
/// whose window already rolled over, an account with no live reading — leaves
/// the record untouched, exactly as #6927's auth probe leaves it untouched
/// when it could not tell.
fn apply_health_feedback(
    workspace: &Path,
    inventory: &[AccountDescriptor],
    snapshots: &[Option<UsageSnapshot>],
    now: DateTime<Utc>,
    effects: &mut CheckEffects,
) -> Result<()> {
    let now_epoch = u64::try_from(now.timestamp()).unwrap_or(0);
    for (account, snapshot) in inventory.iter().zip(snapshots) {
        if !account.enabled {
            continue;
        }
        let Some(snapshot) = snapshot else { continue };
        let observed_at = u64::try_from(snapshot.observed_at.timestamp()).unwrap_or(0);
        // The *same* derivation the reported row goes through, so the hold's
        // deadline can never disagree with the horizon the row advertises.
        let outcome = match snapshot.constraint_at(now) {
            Some((_, resets_at)) => {
                let Some(until) = resets_at.and_then(|reset| u64::try_from(reset.timestamp()).ok())
                else {
                    // At the ceiling with no knowable reset: a hold with no
                    // deadline would have to be invented, and an invented
                    // deadline is worse than none. The row still reports the
                    // constraint; selection learns it the reactive way.
                    continue;
                };
                if until <= now_epoch {
                    continue;
                }
                AvailabilityOutcome::Exhausted { until }
            }
            None if snapshot.has_headroom_at(now) => AvailabilityOutcome::Available { observed_at },
            None => continue,
        };
        let effect = health::record_availability_at(
            workspace,
            &account.id,
            outcome,
            AVAILABILITY_PROVENANCE,
            now_epoch,
        )?;
        match effect {
            health::AvailabilityEffect::MarkedExhausted => {
                effects.marked_exhausted.push(account.id.name.clone());
            }
            health::AvailabilityEffect::ClearedExhaustionHold => {
                effects.cleared.push(account.id.name.clone());
            }
            health::AvailabilityEffect::Unchanged => {}
        }
    }
    Ok(())
}

/// Render the operator-facing table.
///
/// Deliberately **not** [`check::format_table`]: that renderer is labelled
/// "Token pool ranking" and its overdue-reset warning points at `tokens
/// unblock`, neither of which is true for a Codex account (whose recovery is
/// `accounts enable` / the session re-auth runbook). Same columns, honest
/// labels.
#[must_use]
pub fn format_table(report: &ProbeReport) -> String {
    let mut lines = vec![
        format!("Codex account availability (probed at {})", report.ranked_at),
        "=".repeat(84),
        format!(
            "{:<28} {:>9} {:>9} {:<13} {:<25}",
            "Account", "5h util", "7d util", "Status", "Resets at"
        ),
        "-".repeat(84),
    ];
    for account in &report.accounts {
        let util =
            |value: Option<f64>| value.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        let window = if account.status == "exhausted" {
            "7d"
        } else {
            "5h"
        };
        let reset = account
            .limit_reset()
            .map_or_else(|| "-".to_string(), |r| format!("{r} ({window})"));
        let mut row = format!(
            "{:<28} {:>9} {:>9} {:<13} {:<25}",
            account.name,
            util(account.s5h_utilization),
            util(account.s7d_utilization),
            account.status,
            reset
        );
        if let Some(error) = &account.error {
            row.push_str(&format!("  ({error})"));
        }
        lines.push(row);
    }
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for account in &report.accounts {
        *counts.entry(account.status.as_str()).or_insert(0) += 1;
    }
    let summary = counts
        .iter()
        .map(|(status, count)| format!("{count} {status}"))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(String::new());
    lines.push(format!("Total {}: {summary}", report.accounts.len()));
    lines.join("\n")
}

/// Whether any account in `report` is dispatchable **right now**. Drives
/// `accounts check`'s exit-code contract.
///
/// `available` only: every other status — `rate_limited` (the 5h window is
/// full), `exhausted`, `blocked`, `skipped`, `error` — names an account a
/// dispatch would bounce off at this instant. `rate_limited` is the
/// deliberate inclusion-looking exclusion: it self-clears within hours, which
/// makes it a *recoverable* refusal, not a usable account.
#[must_use]
pub fn has_usable_account(report: &ProbeReport) -> bool {
    report
        .accounts
        .iter()
        .any(|account| account.status == "available")
}

/// `(present, age_secs)` for this workspace's Codex ranking file — the codex
/// counterpart of `capacity::ranking_file_state`, used by `loom-daemon
/// health` to report how stale the last probe is.
#[must_use]
pub fn ranking_file_state(workspace: &Path) -> (bool, Option<u64>) {
    let path = ranking_path(workspace);
    let Ok(meta) = std::fs::metadata(&path) else {
        return (false, None);
    };
    let age = meta
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .map(|elapsed| elapsed.as_secs());
    (true, age)
}

#[cfg(test)]
#[path = "codex_check/tests.rs"]
mod tests;
