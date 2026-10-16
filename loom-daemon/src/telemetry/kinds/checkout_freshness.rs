//! `checkout.freshness` (#12509): how far one managed checkout is behind
//! GitHub's tip, emitted by the daemon's checkout fast-forward pass.
//!
//! Role ticks already record the prompt tree they launch from
//! (`role_tick.outcome`'s `loom.prompt.*`, #11989), but a checkout with no role
//! tick (an AWS-box daemon workspace, a sweep worker's clone) reported nothing,
//! so "how often does a daemon host run from a stale or divergent checkout"
//! had no answer. The pass visits every managed checkout, so it emits one of
//! these per checkout per pass.
//!
//! **OTLP-only** (`otlp: Logs`, `native: false`), and only when the exporter is
//! configured. `host` rides on the Resource as `host.name`/`host.id`. Every
//! fact is read locally: the remote tip is the one the pass already learned
//! (`checkout_ff::heard`), so the record costs no network call and writes
//! nothing to `.git`. **No credential or URL can reach it**: the only remote
//! fact is `owner/repo`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::role_tick_telemetry::prompt_tree::PromptTree;

/// Every log attribute key `checkout.freshness` exports beyond `loom.kind`.
/// The collector's log `keep_keys` must list each (contract-tested).
pub const CHECKOUT_FRESHNESS_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.repo",
    "loom.pass_skipped_reason",
    "loom.prompt.commit",
    "loom.prompt.loom_version",
    "loom.prompt.commits_behind",
    "loom.prompt.on_branch",
    "loom.prompt.remote_tip",
    "loom.prompt.commits_behind_remote",
    "loom.prompt.origin_ref_stale",
    "loom.prompt.remote_repo",
    "loom.prompt.remote_foreign",
];

/// The `checkout.freshness` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFreshnessRecord {
    /// When the checkout was read; the record time.
    pub captured_at: DateTime<Utc>,
    /// The workspace's `owner/repo`; absent when `origin` is foreign or
    /// unparseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Why this pass could not measure the checkout against GitHub's tip
    /// (`budget`, `low-disk`, `breaker-open`, `backing-off`, `network-off`,
    /// `unreachable-stop`, `no-head`), so a stale count never looks fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_skipped_reason: Option<String>,
    /// The checkout's prompt tree; absent when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptTree>,
}
