//! Quota burn and pool state (Issue #8857): is each subscription being used
//! around the clock, or sitting idle, or starved because every account in its
//! pool is exhausted?
//!
//! Sampled on the collector's snapshot cadence and exported through
//! `metric.points` (OTLP-only). Two signal families:
//!
//! # Token burn — `loom.llm.tokens.*`, `loom.llm.requests`
//!
//! Delta counters labelled `provider` + `model`, covering the interval since
//! the previous sample; TPM/RPM are rates over them. Host-wide, not per run:
//! a quota is spent by every session on the host (dispatched, role tick or
//! interactive), so every session counts. Per-execution attribution is #8908.
//!
//! Only the Claude transcript store is read today (#8930 extends this to the
//! Codex, OpenCode and Kimi stores, whose existing readers fold whole sessions
//! by *creation* time and so cannot give an interval rate). The fold:
//!
//! - Every `*.jsonl` under `${CLAUDE_CONFIG_DIR:-~/.claude}/projects/`
//!   (including `subagents/`) modified since the window start is read.
//! - Records are decoded by the shared
//!   [`usage_from_record`](crate::script_helpers::transcript_usage::usage_from_record)
//!   and grouped by `message.id` — streamed chunks restate cumulative usage,
//!   so each counter takes its maximum, exactly as
//!   `activity::transcript_parse` dedupes. The grouping is global across files,
//!   so a message copied into a resumed session counts once.
//! - A message counts in the window its **first** record falls in. The window
//!   ends [`MESSAGE_SETTLE_LAG_SECS`] before now so a message's later chunks
//!   are on disk before it is counted; consecutive windows abut, so every
//!   message is counted exactly once.
//! - One distinct message id is one API response: that is `loom.llm.requests`.
//! - `<synthetic>` records (Claude Code's internal echoes) are skipped.
//!
//! The first sample after the daemon starts only anchors the window: history
//! is never replayed as a burst.
//!
//! # Pool state — `loom.pool.*`
//!
//! Per provider, from the accounts `tokens.snapshot` already sampled (the
//! Claude `.ranking` pool and the Codex account registry) plus every enabled
//! API-key-pool account (Z.ai GLM, Kimi, …):
//!
//! - `loom.pool.accounts{state=usable|exhausted}` gauges.
//! - `loom.pool.exhausted` gauge: `1` when the pool has an exhausted account
//!   and no usable one — dispatch against that provider is starved.
//! - `loom.pool.exhaustions` delta: accounts that newly read exhausted since
//!   the previous sample (frequency of backoffs).
//! - `loom.pool.exhausted_seconds` delta: the interval since the previous
//!   sample, credited when the pool read exhausted at that previous sample
//!   (sample-and-hold). Summed over a day, this is the downtime.
//!
//! Only per-provider aggregates: no account label, which keeps cardinality
//! fixed. Per-account state is already `loom.tokens.exhausted`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Duration, Utc};

use crate::script_helpers::transcript_usage::usage_from_record;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::TokenAccountState;

/// `provider` label for the Claude transcript store — the same value
/// `tokens.snapshot` uses for the Claude pool.
pub const CLAUDE_PROVIDER: &str = "claude";

/// How far behind now the burn window ends, so a message's streamed chunks
/// are all written before it is counted.
pub const MESSAGE_SETTLE_LAG_SECS: i64 = 60;

/// `projects/<slug>/<session>/subagents/<agent>.jsonl` is three levels deep;
/// one spare level, and no unbounded walk.
const MAX_WALK_DEPTH: usize = 4;

/// Token and request totals for one model over one window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelBurn {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub requests: i64,
}

/// One API message, deduped across its streamed chunks.
#[derive(Debug, Clone)]
struct Message {
    first_at: DateTime<Utc>,
    model: String,
    usage: ModelBurn,
}

/// Messages keyed by `message.id` (or a per-line key when a record has none),
/// accumulated across every transcript read in one sample.
#[derive(Debug, Default)]
pub struct BurnFold {
    messages: HashMap<String, Message>,
    anonymous: usize,
}

impl BurnFold {
    /// Fold one transcript's lines. Unparseable lines, records without usage
    /// or a timestamp, and `<synthetic>` records are skipped.
    pub fn add_lines<'a>(&mut self, lines: impl IntoIterator<Item = &'a str>) {
        for line in lines {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(record) = usage_from_record(&value) else {
                continue;
            };
            if record.synthetic {
                continue;
            }
            let Some(at) = record
                .timestamp
                .as_deref()
                .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.with_timezone(&Utc))
            else {
                continue;
            };
            let key = record.message_id.clone().unwrap_or_else(|| {
                self.anonymous += 1;
                format!("__anonymous_{}", self.anonymous)
            });
            let usage = ModelBurn {
                input: record.input,
                output: record.output,
                cache_read: record.cache_read,
                cache_write: record.cache_write_5m.saturating_add(record.cache_write_1h),
                requests: 1,
            };
            match self.messages.get_mut(&key) {
                Some(message) => {
                    message.first_at = message.first_at.min(at);
                    let seen = &mut message.usage;
                    seen.input = seen.input.max(usage.input);
                    seen.output = seen.output.max(usage.output);
                    seen.cache_read = seen.cache_read.max(usage.cache_read);
                    seen.cache_write = seen.cache_write.max(usage.cache_write);
                }
                None => {
                    self.messages.insert(
                        key,
                        Message {
                            first_at: at,
                            model: record.model,
                            usage,
                        },
                    );
                }
            }
        }
    }

    /// Per-model totals of the messages whose first record falls in
    /// `(start, end]`.
    #[must_use]
    pub fn window(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> BTreeMap<String, ModelBurn> {
        let mut by_model: BTreeMap<String, ModelBurn> = BTreeMap::new();
        for message in self.messages.values() {
            if message.first_at <= start || message.first_at > end {
                continue;
            }
            let total = by_model.entry(message.model.clone()).or_default();
            total.input = total.input.saturating_add(message.usage.input);
            total.output = total.output.saturating_add(message.usage.output);
            total.cache_read = total.cache_read.saturating_add(message.usage.cache_read);
            total.cache_write = total.cache_write.saturating_add(message.usage.cache_write);
            total.requests = total.requests.saturating_add(message.usage.requests);
        }
        by_model
    }
}

/// Delta-counter points for one provider's per-model burn. Zero counters are
/// not emitted.
#[must_use]
pub fn burn_points(provider: &str, by_model: &BTreeMap<String, ModelBurn>) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    for (model, burn) in by_model {
        for (name, value) in [
            (MetricName::LlmTokensInput, burn.input),
            (MetricName::LlmTokensOutput, burn.output),
            (MetricName::LlmTokensCacheRead, burn.cache_read),
            (MetricName::LlmTokensCacheWrite, burn.cache_write),
            (MetricName::LlmRequests, burn.requests),
        ] {
            if value > 0 {
                points.push(
                    MetricPoint::int(name, value)
                        .label("provider", provider)
                        .label("model", model.as_str()),
                );
            }
        }
    }
    points
}

/// `*.jsonl` files under `dir` (depth-bounded, symlinks not followed)
/// modified at or after `since`.
fn transcripts_modified_since(dir: &Path, since: DateTime<Utc>) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, since: DateTime<Utc>, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if depth < MAX_WALK_DEPTH {
                    walk(&path, depth + 1, since, out);
                }
            } else if file_type.is_file()
                && path.extension().is_some_and(|ext| ext == "jsonl")
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|modified| DateTime::<Utc>::from(modified) >= since)
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 1, since, &mut out);
    out
}

/// Claude burn over `(start, end]`, read from `projects_dir`.
#[must_use]
pub fn claude_burn(
    projects_dir: &Path,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> BTreeMap<String, ModelBurn> {
    let mut fold = BurnFold::default();
    for path in transcripts_modified_since(projects_dir, start) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            fold.add_lines(contents.lines());
        }
    }
    fold.window(start, end)
}

/// One account's standing in its provider's pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolAccount {
    pub provider: String,
    pub account: String,
    pub usable: bool,
    pub exhausted: bool,
}

impl From<&TokenAccountState> for PoolAccount {
    fn from(state: &TokenAccountState) -> Self {
        PoolAccount {
            provider: state.provider.clone(),
            account: state.account.clone(),
            usable: !state.exhausted,
            exhausted: state.exhausted,
        }
    }
}

/// Every enabled API-key-pool account (Z.ai, Kimi, …). An unreadable pool is
/// unknown, so it contributes nothing rather than a fabricated empty pool.
fn api_key_accounts(workspace_root: &Path) -> Vec<PoolAccount> {
    use crate::api_keys_pool::Ineligible;
    crate::api_keys_pool::list_accounts(workspace_root, None)
        .unwrap_or_default()
        .into_iter()
        .filter(|account| account.enabled)
        .map(|account| PoolAccount {
            usable: matches!(account.ineligible, None | Some(Ineligible::AtCapacity)),
            exhausted: account.ineligible == Some(Ineligible::Exhausted),
            provider: account.provider,
            account: account.name,
        })
        .collect()
}

/// What the previous sample saw, so the next one can emit deltas.
#[derive(Debug, Default)]
pub struct QuotaState {
    /// End of the last burn window; `None` until the first sample anchors it.
    burn_until: Option<DateTime<Utc>>,
    /// When the previous pool sample was taken.
    last_pool_sample: Option<DateTime<Utc>>,
    /// Exhausted accounts per provider at the previous sample.
    exhausted: BTreeMap<String, BTreeSet<String>>,
    /// Providers whose pool read exhausted at the previous sample.
    pools_exhausted: BTreeSet<String>,
}

impl QuotaState {
    /// Advance the burn window to `now - lag` and return it, or `None` on
    /// the anchoring first sample (or when the clock went backwards).
    pub fn next_burn_window(
        &mut self,
        now: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let end = now - Duration::seconds(MESSAGE_SETTLE_LAG_SECS);
        let start = self.burn_until.replace(end)?;
        if end <= start {
            self.burn_until = Some(start);
            return None;
        }
        Some((start, end))
    }

    /// Pool gauges and deltas for `accounts` at `now`, advancing the state.
    /// The delta counters are only emitted once a previous sample exists.
    pub fn pool_points(
        &mut self,
        accounts: &[PoolAccount],
        now: DateTime<Utc>,
    ) -> Vec<MetricPoint> {
        let mut by_provider: BTreeMap<&str, (i64, BTreeSet<String>)> = BTreeMap::new();
        for account in accounts {
            let entry = by_provider.entry(account.provider.as_str()).or_default();
            if account.usable {
                entry.0 += 1;
            }
            if account.exhausted {
                entry.1.insert(account.account.clone());
            }
        }
        let elapsed = self
            .last_pool_sample
            .map(|last| (now - last).num_seconds().max(0));
        let mut points = Vec::new();
        let mut exhausted_now = BTreeMap::new();
        let mut pools_exhausted = BTreeSet::new();
        for (provider, (usable, exhausted)) in by_provider {
            let exhausted_count = i64::try_from(exhausted.len()).unwrap_or(i64::MAX);
            let pool_exhausted = exhausted_count > 0 && usable == 0;
            points.push(
                MetricPoint::int(MetricName::PoolAccounts, usable)
                    .label("provider", provider)
                    .label("state", "usable"),
            );
            points.push(
                MetricPoint::int(MetricName::PoolAccounts, exhausted_count)
                    .label("provider", provider)
                    .label("state", "exhausted"),
            );
            points.push(
                MetricPoint::int(MetricName::PoolExhausted, i64::from(pool_exhausted))
                    .label("provider", provider),
            );
            if let Some(elapsed) = elapsed {
                let previous = self.exhausted.get(provider);
                let newly = exhausted
                    .iter()
                    .filter(|name| previous.is_none_or(|prev| !prev.contains(*name)))
                    .count();
                if newly > 0 {
                    points.push(
                        MetricPoint::int(
                            MetricName::PoolExhaustions,
                            i64::try_from(newly).unwrap_or(i64::MAX),
                        )
                        .label("provider", provider),
                    );
                }
                if elapsed > 0 && self.pools_exhausted.contains(provider) {
                    points.push(
                        MetricPoint::int(MetricName::PoolExhaustedSeconds, elapsed)
                            .label("provider", provider),
                    );
                }
            }
            if pool_exhausted {
                pools_exhausted.insert(provider.to_string());
            }
            exhausted_now.insert(provider.to_string(), exhausted);
        }
        self.exhausted = exhausted_now;
        self.pools_exhausted = pools_exhausted;
        self.last_pool_sample = Some(now);
        points
    }
}

static STATE: Mutex<Option<QuotaState>> = Mutex::new(None);

/// One sample's two batches and the interval each one's deltas cover.
struct Sample {
    burn: Vec<MetricPoint>,
    burn_start: Option<DateTime<Utc>>,
    pool: Vec<MetricPoint>,
    pool_start: Option<DateTime<Utc>>,
}

fn sample(workspace_root: &Path, mut accounts: Vec<PoolAccount>, now: DateTime<Utc>) -> Sample {
    accounts.extend(api_key_accounts(workspace_root));
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let state = guard.get_or_insert_with(QuotaState::default);
    let pool_start = state.last_pool_sample;
    let pool = state.pool_points(&accounts, now);
    let (burn, burn_start) =
        match (state.next_burn_window(now), crate::transcript_tokens::claude_projects_dir()) {
            (Some((start, end)), Some(dir)) => {
                (burn_points(CLAUDE_PROVIDER, &claude_burn(&dir, start, end)), Some(start))
            }
            _ => (Vec::new(), None),
        };
    Sample {
        burn,
        burn_start,
        pool,
        pool_start,
    }
}

/// Sample and export, when an ops sink is registered. `accounts` is the
/// `tokens.snapshot` account list the collector already built this tick.
/// Returns before any read when no OTLP exporter is running.
pub async fn record(workspace_root: &Path, accounts: &[TokenAccountState]) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let root = workspace_root.to_path_buf();
    let accounts: Vec<PoolAccount> = accounts.iter().map(PoolAccount::from).collect();
    let now = Utc::now();
    if let Ok(sample) = tokio::task::spawn_blocking(move || sample(&root, accounts, now)).await {
        sink.emit_metrics_since(sample.burn, sample.burn_start);
        sink.emit_metrics_since(sample.pool, sample.pool_start);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "quota_tests.rs"]
mod tests;
