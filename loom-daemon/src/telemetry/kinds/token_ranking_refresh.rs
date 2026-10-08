//! `token_ranking.refresh` (#10744): one record per workspace per round of the
//! daemon's token-ranking refresh loop.
//!
//! The loop is default-on and, every 600 s on every daemon host, can send a
//! `max_tokens: 1` `POST /v1/messages` to Anthropic for each pool account. On
//! an OAuth subscription token that probe is free; on an API key it is metered
//! spend that goes direct to the provider, outside any gateway. Before this
//! kind nothing recorded it: the loop's only output was the `.ranking` file and
//! log lines. This record names, per round, which accounts were probed, what
//! each came back as, and whether any probe used an API-key credential.
//!
//! **OTLP only**, like `auto_update.tick`. The round-level scalars ride as
//! `loom.token_ranking.*` attributes ([`TOKEN_RANKING_LOG_ATTRIBUTE_KEYS`],
//! which the collector's log `keep_keys` must list; contract-tested). The body
//! is the record's JSON, which carries the per-account entries.
//!
//! **No credential material, ever.** An account appears by its pool name only.
//! The credential kind is derived from the token's prefix inside the probing
//! process and only the closed-set label ([`CredentialKind`]) leaves it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eta::Provenance;

/// Every log attribute key `token_ranking.refresh` exports. The collector's
/// `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested).
pub const TOKEN_RANKING_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.token_ranking.round_id",
    "loom.token_ranking.workspace",
    "loom.token_ranking.outcome",
    "loom.token_ranking.failure_class",
    "loom.token_ranking.source",
    "loom.token_ranking.skipped_fresh",
    "loom.token_ranking.account_count",
    "loom.token_ranking.probed_count",
    "loom.token_ranking.api_key_probe_count",
    "loom.token_ranking.ok_count",
    "loom.token_ranking.rate_limited_count",
    "loom.token_ranking.auth_dead_count",
    "loom.token_ranking.skipped_fresh_count",
    "loom.token_ranking.error_count",
    "loom.token_ranking.unsupported_count",
    "loom.token_ranking.duration_ms",
    "loom.token_ranking.version",
    "loom.token_ranking.revision",
    "loom.token_ranking.tree_state",
    "loom.token_ranking.provenance_complete",
];

/// How one workspace's round ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundOutcome {
    /// The `tokens check --ranking` child ran and exited 0.
    Success,
    /// The child could not be spawned, timed out, exited non-zero, or the
    /// blocking task panicked. The loop logs it and keeps going.
    Failure,
    /// The workspace has the loop turned off
    /// (`autonomous.tokenRankingRefresh.enabled=false` or the env override):
    /// nothing ran and nothing was probed.
    Disabled,
}

impl RoundOutcome {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Disabled => "disabled",
        }
    }
}

/// Where the round's ranking came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankingSource {
    /// A fresh claude-monitor `ranking.json` served the ranking. Only overdue
    /// rows were re-probed (#7420); every other account was not contacted.
    Monitor,
    /// The pool was probed account by account.
    Probe,
    /// Not known: the child failed before reporting, the workspace is
    /// disabled, or the child binary predates the round summary.
    Unknown,
}

impl RankingSource {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Probe => "probe",
            Self::Unknown => "unknown",
        }
    }
}

/// One account's outcome this round. A closed set, so dashboards can group on
/// it. The raw `tokens check` status rides beside it in
/// [`TokenRankingAccount::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountOutcome {
    /// `available`.
    Ok,
    /// `rate_limited` or `exhausted`.
    RateLimited,
    /// `blocked`: a 401, a known auth-dead `.bad_tokens` entry, or a
    /// credential that does not have a Claude shape.
    AuthDead,
    /// A fresh claude-monitor row was used as-is; no request was sent.
    SkippedFresh,
    /// The probe errored (timeout, connection, non-401/429 HTTP status).
    Error,
    /// The provider has no probe adapter (`no_probe_adapter:<provider>`).
    Unsupported,
}

impl AccountOutcome {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::RateLimited => "rate_limited",
            Self::AuthDead => "auth_dead",
            Self::SkippedFresh => "skipped_fresh",
            Self::Error => "error",
            Self::Unsupported => "unsupported",
        }
    }

    /// Map a `tokens check` account status onto the closed outcome set.
    #[must_use]
    pub fn from_status(status: &str) -> Self {
        match status {
            "available" => Self::Ok,
            "rate_limited" | "exhausted" => Self::RateLimited,
            "blocked" => Self::AuthDead,
            "skipped" => Self::SkippedFresh,
            "unsupported" => Self::Unsupported,
            _ => Self::Error,
        }
    }
}

/// The kind of credential an account holds, from its token's prefix only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A Claude OAuth subscription token (`sk-ant-oat…`, sent as `Bearer`).
    /// Probing it is free.
    Oauth,
    /// Any other `sk-ant-…` key, sent as `x-api-key`. Probing it is metered.
    ApiKey,
    /// Not a Claude credential shape, a non-Claude provider, or unreadable.
    /// Never probed by `tokens check`.
    Unknown,
}

impl CredentialKind {
    /// Classify a token by prefix. The token itself never leaves this call.
    #[must_use]
    pub fn of_token(token: &str) -> Self {
        if token.starts_with("sk-ant-oat") {
            Self::Oauth
        } else if token.starts_with("sk-ant-") {
            Self::ApiKey
        } else {
            Self::Unknown
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::ApiKey => "api_key",
            Self::Unknown => "unknown",
        }
    }
}

/// One pool account in one round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRankingAccount {
    /// The pool account name (the `.token` file stem). Never a token value.
    pub account: String,
    /// `claude` / `codex`.
    pub provider: String,
    /// The raw `tokens check` status (`available`, `exhausted`, …).
    pub status: String,
    /// The closed-set outcome.
    pub outcome: AccountOutcome,
    /// The credential kind the account holds.
    pub credential_kind: CredentialKind,
    /// A request was sent to the provider for this account this round.
    pub probed: bool,
}

/// One workspace's refresh round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRankingRefreshRecord {
    /// Derived, never random: `derived_hex(["loom.token_ranking.refresh",
    /// host, workspace, started_at], 32)`.
    pub round_id: String,
    /// When the round started; the record's time.
    pub started_at: DateTime<Utc>,
    /// The final path component of the workspace root the round refreshed
    /// (`loom`, never `/home/alice/GitHub/loom`): an absolute path embeds the
    /// host's user name. See `token_ranking_refresh::telemetry::workspace_label`.
    pub workspace: String,
    /// How the round ended.
    pub outcome: RoundOutcome,
    /// For a failure: `spawn_error` / `timeout` / `nonzero_exit` /
    /// `poll_error` / `panic` / `error`. Never the child's output, which is
    /// free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    /// Where the ranking came from.
    pub source: RankingSource,
    /// Requests sent to the provider this round.
    pub probed_count: u32,
    /// Of those, requests that used an API-key credential (metered spend).
    pub api_key_probe_count: u32,
    /// Every account the round reported on.
    pub accounts: Vec<TokenRankingAccount>,
    /// Wall time the round took, milliseconds.
    pub duration_ms: u64,
    /// The running build.
    pub loom: Provenance,
}

impl TokenRankingRefreshRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }

    /// A fresh claude-monitor ranking served this round.
    #[must_use]
    pub fn skipped_fresh(&self) -> bool {
        self.source == RankingSource::Monitor
    }

    /// How many accounts came back as `outcome`.
    #[must_use]
    pub fn count(&self, outcome: AccountOutcome) -> usize {
        self.accounts
            .iter()
            .filter(|a| a.outcome == outcome)
            .count()
    }
}
