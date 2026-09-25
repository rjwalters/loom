//! Fleet captain designation (#8848): first-class placement for **singleton
//! jobs** — checks that watch one shared thing (a forge-wide queue, a public
//! feed staleness check, a token-pool anomaly check) and must run on exactly
//! one fleet host, unlike **per-host jobs** (the daemon watchdog, clean,
//! resync, drift) that every host runs unchanged.
//!
//! # The problem this fixes
//!
//! Before this module, an operator hand-placed a singleton by installing a
//! timer on a chosen host and documenting the choice in prose. Nothing
//! enforced "exactly one": a rebuilt or re-imaged worker silently lost the
//! timer, or a second host silently duplicated it, and each ad hoc wrapper
//! reinvented its own fail-closed host gate (2AMLogic/2am's
//! `batch-fleet-reconcile-schedule.sh` / `loom-wake-pull-schedule.sh`, not
//! present in this repo). Two hosts filing the same dedup-sensitive alert is
//! the #6714 race class this exists to keep out of the picture entirely: no
//! job is armed on two hosts at once *by construction*, because arming
//! requires an exact host-identity match against a single declared value.
//!
//! # The captain: assigned, not elected
//!
//! `fleet.captain: "<host id>"` in the **tracked** `.loom/config.json`
//! (`[`crate::config_resolver::fleet_captain`]`) names the one host, by its
//! own [`crate::sweep_registry::host_identity`], that runs every declared
//! singleton job. This belongs in the tracked tier, not a host-local env
//! var, following the `autonomous.roleRunner.shardCount`/`shardKey`
//! "identical fleet-wide" precedent
//! (`defaults/docs/daemon-reference.md`'s "Role-runner host sharding"
//! section) rather than `shardIndex`'s "must differ per host" one: every
//! host must agree on who the captain is, and a committed file is identical
//! fleet-wide by construction.
//!
//! There is no election and no lease — 2am's own vocabulary is "captain"
//! (assigned) rather than a term implying automatic failover. The "optional
//! later" extension — a lease so a captain down for longer than N hours can
//! be noticed — is explicitly out of scope for this module, tracked as
//! #8902. It would be an *alert*, not failover: a singleton that runs twice
//! is the duplicate-alert bug this exists to prevent, while one that is late
//! is merely late.
//!
//! # Fail-closed semantics (load-bearing)
//!
//! Mirrors [`crate::host_affinity::HostConstraint::matches`]'s fail-closed
//! shape and the refusal-message style of `cli/dispatch.rs`'s
//! `host_constraint_refusal`, at fleet-config scope rather than
//! issue-affinity scope:
//!
//! - **No `fleet.captain` declared at all** ([`CaptainGate::NoCaptainDeclared`]):
//!   refuses. This is a deliberately **defined** outcome, not merely
//!   "whatever falls out" — a declared singleton job with no captain
//!   assigned must never silently arm everywhere (that reopens exactly the
//!   duplicate-alert race this module exists to close) nor silently arm
//!   nowhere with no signal (see the next point).
//! - **A captain IS declared but this host is not it**
//!   ([`CaptainGate::Refused`]): refuses, naming the current captain.
//! - **This host IS the declared captain** ([`CaptainGate::Armed`]): arms.
//!
//! There is no partial-match or case-insensitive fallback — exact string
//! equality against [`crate::sweep_registry::host_identity`], the same
//! "every syntactically valid declared value is trusted at face value"
//! contract [`crate::host_affinity`] documents for its own constraint.
//!
//! # Per-host jobs are unaffected
//!
//! This module is opt-in per job: a per-host job (the daemon watchdog,
//! clean, resync, drift) simply never calls [`arm_singleton_job`]. Nothing
//! here changes any existing cron/timer/tick's behavior — the whole point is
//! that only a job that explicitly declares itself a singleton gains a
//! captain check at all.
//!
//! # Observability
//!
//! A successful [`arm_singleton_job`] call records the job name in this
//! process's armed-singleton registry ([`armed_singleton_job_names`]),
//! sampled into [`crate::telemetry::HostHealthRecord::armed_singleton_jobs`]
//! by [`crate::observability::collector`]'s `sample_host_health`, alongside
//! [`crate::telemetry::HostHealthRecord::is_captain`] — see
//! [`CaptainGate::is_captain_flag`] for why that field is three-valued
//! (`None`/`Some(false)`/`Some(true)`), not a bare `bool`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

/// Outcome of gating a declared singleton job against this host's identity —
/// see the module doc's "Fail-closed semantics" for the contract each
/// variant upholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptainGate {
    /// This host IS the declared captain — the job may arm.
    Armed {
        /// The declared captain host id (equal to the resolving host's own
        /// identity).
        captain: String,
    },
    /// A captain IS declared, but this host is not it.
    Refused {
        /// The declared captain host id.
        captain: String,
        /// This host's own resolved identity.
        current_host_id: String,
    },
    /// No `fleet.captain` declared at all in the effective config.
    NoCaptainDeclared,
}

impl CaptainGate {
    /// `true` only for [`Self::Armed`].
    #[must_use]
    pub fn is_armed(&self) -> bool {
        matches!(self, Self::Armed { .. })
    }

    /// The three-valued `is_captain` telemetry projection —
    /// [`crate::telemetry::HostHealthRecord::is_captain`]'s exact contract:
    /// `None` when the mechanism does not apply here at all (no captain
    /// declared), `Some(false)`/`Some(true)` otherwise.
    #[must_use]
    pub fn is_captain_flag(&self) -> Option<bool> {
        match self {
            Self::Armed { .. } => Some(true),
            Self::Refused { .. } => Some(false),
            Self::NoCaptainDeclared => None,
        }
    }

    /// A human-readable message for `job_name`, mirroring
    /// `cli/dispatch.rs`'s `host_constraint_refusal` named-host refusal
    /// style. Reads naturally for [`Self::Armed`] too (a caller logging its
    /// own arm decision at `info!` can use the same formatter for both
    /// outcomes).
    #[must_use]
    pub fn message(&self, job_name: &str) -> String {
        match self {
            Self::Armed { captain } => {
                format!("singleton job '{job_name}' armed: this host is the fleet captain ({captain}) (#8848).")
            }
            Self::Refused {
                captain,
                current_host_id,
            } => format!(
                "singleton job '{job_name}' refused: fleet captain is {captain}, this host is \
                 {current_host_id} (#8848). Only the captain runs this job — declare \
                 `fleet.captain: \"{current_host_id}\"` in .loom/config.json to arm it here \
                 instead."
            ),
            Self::NoCaptainDeclared => format!(
                "singleton job '{job_name}' refused: no fleet.captain declared in \
                 .loom/config.json (#8848). Declare `fleet.captain: \"<host id>\"` to arm this \
                 job on exactly one fleet host."
            ),
        }
    }
}

/// The pure core of the captain gate: given the declared captain (if any)
/// and this host's own identity, decide the outcome. Exact string equality
/// only — no partial or case-insensitive match, mirroring
/// [`crate::host_affinity::HostConstraint::matches`].
///
/// Takes `current_host_id` as a parameter rather than calling
/// [`crate::sweep_registry::host_identity`] itself, so callers — and tests —
/// inject it as a test double (the `RecordingDispatcher.current_host_id`
/// pattern `work_finder/tests.rs` already uses for the same reason: no
/// process-global `LOOM_HOST_ID`/`HOSTNAME` mutation, and therefore no race
/// with `sweep_registry::tests::host_identity_env_precedence`'s RAII env
/// guard).
#[must_use]
pub fn resolve_gate(captain: Option<&str>, current_host_id: &str) -> CaptainGate {
    match captain.map(str::trim).filter(|s| !s.is_empty()) {
        None => CaptainGate::NoCaptainDeclared,
        Some(cap) if cap == current_host_id => CaptainGate::Armed {
            captain: cap.to_string(),
        },
        Some(cap) => CaptainGate::Refused {
            captain: cap.to_string(),
            current_host_id: current_host_id.to_string(),
        },
    }
}

/// [`resolve_gate`], reading the declared captain from `root`'s effective
/// config ([`crate::config_resolver::fleet_captain`]).
#[must_use]
pub fn resolve_gate_for_root(root: &Path, current_host_id: &str) -> CaptainGate {
    let captain = crate::config_resolver::fleet_captain(root);
    resolve_gate(captain.as_deref(), current_host_id)
}

// ============================================================================
// Armed-singleton-job registry (process-lifetime, for `host.health`)
// ============================================================================

/// Process-lifetime set of singleton job names currently armed on this host —
/// the source [`armed_singleton_job_names`] reads and
/// [`arm_singleton_job`]/[`disarm_singleton_job`] maintain. Mirrors the
/// `OnceLock<Mutex<..>>` process-registry shape `role_runner`'s
/// `ROLE_TICKS`/`LAST_ROLE_TICK` already use for the same "the daemon's own
/// authoritative view of its live process state" purpose.
fn armed_registry() -> &'static Mutex<BTreeSet<String>> {
    static REGISTRY: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Every singleton job name currently recorded as armed on this host, sorted
/// (the registry is a `BTreeSet`). Sampled into
/// [`crate::telemetry::HostHealthRecord::armed_singleton_jobs`].
#[must_use]
pub fn armed_singleton_job_names() -> Vec<String> {
    armed_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

/// Remove `job_name` from the armed registry, idempotently. Exposed so a
/// caller that re-evaluates its own gate on every tick (the intended usage —
/// see [`arm_singleton_job`]) self-heals within one tick if `fleet.captain`
/// changes out from under it, rather than leaving a stale "armed" entry in
/// `host.health` forever.
pub fn disarm_singleton_job(job_name: &str) {
    armed_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(job_name);
}

/// Gate `job_name` against `root`'s declared `fleet.captain` and
/// `current_host_id`, updating the process-lifetime armed registry to match
/// the outcome (armed ⇒ recorded; anything else ⇒ removed, so a job that
/// flips from armed to refused across ticks — a `fleet.captain` edit — never
/// leaves a stale entry behind).
///
/// Returns `Ok(())` when armed, `Err(message)` naming the current captain
/// (or the absence of one) otherwise — the fail-closed refusal a caller
/// should log and skip its normal per-tick work on, exactly the same shape
/// `cli/dispatch.rs`'s host-affinity refusal already establishes for the
/// issue-scoped mechanism.
///
/// **Intended usage**: called once per tick by the schedule-wrapper for a
/// declared singleton job, not once at process startup — a config change or
/// a host-identity change must take effect on the very next tick, not
/// require a daemon restart.
pub fn arm_singleton_job(job_name: &str, root: &Path, current_host_id: &str) -> Result<(), String> {
    let gate = resolve_gate_for_root(root, current_host_id);
    if gate.is_armed() {
        armed_registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(job_name.to_string());
        Ok(())
    } else {
        disarm_singleton_job(job_name);
        Err(gate.message(job_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    // ===== resolve_gate (pure) =====

    #[test]
    fn captain_matches_current_host_arms() {
        let gate = resolve_gate(Some("loom-worker-1"), "loom-worker-1");
        assert_eq!(
            gate,
            CaptainGate::Armed {
                captain: "loom-worker-1".to_string()
            }
        );
        assert!(gate.is_armed());
        assert_eq!(gate.is_captain_flag(), Some(true));
    }

    #[test]
    fn captain_does_not_match_current_host_refuses_naming_the_captain() {
        let gate = resolve_gate(Some("loom-worker-1"), "loom-worker-2");
        assert_eq!(
            gate,
            CaptainGate::Refused {
                captain: "loom-worker-1".to_string(),
                current_host_id: "loom-worker-2".to_string(),
            }
        );
        assert!(!gate.is_armed());
        assert_eq!(gate.is_captain_flag(), Some(false));
        let msg = gate.message("edge-queue-pull");
        assert!(msg.contains("loom-worker-1"), "{msg}");
        assert!(msg.contains("loom-worker-2"), "{msg}");
        assert!(msg.contains("edge-queue-pull"), "{msg}");
    }

    #[test]
    fn no_captain_declared_refuses_with_defined_behavior() {
        let gate = resolve_gate(None, "loom-worker-1");
        assert_eq!(gate, CaptainGate::NoCaptainDeclared);
        assert!(!gate.is_armed());
        assert_eq!(gate.is_captain_flag(), None);
        let msg = gate.message("edge-queue-pull");
        assert!(msg.contains("no fleet.captain declared"), "{msg}");
    }

    #[test]
    fn blank_captain_is_treated_as_not_declared() {
        assert_eq!(resolve_gate(Some("   "), "loom-worker-1"), CaptainGate::NoCaptainDeclared);
        assert_eq!(resolve_gate(Some(""), "loom-worker-1"), CaptainGate::NoCaptainDeclared);
    }

    #[test]
    fn no_partial_or_case_insensitive_match() {
        // Fail-closed: "Loom-Worker-1" must NOT match "loom-worker-1", and a
        // captain value that is a substring of this host's id must not match
        // either — mirrors `host_affinity::HostConstraint`'s own contract.
        assert!(!resolve_gate(Some("Loom-Worker-1"), "loom-worker-1").is_armed());
        assert!(!resolve_gate(Some("loom-worker-1"), "loom-worker-10").is_armed());
        assert!(!resolve_gate(Some("worker-1"), "loom-worker-1").is_armed());
    }

    // ===== resolve_gate_for_root / arm_singleton_job (config + registry) =====

    #[test]
    fn resolve_gate_for_root_reads_the_tracked_config() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"fleet": {"captain": "loom-worker-1"}}"#,
        );
        assert_eq!(
            resolve_gate_for_root(dir.path(), "loom-worker-1"),
            CaptainGate::Armed {
                captain: "loom-worker-1".to_string()
            }
        );
        assert_eq!(
            resolve_gate_for_root(dir.path(), "loom-worker-2"),
            CaptainGate::Refused {
                captain: "loom-worker-1".to_string(),
                current_host_id: "loom-worker-2".to_string(),
            }
        );
    }

    #[test]
    fn resolve_gate_for_root_no_captain_declared_is_defined() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL), r#"{}"#);
        assert_eq!(
            resolve_gate_for_root(dir.path(), "loom-worker-1"),
            CaptainGate::NoCaptainDeclared
        );
    }

    #[test]
    fn arm_singleton_job_records_and_clears_the_registry() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"fleet": {"captain": "loom-worker-1"}}"#,
        );
        let job = format!("test-job-{}", std::process::id());

        // Refused on the non-captain host: never recorded as armed.
        assert!(arm_singleton_job(&job, dir.path(), "loom-worker-2").is_err());
        assert!(!armed_singleton_job_names().contains(&job));

        // Armed on the captain host: recorded.
        assert!(arm_singleton_job(&job, dir.path(), "loom-worker-1").is_ok());
        assert!(armed_singleton_job_names().contains(&job));

        // A later tick that no longer arms (e.g. the config changed) clears it.
        assert!(arm_singleton_job(&job, dir.path(), "loom-worker-2").is_err());
        assert!(!armed_singleton_job_names().contains(&job));

        disarm_singleton_job(&job);
    }
}
