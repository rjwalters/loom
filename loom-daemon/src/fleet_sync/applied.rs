//! The applied fleet-config SHA (Issue #11197): which fleet-gitops commit's
//! files are **on disk on this host right now**.
//!
//! [`publish`](super::publish) funnels every pass here; the value is held in
//! a process-wide cell and read at export time by the OTLP resource builder
//! ([`resource_for_host`](crate::observability::otlp::mapping::resource_for_host)),
//! so a timer pass that applies a new commit shows up on the next export
//! batch with no restart and no cache invalidation.
//!
//! # Applied, not merely read
//!
//! [`ConfigPass::commit`](super::ConfigPass::commit) names the commit a pass
//! *read*, not the one it *applied*: it is set even when a tier drifted and
//! was not written (`fleet.autoApply` off, the lossy-reduction guard, or a
//! write error), and `None` only when the fetch itself failed. The cell here
//! therefore advances **only** on a pass with `error == None` and every tier
//! either in sync or written — a later failed or drifted-unwritten pass
//! leaves the previous value alone, because the files on disk still hold
//! that earlier commit.
//!
//! # The three states, and why the attribute is never empty
//!
//! In SigNoz/ClickHouse `resources_string['loom.fleet.config_sha']` reads as
//! `''` for a **missing** key, so an omitted attribute is indistinguishable
//! from an empty one and every record exported before the SHA is known would
//! count against an "empty config_sha" criterion with no way to tell why.
//! The attribute is therefore always present and never empty — a 40-hex SHA,
//! `unknown` (a fleet host where no applied commit is established yet) or
//! `none` (no fleet store, `fleet.repo` unset). A non-hex sentinel cannot be
//! mistaken for a SHA; the real signal (a host stuck on `unknown`, or a
//! roster host showing `none`) queries as `= 'unknown'` instead of hiding
//! inside an empty count.
//!
//! # Startup-timeout window
//!
//! The startup pass publishes before the OTLP exporter is spawned, so the
//! cell is normally warm before the first export. The exception is a startup
//! pass that hits its wall-clock cap: nothing is published until the first
//! timer pass. [`applied_config_sha`] covers that window by seeding from the
//! host-level snapshot file (`fleet-sync-status.json`) when the cell is
//! empty — and only when the stored pass satisfies the same applied
//! predicate, since a stored render that partly failed does not describe the
//! files on disk.

use std::sync::{Mutex, OnceLock};

use super::FleetSyncStatus;

/// OTLP resource attribute key carrying the applied fleet-gitops SHA on
/// every signal (logs, metrics, traces, relayed agent telemetry).
pub const FLEET_CONFIG_SHA: &str = "loom.fleet.config_sha";

/// The applied fleet-config SHA, as the OTLP resource reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedSha {
    /// The 40-hex commit whose rendered tiers are on disk.
    Sha(String),
    /// A fleet host where no applied commit is established yet.
    Unknown,
    /// No fleet store is configured (`fleet.repo` unset).
    NoStore,
}

impl AppliedSha {
    /// The resource attribute value: the SHA itself, or the literal
    /// `unknown` / `none` sentinels — never empty (see the module doc).
    #[must_use]
    pub fn as_resource_value(&self) -> &str {
        match self {
            AppliedSha::Sha(sha) => sha,
            AppliedSha::Unknown => "unknown",
            AppliedSha::NoStore => "none",
        }
    }
}

/// `None` = nothing established in this process yet (try the status file);
/// `Some(state)` = the last word from `publish`/`clear_status`.
fn cell() -> &'static Mutex<Option<AppliedSha>> {
    static CELL: OnceLock<Mutex<Option<AppliedSha>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// The applied fleet-config SHA for **this host right now**.
///
/// Reads the process-wide cell; when it is empty (startup-timeout window —
/// see the module doc) seeds from the host-level snapshot file, memoizing a
/// successful seed so the file is not re-read per export batch.
#[must_use]
pub fn applied_config_sha() -> AppliedSha {
    if let Ok(guard) = cell().lock() {
        if let Some(held) = guard.as_ref() {
            return held.clone();
        }
    }
    let probed = super::probe_status();
    let seeded = probed.as_ref().and_then(applied_sha_of).map(str::to_string);
    if let (Ok(mut guard), Some(sha)) = (cell().lock(), seeded.clone()) {
        *guard = Some(AppliedSha::Sha(sha));
    }
    seeded.map_or(AppliedSha::Unknown, AppliedSha::Sha)
}

/// The commit of `status`'s config pass, when that pass **applied** cleanly:
/// `error == None`, every tier either in sync or written, and a commit read.
/// Anything else leaves the caller's previous value alone.
fn applied_sha_of(status: &FleetSyncStatus) -> Option<&str> {
    if status.config.error.is_some() {
        return None;
    }
    if status.config.tiers.iter().any(|t| t.drifted && !t.wrote) {
        return None;
    }
    status.config.commit.as_deref().filter(|c| !c.is_empty())
}

/// Record a finished pass (called from [`publish`](super::publish)). Advances
/// the cell only on an applied pass; never clears or regresses it.
pub(crate) fn record_pass(status: &FleetSyncStatus) {
    let Some(sha) = applied_sha_of(status) else {
        return;
    };
    if let Ok(mut guard) = cell().lock() {
        *guard = Some(AppliedSha::Sha(sha.to_string()));
    }
}

/// Forget the store (called from [`clear_status`](super::clear_status), i.e.
/// `fleet.repo` unset): the resource reports `none` from here on.
pub(crate) fn clear() {
    if let Ok(mut guard) = cell().lock() {
        *guard = Some(AppliedSha::NoStore);
    }
}

/// Test seam for the sibling `applied` module's tests and the OTLP resource
/// tests: force or empty the process-wide cell (#11197). Never call outside
/// `#[cfg(test)]` code — the honest writers are [`record_pass`] and [`clear`].
#[cfg(all(test, feature = "otlp"))]
pub(crate) fn force_for_tests(sha: AppliedSha) {
    *cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sha);
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_sync::tests::{sample_status, with_clean_process_state};
    use crate::fleet_sync::{clear_status, publish, TierReport};
    use serial_test::serial;
    use std::path::Path;

    /// The cell is process-wide: every test that touches it serializes on
    /// the same key as the other fleet_sync process-state tests, and resets
    /// the cell (plus the shared loom dir) on the way in and out.
    fn with_clean_cell<T>(f: impl FnOnce() -> T) -> T {
        with_clean_process_state(|_| {
            reset_for_tests();
            let out = f();
            reset_for_tests();
            out
        })
    }

    /// One tier report: `InSync`, or `Wrote` when `wrote`, or drifted-and-
    /// unwritten (the autoApply-off shape) when neither.
    fn tier(wrote: bool, drifted: bool) -> TierReport {
        TierReport {
            tier: "machine tier".to_string(),
            path: Path::new("/etc/loom/defaults.json").to_path_buf(),
            drifted: drifted && !wrote,
            wrote,
            detail: None,
        }
    }

    fn pass(commit: &str, error: Option<&str>, tiers: Vec<TierReport>) -> FleetSyncStatus {
        let mut status = sample_status();
        status.config.commit = Some(commit.to_string());
        status.config.error = error.map(str::to_string);
        status.config.tiers = tiers;
        status
    }

    /// Write `status` to the snapshot file only (not the cell) — the shape a
    /// fresh process finds on disk after a startup pass hit its time cap.
    fn store_status(status: &FleetSyncStatus) {
        super::super::write_status(&super::super::status_path().expect("status path"), status)
            .expect("write");
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn an_applied_pass_sets_the_sha() {
        with_clean_cell(|| {
            publish(&pass(
                "0123456789abcdef0123456789abcdef01234567",
                None,
                vec![tier(false, false)],
            ));
            publish(&pass(
                "0123456789abcdef0123456789abcdef01234567",
                None,
                vec![tier(true, true)],
            ));
            assert_eq!(
                applied_config_sha(),
                AppliedSha::Sha("0123456789abcdef0123456789abcdef01234567".to_string()),
                "in-sync and written tiers are both applied"
            );
            assert_eq!(
                applied_config_sha().as_resource_value(),
                "0123456789abcdef0123456789abcdef01234567"
            );
        });
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn a_failed_or_drifted_pass_keeps_the_previous_sha() {
        with_clean_cell(|| {
            publish(&pass(
                "0123456789abcdef0123456789abcdef01234567",
                None,
                vec![tier(false, false)],
            ));
            // Fetch error: no commit, nothing applied.
            publish(&pass(
                "1111111111111111111111111111111111111111",
                Some("fetch failed"),
                vec![],
            ));
            // Write error on a drifted tier.
            publish(&pass(
                "2222222222222222222222222222222222222222",
                Some("writing machine tier failed"),
                vec![tier(false, true)],
            ));
            // A different read commit with autoApply off: drifted, unwritten.
            publish(&pass(
                "8888888888888888888888888888888888888888",
                None,
                vec![tier(false, true)],
            ));
            assert_eq!(
                applied_config_sha(),
                AppliedSha::Sha("0123456789abcdef0123456789abcdef01234567".to_string()),
                "no later pass cleared or advanced the applied SHA"
            );
        });
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn nothing_established_on_a_fleet_host_reads_unknown() {
        with_clean_cell(|| {
            // No cell value, no status file (the clean temp loom dir).
            assert_eq!(applied_config_sha(), AppliedSha::Unknown);
            assert_eq!(applied_config_sha().as_resource_value(), "unknown");
        });
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn the_cell_seeds_from_the_status_file_only_for_an_applied_pass() {
        with_clean_cell(|| {
            // An applied stored pass seeds the cell (the startup-timeout
            // window: the files on disk are that commit).
            store_status(&pass(
                "0123456789abcdef0123456789abcdef01234567",
                None,
                vec![tier(false, false)],
            ));
            assert_eq!(
                applied_config_sha(),
                AppliedSha::Sha("0123456789abcdef0123456789abcdef01234567".to_string())
            );
            // A stored pass that errored does not: unknown, not a stale SHA.
            reset_for_tests();
            store_status(&pass(
                "0123456789abcdef0123456789abcdef01234567",
                Some("fetch failed"),
                vec![],
            ));
            assert_eq!(applied_config_sha(), AppliedSha::Unknown);
        });
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn clearing_the_store_reports_none() {
        with_clean_cell(|| {
            publish(&pass(
                "0123456789abcdef0123456789abcdef01234567",
                None,
                vec![tier(false, false)],
            ));
            clear_status();
            assert_eq!(applied_config_sha(), AppliedSha::NoStore);
            assert_eq!(applied_config_sha().as_resource_value(), "none");
        });
    }
}
