//! Machine-wide **issue-filing lock** — serializes `gh issue create` bursts
//! across every workspace on a host, and (softly) across hosts (Issue #6714).
//!
//! # Why this exists
//!
//! #3707 identified that parallel issue-creating agents race on `gh issue
//! create` and can cross-contaminate bodies. It shipped a **documentation-only**
//! mitigation: *"Do not run concurrent Architects — serialize issue creation."*
//! On **2026-08-08T02:49:04Z–02:49:23Z** that guidance failed exactly as a
//! convention must: two Architects filing 5-issue bursts into **different**
//! repos (`2AMLogic/gf180-sram` and `2AMLogic/sky130-modexp`) overlapped, and
//! gf180-sram's five bodies were overwritten with sky130-modexp's — one
//! directionally, off by one. Titles were unaffected, so nothing looked wrong
//! for **13 days**.
//!
//! The documentation could not hold for three structural reasons, none of them
//! operator error:
//!
//! 1. [`crate::role_collision::InProgressGuard`] serializes `(root, role)` — it
//!    is **per-workspace by construction** and cannot serialize an Architect in
//!    one repo against an Architect in another.
//! 2. Concurrent cross-repo Architects are *normal operation*: the daemon is the
//!    scheduler, no human is in the loop at dispatch time, so "do not run
//!    concurrent Architects" is not a thing anyone can comply with.
//! 3. [`crate::issue_creation_mutex::IssueCreationMutex`] (#3707's Phase-2
//!    primitive) is an **in-process** `tokio::sync::Mutex`. Agents run as
//!    separate OS processes spawned by `spawn-claude.sh`, so nothing that mutex
//!    guards is in the same address space as the `gh issue create` that
//!    actually races.
//!
//! This module is the mechanism the convention could not be: a **cross-process**
//! lock, held around the actual filing call site
//! (`.loom/scripts/create-issue.sh`, the single-sourced entry point every
//! issue-creating role already goes through).
//!
//! # Two tiers, with an honest boundary
//!
//! | Tier | Scope | Strength |
//! |---|---|---|
//! | Host | every workspace + every agent process on one machine | **hard** mutual exclusion (`mkdir`-atomic) |
//! | Fleet | other hosts' daemons | **soft**, TTL-bounded backoff |
//!
//! The host tier is what actually closes the 2026-08-08 hazard: two processes
//! can only swap each other's *body text* if they share memory or a filesystem,
//! which by definition means one machine. The fleet tier covers the weaker,
//! genuinely cross-host hazard #3707 also named — a burst binding the wrong
//! freshly-minted issue **number** into a `Part of #N` cross-reference.
//!
//! The fleet tier is deliberately soft, and this is not a shortcut: it rides
//! [`crate::peer_claims`]'s advertise/observe/expire transport, whose own module
//! documentation states the contract — *"A room broadcast is eventually
//! consistent, so this is a fast backoff, not a lock."* A real cross-host mutex
//! would need an atomic authority ([`crate::peer_claims`]'s still-unbuilt Phase
//! 2 CAS), not a broadcast. Rather than pretend otherwise, the fleet tier
//! **mirrors** an observed peer hold into this same on-disk store
//! ([`record_peer_hold`]), so host-local filers back off from it through exactly
//! the one code path they already consult.
//!
//! # On-disk protocol (shared with the Bash half)
//!
//! `defaults/scripts/lib/filing-lock.sh` implements the identical protocol, so a
//! shell filer and the daemon serialize against each other. Mirrors the
//! [`crate::build_slot`] / `build-slot.sh` pairing.
//!
//! ```text
//! <store>/                     # ~/.loom/locks/issue-filing by default
//!   holder/                    # the lock itself — `mkdir`-atomic
//!     owner.json               # {"host","pid","label","acquired_at"}
//!   peers/
//!     <host>                   # a peer host's advertised hold; mtime = LOCAL receipt
//! ```
//!
//! # Four safety properties (all load-bearing)
//!
//! 1. **A crashed holder cannot wedge fleet-wide issue creation.** Two
//!    independent reap legs: the holder dir's mtime aging past
//!    [`DEFAULT_STALE_SECS`], and — for an owner recorded on *this* host — a
//!    dead owner PID, reaped immediately (the [`crate::live_claim`] discipline
//!    the `loom:building` lease already uses).
//! 2. **Bounded and fail-safe.** [`acquire_in`] waits at most `wait` and then
//!    returns [`AcquireOutcome::Deferred`] — the caller **must not file**; it
//!    defers its burst to the next tick and says so in its log. This is the one
//!    place this module deliberately does *not* copy [`crate::build_slot`],
//!    which degrades open: an unserialized build wastes CPU, an unserialized
//!    filing burst corrupts issue bodies.
//! 3. **Degrades open only when there is no lock to take.** An unusable store
//!    (no `$HOME`, unwritable path) yields [`AcquireOutcome::DegradedOpen`] and
//!    the caller proceeds — refusing to file because the lock store is broken
//!    would convert a corruption risk into a total outage, and a store nobody
//!    can write is a store nobody is serialized by anyway.
//! 4. **Re-entrant.** A holder exports [`FILING_LOCK_HELD_ENV`] to children, so
//!    a role that wraps a whole burst does not deadlock against the per-call
//!    acquire inside `create-issue.sh`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

// ============================================================================
// Configuration (env-only — the Bash half must resolve identical values)
// ============================================================================
//
// Env-only on purpose, matching [`crate::build_slot`]'s precedent: a knob with
// an independent Bash-side reader stays env-only rather than honoring
// `.loom/config.json` on one path and silently ignoring it on the other.

/// Set to `0` (also `false`/`off`/`no`) to disable filing-lock serialization
/// entirely — every acquire degrades open. The pre-#6714 behavior, for an
/// operator who needs it back in a hurry.
pub const FILING_LOCK_ENABLED_ENV: &str = "LOOM_FILING_LOCK";

/// Override for the machine-wide store directory (default
/// `~/.loom/locks/issue-filing`). Primarily a test seam; also lets an operator
/// relocate the store.
pub const FILING_LOCK_DIR_ENV: &str = "LOOM_FILING_LOCK_DIR";

/// Bounded wait for the lock, in seconds. On expiry the acquire **defers**
/// (property 2 above) — it never blocks longer and never files unserialized.
pub const FILING_LOCK_WAIT_SECS_ENV: &str = "LOOM_FILING_LOCK_WAIT_SECS";

/// Default bounded wait: 120s. A filing burst is a handful of API calls
/// (seconds), so 120s absorbs several queued bursts while still deferring
/// promptly rather than parking an agent behind a wedged holder.
pub const DEFAULT_WAIT_SECS: u64 = 120;

/// Age (seconds) at which the holder lock is considered abandoned and reaped.
pub const FILING_LOCK_STALE_SECS_ENV: &str = "LOOM_FILING_LOCK_STALE_SECS";

/// Default stale threshold: 300s. Deliberately far longer than a real burst
/// (seconds) so a slow-but-healthy holder is never reaped out from under
/// itself, and far shorter than [`crate::build_slot`]'s 1h — filing is not a
/// long-running stage, so a wedged holder should clear in minutes.
pub const DEFAULT_STALE_SECS: u64 = 300;

/// TTL (seconds) for a **peer** host's advertised hold, measured against LOCAL
/// receipt time. Mirrors [`crate::peer_claims`]'s own rule: a peer's wall clock
/// is not comparable across hosts, so it is never used for TTL math.
pub const FILING_LOCK_PEER_TTL_SECS_ENV: &str = "LOOM_FILING_LOCK_PEER_TTL_SECS";

/// Default peer-hold TTL: 60s. Short on purpose — a peer's `FilingUnlock` ad
/// may be lost by the eventually-consistent transport, and a lost unlock must
/// cost at most one minute of extra queuing on every other host, never a
/// fleet-wide wedge.
pub const DEFAULT_PEER_TTL_SECS: u64 = 60;

/// Re-entrancy sentinel exported into a holder's child environment.
pub const FILING_LOCK_HELD_ENV: &str = "LOOM_FILING_LOCK_HELD";

/// How often to re-probe while waiting.
pub const DEFAULT_POLL: Duration = Duration::from_millis(250);

/// Whether filing-lock serialization is enabled ([`FILING_LOCK_ENABLED_ENV`]
/// not set to a falsey value).
#[must_use]
pub fn is_enabled() -> bool {
    match std::env::var(FILING_LOCK_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

/// Whether this process is already inside a filing lock held by an ancestor.
#[must_use]
pub fn is_held_here() -> bool {
    std::env::var(FILING_LOCK_HELD_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Resolve a positive-seconds env knob, falling back to `default`.
fn resolve_secs(var: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(var)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(default),
    )
}

/// Resolve the bounded wait from [`FILING_LOCK_WAIT_SECS_ENV`].
#[must_use]
pub fn resolve_wait() -> Duration {
    resolve_secs(FILING_LOCK_WAIT_SECS_ENV, DEFAULT_WAIT_SECS)
}

/// Resolve the stale-holder threshold from [`FILING_LOCK_STALE_SECS_ENV`].
#[must_use]
pub fn resolve_stale() -> Duration {
    resolve_secs(FILING_LOCK_STALE_SECS_ENV, DEFAULT_STALE_SECS)
}

/// Resolve the peer-hold TTL from [`FILING_LOCK_PEER_TTL_SECS_ENV`].
#[must_use]
pub fn resolve_peer_ttl() -> Duration {
    resolve_secs(FILING_LOCK_PEER_TTL_SECS_ENV, DEFAULT_PEER_TTL_SECS)
}

/// The machine-wide store directory: [`FILING_LOCK_DIR_ENV`] when set, else
/// `~/.loom/locks/issue-filing`. `None` when neither resolves (no home
/// directory) — the caller degrades open.
#[must_use]
pub fn store_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(FILING_LOCK_DIR_ENV) {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    Some(
        dirs::home_dir()?
            .join(".loom")
            .join("locks")
            .join("issue-filing"),
    )
}

/// The holder lock directory inside `store`.
#[must_use]
pub fn holder_path(store: &Path) -> PathBuf {
    store.join("holder")
}

/// The peer-hold directory inside `store`.
#[must_use]
pub fn peers_dir(store: &Path) -> PathBuf {
    store.join("peers")
}

/// Reduce a host identity to a safe single path component. Anything outside
/// `[A-Za-z0-9._-]` folds to `_`, so a hostile or merely exotic advertised host
/// string can never escape [`peers_dir`] (no `/`, no `..`).
#[must_use]
pub fn sanitize_host(host: &str) -> String {
    let mapped: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `.`/`..` would still be path-traversal-ish components after mapping.
    match mapped.as_str() {
        "" | "." | ".." => "_".to_string(),
        _ => mapped,
    }
}

// ============================================================================
// Owner record
// ============================================================================

/// Who holds the lock, as recorded in `<store>/holder/owner.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilingLockOwner {
    /// The holding host's identity ([`crate::sweep_registry::host_identity`]).
    pub host: String,
    /// The holding process's PID — only meaningful together with `host`.
    pub pid: u32,
    /// What the holder is filing (role / call site), for logs.
    pub label: String,
    /// Wall-clock epoch seconds at acquire. **Diagnostic only** — staleness is
    /// measured against the lock directory's mtime, never this.
    pub acquired_at: u64,
}

impl FilingLockOwner {
    #[must_use]
    fn to_json(&self) -> String {
        json!({
            "host": self.host,
            "pid": self.pid,
            "label": self.label,
            "acquired_at": self.acquired_at,
        })
        .to_string()
    }

    /// Parse an `owner.json` payload. Returns `None` for anything malformed —
    /// an unreadable owner record means "unknown owner", which the reap legs
    /// treat conservatively (mtime aging still applies; dead-PID reaping does
    /// not).
    #[must_use]
    pub fn from_json_str(s: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(s).ok()?;
        let obj = v.as_object()?;
        Some(Self {
            host: obj.get("host").and_then(Value::as_str)?.to_owned(),
            pid: u32::try_from(obj.get("pid").and_then(Value::as_u64).unwrap_or(0)).ok()?,
            label: obj
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            acquired_at: obj.get("acquired_at").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

/// Read the current holder's owner record, if the lock is held and the record
/// is readable.
#[must_use]
pub fn read_owner(store: &Path) -> Option<FilingLockOwner> {
    let raw = std::fs::read_to_string(holder_path(store).join("owner.json")).ok()?;
    FilingLockOwner::from_json_str(&raw)
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Age of `path` by mtime. `None` when the mtime is unreadable — never reap a
/// lock we cannot age (the conservative direction, matching
/// `tokens_pool::locking::is_stale`).
fn age_of(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    SystemTime::now().duration_since(modified).ok()
}

// ============================================================================
// Peer holds (the fleet tier's on-disk mirror)
// ============================================================================

/// Record that peer `host` advertised holding the filing lock (Issue #6714).
///
/// Called from [`crate::safehouse::PeerClaimSink`] on an observed
/// [`crate::peer_claims::ClaimKind::FilingLock`] ad. The marker file's **mtime
/// is the local receipt time**, which is what [`live_peer_holds`] ages against
/// — the advertiser's wall clock is never trusted (clock skew), exactly as
/// [`crate::peer_claims`] documents for its own TTL.
///
/// Best-effort: a failure to write the marker degrades the fleet tier to
/// host-only serialization, which is strictly better than failing the caller.
pub fn record_peer_hold(store: &Path, host: &str) {
    let dir = peers_dir(store);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(sanitize_host(host));
    // Rewriting refreshes the mtime, which is exactly the re-advertisement
    // heartbeat semantics `PeerClaimView::observe_at` gives repeat ads.
    let _ = std::fs::write(&path, host.as_bytes());
}

/// Clear peer `host`'s advertised hold (an observed
/// [`crate::peer_claims::ClaimKind::FilingUnlock`], or a TTL lapse).
pub fn clear_peer_hold(store: &Path, host: &str) {
    let _ = std::fs::remove_file(peers_dir(store).join(sanitize_host(host)));
}

/// Every peer host whose advertised hold is still within `ttl` of local
/// receipt, pruning the ones that are not. Sorted, so log lines and test
/// assertions are deterministic.
#[must_use]
pub fn live_peer_holds(store: &Path, ttl: Duration) -> Vec<String> {
    let dir = peers_dir(store);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match age_of(&path) {
            // Expired: prune it here so a crashed peer's marker cannot outlive
            // its TTL and wedge every other host's filing.
            Some(age) if age >= ttl => {
                let _ = std::fs::remove_file(&path);
            }
            // Unreadable mtime: cannot age it, so cannot trust it as a live
            // hold either. Dropping it is the fail-OPEN direction, which is
            // the correct one for a soft advisory tier.
            None => {
                let _ = std::fs::remove_file(&path);
            }
            Some(_) => {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    live.push(name.to_string());
                }
            }
        }
    }
    live.sort();
    live
}

// ============================================================================
// Acquire
// ============================================================================

/// What an acquire attempt produced.
#[derive(Debug)]
pub enum AcquireOutcome {
    /// The lock is held. Release happens on drop of the guard.
    Acquired(FilingLockGuard),
    /// The bounded wait expired with the lock still held elsewhere. The caller
    /// **must not file** — it defers its burst to the next tick. `reason` names
    /// the blocker for the caller's log line.
    Deferred { reason: String },
    /// No lock could exist (serialization disabled, or the store is unusable),
    /// or an ancestor already holds it. The caller proceeds.
    DegradedOpen { reason: String },
    /// An ancestor process already holds the lock ([`FILING_LOCK_HELD_ENV`]) —
    /// proceed without taking a second one.
    Reentrant,
}

impl AcquireOutcome {
    /// Whether the caller may proceed to file. `true` for everything except
    /// [`AcquireOutcome::Deferred`].
    #[must_use]
    pub fn may_file(&self) -> bool {
        !matches!(self, Self::Deferred { .. })
    }

    /// A stable one-word identifier for logs/metrics.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Acquired(_) => "acquired",
            Self::Deferred { .. } => "deferred",
            Self::DegradedOpen { .. } => "degraded-open",
            Self::Reentrant => "reentrant",
        }
    }
}

/// RAII hold on the machine-wide filing lock. Releasing (`rmdir`) happens on
/// drop, including on panic or early return.
#[derive(Debug)]
pub struct FilingLockGuard {
    holder: PathBuf,
    label: String,
    acquired_at: Instant,
}

impl FilingLockGuard {
    /// The lock directory this guard holds.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.holder
    }

    /// How long the lock has been held.
    #[must_use]
    pub fn held_for(&self) -> Duration {
        self.acquired_at.elapsed()
    }
}

impl Drop for FilingLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.holder.join("owner.json"));
        let _ = std::fs::remove_dir(&self.holder);
        log::info!(
            "filing_lock: released after {:.2}s ('{}')",
            self.acquired_at.elapsed().as_secs_f64(),
            self.label
        );
    }
}

/// Whether the current holder should be reaped as abandoned at this instant.
///
/// Two independent legs, both required by the "a crashed holder cannot wedge
/// fleet-wide issue creation" property:
///
/// * **Dead owner PID on this host** — decisive and immediate. Only applies
///   when the recorded `host` matches `self_host`; a PID number from another
///   machine says nothing about a process here.
/// * **mtime age past `stale`** — the catch-all for a holder we cannot probe
///   (another host's mirror-write, an unreadable owner record, a killed shell
///   whose PID has since been recycled).
fn holder_is_abandoned(store: &Path, self_host: &str, stale: Duration) -> bool {
    let holder = holder_path(store);
    if let Some(owner) = read_owner(store) {
        if owner.host == self_host
            && owner.pid != 0
            && !crate::live_claim::pid_is_live_process(owner.pid)
        {
            log::warn!(
                "filing_lock: reaping abandoned hold — owner pid {} on this host ({}) is not \
                 running ('{}')",
                owner.pid,
                owner.host,
                owner.label
            );
            return true;
        }
    }
    age_of(&holder).is_some_and(|age| age >= stale)
}

/// Remove an abandoned holder directory (owner record first, then the dir).
fn reap_holder(store: &Path) {
    let holder = holder_path(store);
    let _ = std::fs::remove_file(holder.join("owner.json"));
    let _ = std::fs::remove_dir(&holder);
}

/// Acquire the machine-wide filing lock, resolving every knob from the
/// environment.
///
/// **Blocking** — call from a synchronous context (or `spawn_blocking`), never
/// inline on a tokio runtime worker.
#[must_use]
pub fn acquire(label: &str) -> AcquireOutcome {
    if is_held_here() {
        return AcquireOutcome::Reentrant;
    }
    if !is_enabled() {
        return AcquireOutcome::DegradedOpen {
            reason: format!("{FILING_LOCK_ENABLED_ENV} disables filing-lock serialization"),
        };
    }
    let Some(store) = store_dir() else {
        return AcquireOutcome::DegradedOpen {
            reason: format!("no home directory to resolve the store (set {FILING_LOCK_DIR_ENV})"),
        };
    };
    acquire_in(
        &store,
        &crate::sweep_registry::host_identity(),
        std::process::id(),
        label,
        resolve_wait(),
        DEFAULT_POLL,
        resolve_stale(),
        resolve_peer_ttl(),
    )
}

/// [`acquire`] with every input injected — the testable core (no env reads, no
/// `$HOME`, caller-chosen timings so a test never waits seconds).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn acquire_in(
    store: &Path,
    self_host: &str,
    pid: u32,
    label: &str,
    wait: Duration,
    poll: Duration,
    stale: Duration,
    peer_ttl: Duration,
) -> AcquireOutcome {
    if std::fs::create_dir_all(store).is_err() {
        // Property 3: a store nobody can write is a store nobody is serialized
        // by. Proceeding is exactly as safe as the pre-#6714 world; refusing
        // would turn a corruption risk into a filing outage.
        return AcquireOutcome::DegradedOpen {
            reason: format!("store {} is unusable", store.display()),
        };
    }
    let holder = holder_path(store);
    let started = Instant::now();
    let deadline = started + wait;
    let mut last_blocker;
    let mut logged_wait = false;

    loop {
        // Fleet tier first: a peer host's advertised hold blocks us just like a
        // local holder would, and pruning here is what keeps a crashed peer's
        // marker from outliving its TTL.
        let peers = live_peer_holds(store, peer_ttl);
        if peers.is_empty() {
            match std::fs::create_dir(&holder) {
                Ok(()) => {
                    let owner = FilingLockOwner {
                        host: self_host.to_owned(),
                        pid,
                        label: label.to_owned(),
                        acquired_at: now_epoch_secs(),
                    };
                    // Best-effort: the owner record only enables the dead-PID
                    // reap leg. Losing it costs precision, not correctness —
                    // the mtime leg still bounds the hold.
                    let _ = std::fs::write(holder.join("owner.json"), owner.to_json());
                    log::info!(
                        "filing_lock: acquired for '{label}' after {:.2}s ({})",
                        started.elapsed().as_secs_f64(),
                        holder.display()
                    );
                    return AcquireOutcome::Acquired(FilingLockGuard {
                        holder,
                        label: label.to_owned(),
                        acquired_at: Instant::now(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if holder_is_abandoned(store, self_host, stale) {
                        reap_holder(store);
                        // Retry immediately: a racing peer may have reaped and
                        // re-taken it first, which is an ordinary busy round.
                        continue;
                    }
                    last_blocker = match read_owner(store) {
                        Some(o) => {
                            format!("host {} pid {} is filing ('{}')", o.host, o.pid, o.label)
                        }
                        None => "another filer holds the issue-filing lock".to_string(),
                    };
                }
                Err(e) => {
                    return AcquireOutcome::DegradedOpen {
                        reason: format!("lock path {} is unusable: {e}", holder.display()),
                    };
                }
            }
        } else {
            last_blocker = format!("peer host(s) {} are filing", peers.join(", "));
        }

        if !logged_wait {
            log::info!("filing_lock: '{label}' waiting up to {}s — {last_blocker}", wait.as_secs());
            logged_wait = true;
        }
        if Instant::now() >= deadline {
            // Property 2: fail SAFE, not open. The caller defers its burst.
            let reason = format!(
                "could not acquire the issue-filing lock within {}s ({last_blocker})",
                wait.as_secs()
            );
            log::warn!("filing_lock: DEFERRING '{label}' — {reason}");
            return AcquireOutcome::Deferred { reason };
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn tmp_store(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loom-filing-lock-{name}-{}-{}",
            std::process::id(),
            now_epoch_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn quick(store: &Path, host: &str, pid: u32, label: &str) -> AcquireOutcome {
        acquire_in(
            store,
            host,
            pid,
            label,
            Duration::from_millis(120),
            Duration::from_millis(10),
            Duration::from_secs(300),
            Duration::from_secs(60),
        )
    }

    // ===== basic mutual exclusion =====

    #[test]
    fn acquire_then_release_round_trips() {
        let store = tmp_store("round-trip");
        let outcome = quick(&store, "host-a", 1234, "architect");
        let guard = match outcome {
            AcquireOutcome::Acquired(g) => g,
            other => panic!("expected Acquired, got {}", other.as_str()),
        };
        assert!(holder_path(&store).is_dir());
        let owner = read_owner(&store).unwrap();
        assert_eq!(owner.host, "host-a");
        assert_eq!(owner.pid, 1234);
        assert_eq!(owner.label, "architect");
        drop(guard);
        assert!(!holder_path(&store).exists());
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn second_acquire_defers_while_first_holds() {
        let store = tmp_store("defer");
        let held = match quick(&store, "host-a", std::process::id(), "architect") {
            AcquireOutcome::Acquired(g) => g,
            other => panic!("expected Acquired, got {}", other.as_str()),
        };
        // Property 2: the second filer must NOT proceed unserialized.
        let second = quick(&store, "host-a", std::process::id(), "auditor");
        match &second {
            AcquireOutcome::Deferred { reason } => {
                assert!(
                    reason.contains("architect"),
                    "defer reason should name the blocker: {reason}"
                );
            }
            other => panic!("expected Deferred, got {}", other.as_str()),
        }
        assert!(!second.may_file(), "a deferred filer must not file");
        drop(held);
        // Once released the next filer proceeds.
        assert!(matches!(
            quick(&store, "host-a", std::process::id(), "auditor"),
            AcquireOutcome::Acquired(_)
        ));
        let _ = std::fs::remove_dir_all(&store);
    }

    // ===== property 1: a crashed holder cannot wedge issue creation =====

    #[test]
    fn dead_owner_pid_on_this_host_is_reaped_immediately() {
        let store = tmp_store("dead-pid");
        // Simulate a killed holder: the lock dir and owner record survive, the
        // process does not. PID 0 is never a live user process, and we record a
        // clearly-dead high PID that `pid_is_live_process` rejects.
        std::fs::create_dir_all(holder_path(&store)).unwrap();
        let dead = FilingLockOwner {
            host: "host-a".to_string(),
            // A PID that cannot be alive: 0 is reserved, but we want the
            // owner-record leg, so use a PID far above any plausible live one.
            pid: 4_294_967_294,
            label: "crashed-architect".to_string(),
            acquired_at: now_epoch_secs(),
        };
        std::fs::write(holder_path(&store).join("owner.json"), dead.to_json()).unwrap();

        // Same host ⇒ the dead-PID leg fires and the next filer gets in even
        // though the mtime is brand new (nowhere near the stale threshold).
        match quick(&store, "host-a", std::process::id(), "next-architect") {
            AcquireOutcome::Acquired(g) => {
                assert_eq!(read_owner(&store).unwrap().label, "next-architect");
                drop(g);
            }
            other => panic!("a crashed holder wedged issue creation: {}", other.as_str()),
        }
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn stale_holder_is_reaped_by_mtime_even_without_an_owner_record() {
        let store = tmp_store("stale-mtime");
        std::fs::create_dir_all(holder_path(&store)).unwrap();
        // No owner.json at all — the dead-PID leg cannot fire, so only the
        // mtime leg can free this. A zero-length stale window makes any age
        // qualify without a sleep.
        let outcome = acquire_in(
            &store,
            "host-a",
            std::process::id(),
            "next",
            Duration::from_millis(120),
            Duration::from_millis(10),
            Duration::from_secs(0),
            Duration::from_secs(60),
        );
        assert!(
            matches!(outcome, AcquireOutcome::Acquired(_)),
            "an aged holder with no owner record must be reapable"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn another_hosts_pid_is_never_used_for_liveness() {
        let store = tmp_store("foreign-pid");
        std::fs::create_dir_all(holder_path(&store)).unwrap();
        let foreign = FilingLockOwner {
            host: "host-b".to_string(),
            pid: 4_294_967_294,
            label: "remote-architect".to_string(),
            acquired_at: now_epoch_secs(),
        };
        std::fs::write(holder_path(&store).join("owner.json"), foreign.to_json()).unwrap();
        // A PID number from another machine says nothing about a process here,
        // so the dead-PID leg must NOT fire — only the (not yet reached) mtime
        // leg could free this.
        match quick(&store, "host-a", std::process::id(), "local") {
            AcquireOutcome::Deferred { .. } => {}
            other => panic!("a foreign host's PID was wrongly treated as dead: {}", other.as_str()),
        }
        let _ = std::fs::remove_dir_all(&store);
    }

    // ===== property 3: degrade open, never fail, on an unusable store =====

    #[test]
    fn unusable_store_degrades_open() {
        let store = tmp_store("unusable");
        // A *file* where the store directory must be ⇒ create_dir_all fails.
        let path = store.join("not-a-dir");
        std::fs::write(&path, b"x").unwrap();
        let outcome = quick(&path, "host-a", 1, "architect");
        assert!(
            matches!(outcome, AcquireOutcome::DegradedOpen { .. }),
            "an unusable store must degrade open, not defer: {}",
            outcome.as_str()
        );
        assert!(outcome.may_file());
        let _ = std::fs::remove_dir_all(&store);
    }

    // ===== fleet tier: peer holds =====

    #[test]
    fn a_live_peer_hold_blocks_local_filing_and_expires() {
        let store = tmp_store("peer-hold");
        record_peer_hold(&store, "host-remote");
        assert_eq!(live_peer_holds(&store, Duration::from_secs(60)), vec!["host-remote"]);

        // Live peer hold ⇒ a local filer defers rather than filing alongside it.
        match quick(&store, "host-a", std::process::id(), "architect") {
            AcquireOutcome::Deferred { reason } => {
                assert!(reason.contains("host-remote"), "reason: {reason}");
            }
            other => panic!("expected Deferred, got {}", other.as_str()),
        }

        // Property 1, fleet tier: a peer that crashes without sending
        // FilingUnlock must not wedge us — a zero TTL makes any marker expired.
        assert!(live_peer_holds(&store, Duration::from_secs(0)).is_empty());
        let outcome = acquire_in(
            &store,
            "host-a",
            std::process::id(),
            "architect",
            Duration::from_millis(120),
            Duration::from_millis(10),
            Duration::from_secs(300),
            Duration::from_secs(0),
        );
        assert!(
            matches!(outcome, AcquireOutcome::Acquired(_)),
            "an expired peer hold must not wedge filing: {}",
            outcome.as_str()
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn clearing_a_peer_hold_unblocks_immediately() {
        let store = tmp_store("peer-clear");
        record_peer_hold(&store, "host-remote");
        clear_peer_hold(&store, "host-remote");
        assert!(live_peer_holds(&store, Duration::from_secs(60)).is_empty());
        assert!(matches!(
            quick(&store, "host-a", std::process::id(), "architect"),
            AcquireOutcome::Acquired(_)
        ));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn peer_host_names_cannot_escape_the_peers_directory() {
        assert_eq!(sanitize_host("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_host(".."), "_");
        assert_eq!(sanitize_host(""), "_");
        assert_eq!(sanitize_host("host-e1d4c843"), "host-e1d4c843");
    }

    // ===== the #6714 regression: two filers in DIFFERENT repos =====

    /// The 2026-08-08 shape, mechanized: two issue-creating agents working
    /// **different repos** file concurrently through one shared scratch path
    /// (the only way two processes can swap each other's body text). Without
    /// serialization the second filer's body overwrites the first's between the
    /// first's write and read — the off-by-one body swap that corrupted
    /// gf180-sram #6–#10 with sky130-modexp's bodies.
    ///
    /// Asserts the AC's exact property: **each filed issue's body matches the
    /// request that produced it.**
    #[test]
    fn concurrent_filers_in_different_repos_never_cross_contaminate_bodies() {
        let store = tmp_store("cross-repo");
        // The shared mutable state a real burst has: one scratch body path per
        // host (a fixed `/tmp/...` body file), read back at `gh issue create`
        // time.
        let scratch = store.join("scratch-body.md");
        // The "forge": what each filer actually filed, in order.
        let filed: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let deferred = Arc::new(AtomicUsize::new(0));

        let repos = [
            ("gf180-sram", "SRAM macro: characterize the 512x8 bitcell array"),
            ("sky130-modexp", "RTL synthesis: rtl/modexp.v timing closure"),
        ];

        let mut handles = Vec::new();
        for (repo, body) in repos {
            let store = store.clone();
            let scratch = scratch.clone();
            let filed = Arc::clone(&filed);
            let deferred = Arc::clone(&deferred);
            handles.push(std::thread::spawn(move || {
                for i in 0..10 {
                    let want = format!("{body} (#{i})");
                    let outcome = acquire_in(
                        &store,
                        "host-a",
                        std::process::id(),
                        repo,
                        Duration::from_secs(10),
                        Duration::from_millis(1),
                        Duration::from_secs(300),
                        Duration::from_secs(60),
                    );
                    let guard = match outcome {
                        AcquireOutcome::Acquired(g) => g,
                        AcquireOutcome::Deferred { .. } => {
                            deferred.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                        other => panic!("unexpected outcome {}", other.as_str()),
                    };
                    // --- critical section: the filing burst ---
                    std::fs::write(&scratch, want.as_bytes()).unwrap();
                    // Widen the window a racing filer would exploit.
                    std::thread::yield_now();
                    std::thread::sleep(Duration::from_millis(1));
                    let read_back = std::fs::read_to_string(&scratch).unwrap();
                    filed.lock().unwrap().push((repo.to_string(), read_back));
                    drop(guard);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let filed = filed.lock().unwrap();
        assert_eq!(
            deferred.load(Ordering::SeqCst),
            0,
            "the bounded wait was generous enough that nothing should have deferred"
        );
        assert_eq!(filed.len(), 20, "every burst entry should have filed");
        for (repo, body) in filed.iter() {
            let expected_prefix = repos
                .iter()
                .find(|(r, _)| r == repo)
                .map(|(_, b)| *b)
                .unwrap();
            assert!(
                body.starts_with(expected_prefix),
                "issue filed against {repo} carries another repo's body: {body:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&store);
    }

    /// The unserialized control, made **deterministic** rather than
    /// probabilistic: the interleaving that corrupted gf180-sram is forced with
    /// barriers, so this proves the hazard shape is real without ever being a
    /// flaky timing test.
    ///
    /// Sequence: filer A (gf180-sram) writes its body → filer B
    /// (sky130-modexp) writes its body over the shared scratch path → A reads
    /// back and files **B's** body under A's title. That is exactly the
    /// one-directional, off-by-one swap the incident produced. The test above
    /// runs the same shape *through the lock* and shows it cannot happen.
    #[test]
    fn without_the_lock_the_forced_interleaving_cross_contaminates() {
        let store = tmp_store("cross-repo-unlocked");
        let scratch = store.join("scratch-body.md");
        let a_wrote = Arc::new(std::sync::Barrier::new(2));
        let b_wrote = Arc::new(std::sync::Barrier::new(2));

        let a = {
            let scratch = scratch.clone();
            let a_wrote = Arc::clone(&a_wrote);
            let b_wrote = Arc::clone(&b_wrote);
            std::thread::spawn(move || {
                std::fs::write(&scratch, b"SRAM macro: 512x8 bitcell array").unwrap();
                a_wrote.wait(); // let B in — the missing mutual exclusion
                b_wrote.wait(); // B has now overwritten the shared scratch
                std::fs::read_to_string(&scratch).unwrap()
            })
        };
        let b = {
            let scratch = scratch.clone();
            let a_wrote = Arc::clone(&a_wrote);
            let b_wrote = Arc::clone(&b_wrote);
            std::thread::spawn(move || {
                a_wrote.wait();
                std::fs::write(&scratch, b"RTL synthesis: rtl/modexp.v timing closure").unwrap();
                b_wrote.wait();
            })
        };
        let a_filed = a.join().unwrap();
        b.join().unwrap();

        assert!(
            a_filed.starts_with("RTL synthesis"),
            "the unserialized control did not reproduce cross-contamination — the \
             serialized test above would then prove nothing (got {a_filed:?})"
        );
        let _ = std::fs::remove_dir_all(&store);
    }
}
