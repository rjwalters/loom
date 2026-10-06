//! `eta.snapshot` (#9329): one host's **live** ETA estimate set, for the
//! fleet dashboard.
//!
//! The estimate the dashboard shows and the estimate SigNoz scores are the
//! same number reached two different ways, so they are two record kinds:
//!
//! - [`EtaEstimateRecord`](super::eta::EtaEstimateRecord) is one *event* —
//!   one estimate, with its whole `eta-explanation/v1` record, emitted when
//!   the estimate is made and kept in SigNoz for accuracy scoring. It is
//!   OTLP-only.
//! - This kind is one *state* — every issue this host currently has an
//!   estimate for, as of now, with only the few scalars a list view needs.
//!   The full explanation is not carried: loom-ui fetches it on demand from
//!   SigNoz by `estimate_id` ("why this ETA?"), per operator decision 7 on
//!   #9289.
//!
//! **Native-HTTPS only**, exactly like [`queue.snapshot`](super::super::queue_snapshot)
//! and for the same reason: a host-scoped "newest wins" record is a dashboard
//! state key (`eta:<hostId>` in the `FleetState` Durable Object), not a time
//! series. SigNoz already has every estimate as `eta.estimate`, so an OTLP
//! queue would carry this record only to drop it.
//!
//! It is deliberately **not** folded into `queue.snapshot`: that record is the
//! work finder's *ready queue*, while `land` estimates cover building and
//! in-review items that are not in it at all.
//!
//! Anti-leak rules, enforced where the record is built
//! (`observability::eta_snapshot`) — the same three `queue.snapshot` holds:
//! - `repo` is the forge `owner/repo` slug and never a local path. The ETA
//!   tracker only ever keys items by slug, so a row cannot carry a path.
//! - Every row carries its own [`RepoVisibility`]; a missing or unknown tag
//!   decodes to `Private`.
//! - Every field is daemon-authored (an enum, a number, or a derived id).
//!   No forge free text — no title, no label text, no comment body — is
//!   carried at all.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eta::{Kind, NoEstimateReason, Stage};
use crate::telemetry::RepoVisibility;

/// Most rows one record carries. Rows past this are counted in
/// [`EtaSnapshotRecord::rows_truncated`] and
/// [`EtaSnapshotRecord::rows_truncated_by_kind`]; the cut is by priority
/// (`land` with `p50`, `land` refusals, then `start`/`finish`), not sort order.
///
/// Measured (#10052): a serialized row is ~245 bytes for a short slug (~350 with long slugs), so 200 rows is ~50-70 KB.
/// With `alternates` (#10390) a `land` row carrying 7 is ~1.5 KB, so a full
/// record is at most ~300 KB, well under the dashboard's 2 MiB value limit.
/// The record lands as one `eta:<hostId>` dashboard state value, so the cap
/// is held rather than raised: priority cutting, not a bigger record, is
/// what keeps every repo's `land` rows in.
pub const MAX_ROWS: usize = 200;

/// Most alternates one row carries; mirrors loom-ui's `MAX_ALTERNATES`
/// (`src/etaState.ts`), which slices before it filters.
pub const MAX_ALTERNATES: usize = 8;

/// One shadow heuristic's newest estimate (or refusal) for the same
/// `(repo, issue, kind)` as its row (#10390). Never the row's answer.
///
/// Quantiles are remaining seconds from this alternate's own `as_of`. Absent
/// is never zero: a refusal has all quantiles absent and
/// `no_estimate_reason` set. Wire shape mirrors loom-ui
/// `test/etaState.test.ts` "normalizeEtaSnapshot alternates (#1669)".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtaSnapshotAlternate {
    /// e.g. `land-2026-10-04-twin-otter`.
    pub heuristic: String,
    /// That heuristic's own `eta.estimate` id.
    pub estimate_id: String,
    /// The alternate's own `as_of` (may differ from the row's).
    pub as_of: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p25: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p75: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_estimate_reason: Option<NoEstimateReason>,
}

/// One issue's current estimate for one [`Kind`].
///
/// The field set is operator decision 7 on #9289 verbatim — `repo`, `issue`,
/// `pr`, `kind`, `p25`/`p50`/`p75`, `heuristic`, `estimate_id`, `as_of`,
/// `stage`, `no_estimate_reason` — plus the `visibility` tag every per-repo
/// row carries so the dashboard's redaction layer can act on it.
///
/// **Absent is never zero.** A refusal (no history yet, a hold label, a
/// human-gated stage) is a row with `p25`/`p50`/`p75` all absent and
/// `no_estimate_reason` set: that an issue *cannot* be estimated, and why, is
/// itself the answer, so refusals are carried rather than dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtaSnapshotRow {
    /// Forge `owner/repo`.
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// Issue number.
    pub issue: u32,
    /// The PR this issue's work is in, when one is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u32>,
    /// What is predicted: `start`, `finish` or `land`.
    pub kind: Kind,
    /// Remaining seconds, 25th percentile. Absent on a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p25: Option<i64>,
    /// Remaining seconds, median.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<i64>,
    /// Remaining seconds, 75th percentile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p75: Option<i64>,
    /// The heuristic that made it (`land-v1`, …). Always the kind's
    /// **`current`** heuristic: a shadow candidate's estimate (#9328) is
    /// never the subject's answer and appears only under `alternates`.
    pub heuristic: String,
    /// The derived id of the estimate, for the on-demand "why this ETA?"
    /// lookup of the full `eta-explanation/v1` record in SigNoz.
    pub estimate_id: String,
    /// The instant the estimate describes — its freshness stamp.
    pub as_of: DateTime<Utc>,
    /// The stage the item was in. Absent when it had none (a refusal with no
    /// resolvable stage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<Stage>,
    /// Why there is no estimate. Present exactly when `p25`/`p50`/`p75` are
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_estimate_reason: Option<NoEstimateReason>,
    /// Shadow candidates' estimates for this item (#10390). Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternates: Vec<EtaSnapshotAlternate>,
}

/// `eta.snapshot`: one host's live ETA estimate set. Host-scoped, newest per
/// host (the `eta:<hostId>` key), each row carrying its own repo and
/// visibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtaSnapshotRecord {
    /// The newest `as_of` among [`Self::rows`] — the freshness stamp. A
    /// snapshot is emitted only when the estimate set changed, so an ageing
    /// `as_of` means the tracker has stopped producing new estimates, not
    /// that the exporter stalled.
    pub as_of: DateTime<Utc>,
    /// One row per `(repo, issue, kind)` this host currently estimates, in
    /// `(repo, issue, kind)` order. At most [`MAX_ROWS`], chosen by priority.
    pub rows: Vec<EtaSnapshotRow>,
    /// Rows dropped by the [`MAX_ROWS`] cap.
    #[serde(default)]
    pub rows_truncated: usize,
    /// [`Self::rows_truncated`] broken down by [`Kind`], so a truncation that
    /// dropped `land` rows is visible. Absent when nothing was dropped and
    /// from older daemons.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rows_truncated_by_kind: BTreeMap<Kind, usize>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "eta_snapshot_tests.rs"]
mod tests;
