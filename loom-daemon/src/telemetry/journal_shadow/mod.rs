//! J2 telemetry journal **shadow** streams (Epic #9908, Phase 2d, #11348).
//!
//! `docs/design/persistence-inventory.md` §9.3: the telemetry stores T1–T6
//! each gain one stream of the shared journal core ([`crate::journal`]), one
//! segment directory per stream (Q3 option 1):
//!
//! | Stream        | Inventory | Legacy store (still authoritative)                     |
//! |---------------|-----------|--------------------------------------------------------|
//! | `sweep`       | T1        | `.loom/logs/sweep-outcome-telemetry.jsonl`             |
//! | `role_tick`   | T2        | `.loom/logs/role-tick-telemetry.jsonl`                 |
//! | `rework`      | T3        | `.loom/logs/sweep-rework-events.jsonl`                 |
//! | `merge_queue` | T4        | `.loom/logs/merge-queue-events.jsonl`                  |
//! | `shell`       | T5        | `.loom/logs/{guide-docs,merge-admission}-telemetry.jsonl` |
//! | `experiment`  | T6        | `.loom/stats/sweep-model-stats.jsonl`                  |
//!
//! T7/T8 (`ci_telemetry`) are deliberately absent (Q4 option 2).
//!
//! # Contract (Phase 2: shadow only)
//!
//! - **Default off.** Nothing is written unless `LOOM_TELEMETRY_JOURNAL` is
//!   truthy ([`ENABLE_ENV`]); with it unset no journal directory is created.
//! - **Legacy first.** Every producer appends its shadow record only after its
//!   legacy write returned success ([`after_legacy`] is the one ordering
//!   primitive; the producers that cannot use a closure call [`shadow_append`]
//!   on their success path).
//! - **Never fatal.** A shadow failure (lock busy, oversize, I/O) is logged
//!   and swallowed; it can never turn a successful legacy operation into a
//!   failure. Legacy readers and exporters are untouched.
//! - **Data verbatim.** The record `data` is exactly what the legacy store
//!   holds (the `TelemetryEnvelope` for T1/T2/T5), so the backend wire schema
//!   does not change and [`compare`] can match the two sides.
//! - **Location.** `<workspace>/.loom/state/journal/telemetry/<stream>/`
//!   (§9.3; `.loom/state/` is ignored wholesale), overridable with an absolute
//!   [`ROOT_ENV`]. Telemetry is workspace-scoped today, so the root is too.
//! - **No credentials.** Producers pass the same records they already write to
//!   the legacy logs; the divergence report carries counts and fingerprints,
//!   never record content.

pub mod compare;
pub mod input;
pub mod verify;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests;

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::journal::{Journal, JournalError, JournalOptions};

/// Truthy (`1`/`true`/`yes`/`on`) enables shadow appends. Anything else,
/// including unset, leaves every producer exactly as it was.
pub const ENABLE_ENV: &str = "LOOM_TELEMETRY_JOURNAL";
/// Absolute override of the telemetry journal root. A relative value disables
/// shadowing rather than writing somewhere surprising.
pub const ROOT_ENV: &str = "LOOM_TELEMETRY_JOURNAL_ROOT";

/// The six J2 streams this slice shadows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TelemetryStream {
    Sweep,
    RoleTick,
    Rework,
    MergeQueue,
    Shell,
    Experiment,
}

impl TelemetryStream {
    pub const ALL: [TelemetryStream; 6] = [
        TelemetryStream::Sweep,
        TelemetryStream::RoleTick,
        TelemetryStream::Rework,
        TelemetryStream::MergeQueue,
        TelemetryStream::Shell,
        TelemetryStream::Experiment,
    ];

    /// The stream's directory name under the telemetry journal root.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            TelemetryStream::Sweep => "sweep",
            TelemetryStream::RoleTick => "role_tick",
            TelemetryStream::Rework => "rework",
            TelemetryStream::MergeQueue => "merge_queue",
            TelemetryStream::Shell => "shell",
            TelemetryStream::Experiment => "experiment",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Streams whose records are `TelemetryEnvelope`s.
    #[must_use]
    pub fn carries_envelope(self) -> bool {
        matches!(
            self,
            TelemetryStream::Sweep | TelemetryStream::RoleTick | TelemetryStream::Shell
        )
    }
}

/// A resolved, enabled shadow configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowConfig {
    pub root: PathBuf,
    pub options: JournalOptions,
}

impl ShadowConfig {
    /// An enabled configuration at `root` with the core's default bounds.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        ShadowConfig {
            root: root.into(),
            options: JournalOptions::default(),
        }
    }

    /// `<workspace>/.loom/state/journal/telemetry` (§9.3).
    #[must_use]
    pub fn default_root(workspace: &Path) -> PathBuf {
        workspace
            .join(".loom")
            .join("state")
            .join("journal")
            .join("telemetry")
    }

    /// The configuration in force for `workspace`, or `None` when shadowing
    /// is disabled (the default).
    #[must_use]
    pub fn resolve(workspace: &Path) -> Option<ShadowConfig> {
        #[cfg(test)]
        if let Some(forced) = tests::forced_root() {
            return forced.map(ShadowConfig::at);
        }
        let enable = std::env::var(ENABLE_ENV).ok();
        let root = std::env::var(ROOT_ENV).ok();
        Self::from_values(enable.as_deref(), root.as_deref(), workspace)
    }

    /// Pure resolution from the two env values.
    #[must_use]
    pub fn from_values(
        enable: Option<&str>,
        root: Option<&str>,
        workspace: &Path,
    ) -> Option<ShadowConfig> {
        let enabled = enable.is_some_and(|v| {
            matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        });
        if !enabled {
            return None;
        }
        match root.map(str::trim).filter(|r| !r.is_empty()) {
            None => Some(ShadowConfig::at(Self::default_root(workspace))),
            Some(r) if Path::new(r).is_absolute() => Some(ShadowConfig::at(r)),
            Some(r) => {
                log::warn!(
                    "telemetry journal: {ROOT_ENV}={r:?} is not absolute — shadow appends disabled"
                );
                None
            }
        }
    }

    #[must_use]
    pub fn journal(&self) -> Journal {
        Journal::with_options(&self.root, self.options)
    }

    /// Where durable divergence reports go: a sibling of the root
    /// (`…/telemetry-divergence/`), never one of the telemetry streams.
    #[must_use]
    pub fn report_root(&self) -> PathBuf {
        let name = self
            .root
            .file_name()
            .map_or_else(|| "telemetry".into(), |n| n.to_string_lossy().into_owned());
        self.root.with_file_name(format!("{name}-divergence"))
    }
}

/// One shadow record: the journal `kind`, an optional natural key and the
/// legacy record verbatim as `data`.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowRecord {
    pub kind: String,
    pub key: Option<String>,
    pub data: Value,
}

impl ShadowRecord {
    /// A `TelemetryEnvelope` (as JSON): kind is its `record.kind`.
    #[must_use]
    pub fn envelope(data: Value) -> Self {
        let kind = data
            .pointer("/record/kind")
            .and_then(Value::as_str)
            .unwrap_or("telemetry")
            .to_owned();
        ShadowRecord {
            kind,
            key: None,
            data,
        }
    }

    /// The shadow of a T1/T2 envelope: the envelope verbatim, as written.
    #[must_use]
    pub fn from_envelope(envelope: &super::TelemetryEnvelope) -> Option<Self> {
        Self::of("telemetry", None, envelope).map(|r| Self::envelope(r.data))
    }

    /// Serialize any legacy record; `None` (and a log line) if it cannot be.
    #[must_use]
    pub fn of<T: serde::Serialize>(kind: &str, key: Option<String>, record: &T) -> Option<Self> {
        match serde_json::to_value(record) {
            Ok(data) => Some(ShadowRecord {
                kind: kind.to_owned(),
                key,
                data,
            }),
            Err(e) => {
                log::warn!("telemetry journal: cannot encode {kind} shadow record: {e}");
                None
            }
        }
    }
}

/// What a shadow append did. Informational only — never an error to callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowOutcome {
    Disabled,
    Skipped,
    Appended { seq: u64 },
    Failed(String),
}

/// Append `record` to `stream` under `config`. The only write path.
///
/// # Errors
///
/// Any [`JournalError`] (busy lock, oversize entry, I/O).
pub fn append_with(
    config: &ShadowConfig,
    stream: TelemetryStream,
    record: ShadowRecord,
) -> Result<u64, JournalError> {
    let appended = config.journal().stream(stream.name())?.append(
        &record.kind,
        record.key.as_deref(),
        record.data,
    )?;
    Ok(appended.seq)
}

/// Shadow-append one record for `workspace`. **Call only after the legacy
/// write succeeded.** Builds the record lazily (nothing is serialized while
/// disabled) and never fails: errors are logged and reported in the outcome.
pub fn shadow_append(
    workspace: &Path,
    stream: TelemetryStream,
    record: impl FnOnce() -> Option<ShadowRecord>,
) -> ShadowOutcome {
    let Some(config) = ShadowConfig::resolve(workspace) else {
        return ShadowOutcome::Disabled;
    };
    let Some(record) = record() else {
        return ShadowOutcome::Skipped;
    };
    match append_with(&config, stream, record) {
        Ok(seq) => ShadowOutcome::Appended { seq },
        Err(e) => {
            log::warn!(
                "telemetry journal: {} shadow append under {} failed: {e} — legacy write \
                 already succeeded and stays authoritative (#11348)",
                stream.name(),
                config.root.display()
            );
            ShadowOutcome::Failed(e.to_string())
        }
    }
}

/// Run the `legacy` write; only when it returns `Ok` append the shadow record
/// built from its value. Returns the legacy result unchanged — a shadow
/// failure can never alter it.
///
/// # Errors
///
/// Exactly the legacy write's own error.
pub fn after_legacy<T, E>(
    workspace: &Path,
    stream: TelemetryStream,
    legacy: impl FnOnce() -> Result<T, E>,
    record: impl FnOnce(&T) -> Option<ShadowRecord>,
) -> Result<T, E> {
    let value = legacy()?;
    shadow_append(workspace, stream, || record(&value));
    Ok(value)
}

/// T6: shadow one `sweep-model-stats.jsonl` record already appended at
/// `stats_path` into the J2 `experiment` stream. Never fails.
pub fn shadow_experiment(stats_path: &Path, record: &Value) -> ShadowOutcome {
    let workspace = workspace_of_loom_file(stats_path);
    shadow_append(&workspace, TelemetryStream::Experiment, || {
        let phase = record
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("record");
        Some(ShadowRecord {
            kind: format!("experiment.{phase}"),
            key: None,
            data: record.clone(),
        })
    })
}

/// The workspace owning a legacy file `<ws>/.loom/<dir>/<file>`; for any
/// other path, the current directory (where the CLI producers run).
#[must_use]
pub fn workspace_of_loom_file(path: &Path) -> PathBuf {
    let ws = path
        .parent()
        .and_then(Path::parent)
        .filter(|loom| loom.file_name().is_some_and(|n| n == ".loom"))
        .and_then(Path::parent);
    match ws {
        Some(ws) if ws.as_os_str().is_empty() => PathBuf::from("."),
        Some(ws) => ws.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}
