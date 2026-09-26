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
//!
//! # Two arm registries (#8901)
//!
//! [`arm_singleton_job`]'s registry is **process-lifetime**: correct for a
//! job ticking *inside* the daemon (see [`crate::ci_telemetry`]'s poller, the
//! first real consumer), but a `loom-daemon fleet-captain <job>` invocation
//! from a shell wrapper is its own short-lived process that exits
//! immediately — recording an arm in a process-lifetime registry there would
//! be written and lost in the same breath, which is why `cli/fleet_captain_cmd.rs`'s
//! gate *check* deliberately never touches it.
//!
//! [`record_shell_arm`] is the durable, cross-process half that closes that
//! gap: a JSON file under `<root>/.loom/state/fleet-captain/armed.json`
//! (never git-tracked — same per-host-runtime-state class as
//! `.loom/state/ci-telemetry/`), keyed by job name, holding only
//! `last_armed_at`. The CLI's `run()` writes it after a successful (`Armed`)
//! gate check; [`shell_armed_job_names`] is the read side
//! [`armed_singleton_job_names_for_host`] merges into `host.health` alongside
//! the in-process registry.
//!
//! ## Staleness policy (the "Pick one deliberately" decision, #8901)
//!
//! A durable arm record left forever would go stale the moment its wrapper is
//! uninstalled or the host is retired — a permanently-stuck "armed" entry
//! that makes the dashboard's "singleton armed on a non-captain host" flag
//! cry wolf on every peer host from then on, which the issue that added this
//! registry calls out as *worse* than the honest empty list this replaces.
//! Three policies were on the table:
//!
//! 1. **A TTL derived from the job's own declared cadence.** Most precise,
//!    but requires every singleton job to declare a cadence somewhere (no
//!    such registry exists today), and a wrapper's timer definition already
//!    lives outside this repo (systemd/launchd/cron) — duplicating it here
//!    would be a second source of truth that drifts from the first.
//! 2. **A fixed, conservative TTL** ([`crate::config_resolver::DEFAULT_FLEET_CAPTAIN_ARM_TTL_SECS`],
//!    overridable via `fleet.captainArmTtlSecs`) — refreshed on every
//!    successful arm, so a live wrapper never flickers stale between its own
//!    ticks, and an uninstalled one self-heals within one TTL window with no
//!    action required.
//! 3. **An explicit `disarm` verb** the wrapper calls at its own teardown —
//!    precise timing, but *silently wrong* the moment a host is re-imaged or
//!    a wrapper is removed by deleting its timer unit rather than running it
//!    one last time to disarm — exactly the permanently-stuck-entry failure
//!    mode this exists to avoid.
//!
//! **Chosen: (2) as the safety net, with (3) offered as an optional
//! precision layer on top** (`loom-daemon fleet-captain <job> --disarm`,
//! [`forget_shell_arm`]). The TTL alone already guarantees no entry can be
//! stuck forever — the property #8901 asks for explicitly — and needs no new
//! per-job declaration; the disarm verb is free to add and lets a
//! well-behaved wrapper clear its entry immediately instead of waiting out
//! the TTL, without weakening the guarantee if it never calls it. (1) is left
//! as a possible future refinement once a job-cadence registry exists for
//! another reason.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

/// Process-lifetime set of singleton jobs whose most recent
/// [`arm_singleton_job`] call was refused because **no** `fleet.captain` is
/// declared (#9014) — the misconfiguration that otherwise stops such a job
/// on every host with only a log line to show for it. Kept separate from a
/// not-this-host refusal, which is routine on every non-captain host.
fn captainless_registry() -> &'static Mutex<BTreeSet<String>> {
    static REGISTRY: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Singleton job names currently refused on this host for want of a declared
/// `fleet.captain` (#9014), sorted. Sampled into
/// [`crate::telemetry::HostHealthRecord::captainless_singleton_jobs`].
#[must_use]
pub fn captainless_singleton_job_names() -> Vec<String> {
    captainless_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
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
    {
        let mut captainless = captainless_registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if gate == CaptainGate::NoCaptainDeclared {
            captainless.insert(job_name.to_string());
        } else {
            captainless.remove(job_name);
        }
    }
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

// ============================================================================
// Durable shell-arm registry (cross-process, for host.health) — #8901
// ============================================================================

/// One durable shell-arm record: when a `loom-daemon fleet-captain <job>`
/// invocation on this host most recently resolved `CaptainGate::Armed`. No
/// other fields — the registry is deliberately minimal, since staleness is
/// judged purely on recency (see the module doc's "Staleness policy").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ShellArmEntry {
    last_armed_at: DateTime<Utc>,
}

/// Job name -> its most recent shell-driven arm. Never git-tracked (see
/// [`shell_arm_registry_path`]'s doc) and per-host by construction — each
/// host's `.loom/state/` is its own local runtime directory, not synced.
type ShellArmRegistry = BTreeMap<String, ShellArmEntry>;

/// `<root>/.loom/state/fleet-captain/armed.json` — durable, per-host, never
/// git-tracked (added to `EPHEMERAL_PATTERNS` in `init/post_init.rs`
/// alongside its `ci-telemetry` sibling): committing one host's arm state
/// would hand it to every other host as a false starting fact.
fn shell_arm_registry_path(root: &Path) -> PathBuf {
    root.join(".loom")
        .join("state")
        .join("fleet-captain")
        .join("armed.json")
}

fn load_shell_arm_registry(root: &Path) -> ShellArmRegistry {
    std::fs::read_to_string(shell_arm_registry_path(root))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic JSON write: temp file (pid-suffixed, so two concurrent writers on
/// the same host never collide on the temp name) + fsync + rename — same
/// pattern as `ci_telemetry::state`'s `save_json`.
fn save_shell_arm_registry(root: &Path, registry: &ShellArmRegistry) -> io::Result<()> {
    let path = shell_arm_registry_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(registry).map_err(io::Error::other)?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, &path)
}

/// A held, exclusive, blocking `flock` on `armed.lock` beside the registry
/// (#9014), released on drop. Serialises the load → modify → save in
/// [`record_shell_arm`]/[`forget_shell_arm`] so two concurrent
/// `loom-daemon fleet-captain` invocations on one host can no longer each
/// read the old file and have the second rename drop the first's entry.
/// Reads stay lock-free: the save is an atomic rename, so a reader always
/// sees one complete version.
struct ShellArmLock {
    _file: std::fs::File,
}

impl ShellArmLock {
    fn acquire(root: &Path) -> io::Result<Self> {
        let path = shell_arm_registry_path(root).with_extension("lock");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: `flock` on a descriptor we own for the duration of the
            // call; no memory is shared with the kernel.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { _file: file })
    }
}

/// Record (or refresh) a durable shell-driven arm for `job_name` on this
/// host, called by `cli/fleet_captain_cmd.rs`'s `run()` after a successful
/// (`Armed`) gate check — never by `evaluate()`, which stays a pure read (see
/// its own doc and the `evaluating_does_not_touch_the_armed_registry`-style
/// tests). Re-arming an already-armed job overwrites its `last_armed_at`
/// rather than adding a second entry — the registry is keyed by job name, so
/// "refresh, don't duplicate" is a property of the data shape, not extra
/// logic.
pub fn record_shell_arm(root: &Path, job_name: &str, now: DateTime<Utc>) -> io::Result<()> {
    let _lock = ShellArmLock::acquire(root)?;
    let mut registry = load_shell_arm_registry(root);
    registry.insert(job_name.to_string(), ShellArmEntry { last_armed_at: now });
    save_shell_arm_registry(root, &registry)
}

/// Remove any durable shell-arm record for `job_name` on this host,
/// idempotently — the explicit-disarm precision option
/// (`loom-daemon fleet-captain <job> --disarm`) a well-behaved wrapper may
/// call at its own teardown. Never required for correctness: the TTL in
/// [`shell_armed_job_names`] already guarantees no entry survives forever
/// even if this is never called.
pub fn forget_shell_arm(root: &Path, job_name: &str) -> io::Result<()> {
    let _lock = ShellArmLock::acquire(root)?;
    let mut registry = load_shell_arm_registry(root);
    if registry.remove(job_name).is_some() {
        save_shell_arm_registry(root, &registry)
    } else {
        Ok(())
    }
}

/// Shell-driven job names armed on this host and not yet stale as of `now`,
/// per the `ttl` window (see the module doc's "Staleness policy" for why a
/// fixed TTL was chosen). A record whose `last_armed_at` is in the future
/// (clock skew) is treated as fresh rather than discarded — the same
/// "unknown/odd reads as not-yet-a-problem" posture the rest of this crate's
/// health sampling uses for an unmeasurable signal.
#[must_use]
pub fn shell_armed_job_names(root: &Path, now: DateTime<Utc>, ttl: Duration) -> Vec<String> {
    let ttl_secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
    load_shell_arm_registry(root)
        .into_iter()
        .filter(|(_, entry)| {
            now.signed_duration_since(entry.last_armed_at).num_seconds() <= ttl_secs
        })
        .map(|(name, _)| name)
        .collect()
}

/// Every singleton job name currently armed on this host, merging the
/// in-process registry ([`armed_singleton_job_names`], in-daemon jobs like
/// [`crate::ci_telemetry`]'s poller) with the durable shell-arm registry
/// ([`shell_armed_job_names`], not yet stale per `root`'s
/// `fleet.captainArmTtlSecs`) — the single source
/// [`crate::observability::collector::sample_host_health`] samples into
/// [`crate::telemetry::HostHealthRecord::armed_singleton_jobs`]. Sorted and
/// deduped (a job could in principle be armed both ways, though that would be
/// an unusual deployment).
#[must_use]
pub fn armed_singleton_job_names_for_host(root: &Path) -> Vec<String> {
    let ttl = Duration::from_secs(crate::config_resolver::fleet_captain_arm_ttl_secs(root));
    let mut names: BTreeSet<String> = armed_singleton_job_names().into_iter().collect();
    names.extend(shell_armed_job_names(root, Utc::now(), ttl));
    names.into_iter().collect()
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

    // ===== Durable shell-arm registry (#8901) =====

    #[test]
    fn shell_arm_written_by_one_call_is_read_back_by_another() {
        // Simulates the daemon/CLI split this registry exists to close: two
        // independent calls sharing only the filesystem, exactly like two
        // separate `loom-daemon fleet-captain` process invocations would.
        let dir = tempdir().unwrap();
        let now = Utc::now();
        record_shell_arm(dir.path(), "forge-queue-check", now).unwrap();

        let ttl = Duration::from_secs(3600);
        assert_eq!(
            shell_armed_job_names(dir.path(), now, ttl),
            vec!["forge-queue-check".to_string()]
        );
    }

    #[test]
    fn a_stale_shell_arm_stops_being_reported() {
        let dir = tempdir().unwrap();
        let armed_at = Utc::now();
        record_shell_arm(dir.path(), "forge-queue-check", armed_at).unwrap();

        let ttl = Duration::from_secs(3600);
        let just_inside = armed_at + chrono::Duration::seconds(3599);
        let just_outside = armed_at + chrono::Duration::seconds(3601);

        assert_eq!(
            shell_armed_job_names(dir.path(), just_inside, ttl),
            vec!["forge-queue-check".to_string()],
            "still within the TTL window"
        );
        assert!(
            shell_armed_job_names(dir.path(), just_outside, ttl).is_empty(),
            "past the TTL window must stop being reported, not linger forever"
        );
    }

    #[test]
    fn re_arming_refreshes_rather_than_duplicating() {
        let dir = tempdir().unwrap();
        let first = Utc::now();
        record_shell_arm(dir.path(), "forge-queue-check", first).unwrap();
        let second = first + chrono::Duration::seconds(30);
        record_shell_arm(dir.path(), "forge-queue-check", second).unwrap();

        // Only one entry — re-arming overwrote, it didn't add a sibling.
        let registry = load_shell_arm_registry(dir.path());
        assert_eq!(registry.len(), 1);
        assert_eq!(registry["forge-queue-check"].last_armed_at, second);
    }

    #[test]
    fn forget_shell_arm_removes_it_immediately_ahead_of_the_ttl() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        record_shell_arm(dir.path(), "forge-queue-check", now).unwrap();
        assert!(!shell_armed_job_names(dir.path(), now, Duration::from_secs(3600)).is_empty());

        forget_shell_arm(dir.path(), "forge-queue-check").unwrap();
        assert!(shell_armed_job_names(dir.path(), now, Duration::from_secs(3600)).is_empty());

        // Idempotent: forgetting an already-absent job is not an error.
        assert!(forget_shell_arm(dir.path(), "forge-queue-check").is_ok());
    }

    #[test]
    fn armed_singleton_job_names_for_host_merges_process_and_shell_registries() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"fleet": {"captain": "loom-worker-1"}}"#,
        );
        let in_process_job = format!("in-process-job-{}", std::process::id());
        let shell_job = format!("shell-job-{}", std::process::id());

        assert!(arm_singleton_job(&in_process_job, dir.path(), "loom-worker-1").is_ok());
        record_shell_arm(dir.path(), &shell_job, Utc::now()).unwrap();

        let names = armed_singleton_job_names_for_host(dir.path());
        assert!(names.contains(&in_process_job), "{names:?}");
        assert!(names.contains(&shell_job), "{names:?}");

        disarm_singleton_job(&in_process_job);
        forget_shell_arm(dir.path(), &shell_job).unwrap();
    }

    #[test]
    fn armed_singleton_job_names_for_host_honors_the_configured_ttl() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"fleet": {"captainArmTtlSecs": 1}}"#,
        );
        let shell_job = format!("short-ttl-job-{}", std::process::id());
        let ten_minutes_ago = Utc::now() - chrono::Duration::minutes(10);
        record_shell_arm(dir.path(), &shell_job, ten_minutes_ago).unwrap();

        assert!(
            !armed_singleton_job_names_for_host(dir.path()).contains(&shell_job),
            "a 1-second configured TTL must age this out immediately"
        );
    }

    #[test]
    fn armed_singleton_jobs_still_omits_when_nothing_is_armed() {
        // The `#[serde(skip_serializing_if)]` contract on
        // `HostHealthRecord::armed_singleton_jobs` depends on an empty `Vec`,
        // never `None`-vs-populated confusion — this pins that the durable
        // shell-arm side returns a plain empty `Vec` (not, say, a sentinel)
        // for a root nothing has ever armed.
        //
        // Deliberately checks `shell_armed_job_names` here, not the merged
        // `armed_singleton_job_names_for_host`: the latter also folds in
        // [`armed_registry`], a **process-wide global** that other tests in
        // this same binary legitimately arm/disarm concurrently (test
        // binaries run tests in parallel threads) — asserting it is globally
        // empty would be racy against unrelated tests, not a property of
        // this root's own (fresh, unique-per-test) durable file.
        let dir = tempdir().unwrap();
        assert!(shell_armed_job_names(dir.path(), Utc::now(), Duration::from_secs(3600)).is_empty());
    }

    #[test]
    fn a_no_captain_refusal_is_recorded_as_captainless_9014() {
        let dir = tempdir().unwrap();
        let job = format!("captainless-job-{}", std::process::id());
        let config = dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL);

        write(&config, r#"{}"#);
        assert!(arm_singleton_job(&job, dir.path(), "host-a").is_err());
        assert!(captainless_singleton_job_names().contains(&job));

        // A declared captain on another host is a routine refusal, not a
        // misconfiguration — it must leave the captainless set.
        write(&config, r#"{"fleet": {"captain": "host-b"}}"#);
        assert!(arm_singleton_job(&job, dir.path(), "host-a").is_err());
        assert!(!captainless_singleton_job_names().contains(&job));

        write(&config, r#"{}"#);
        assert!(arm_singleton_job(&job, dir.path(), "host-a").is_err());
        write(&config, r#"{"fleet": {"captain": "host-a"}}"#);
        assert!(arm_singleton_job(&job, dir.path(), "host-a").is_ok());
        assert!(!captainless_singleton_job_names().contains(&job));
        disarm_singleton_job(&job);
    }

    #[test]
    fn concurrent_shell_arms_on_one_host_lose_no_entry_9014() {
        // Without the registry lock, each writer read the old file and the
        // last rename won, dropping the other writers' entries.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let now = Utc::now();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || record_shell_arm(&root, &format!("job-{i}"), now))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(shell_armed_job_names(&root, now, Duration::from_secs(60)).len(), 16);
    }
}
