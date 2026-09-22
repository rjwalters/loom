//! The **per-host backstop ceiling** — an admission bound on how much of the
//! queue may land on a metered, pay-per-token tap at once (Issue #8555).
//!
//! # Why the backstop tier needs a bound the others do not
//!
//! Every other tap in a preference list is **flat rate**: a Claude
//! subscription, a Codex seat. Overusing one costs nothing extra — it exhausts
//! on a plan limit and recovers on a clock, which is exactly what
//! [`super::availability`] already models. The backstop tap is the one entry
//! with a **marginal cost**, and it has the opposite failure mode: it
//! effectively never exhausts, so nothing stops it. An all-day Claude outage
//! routes the *entire* backlog through it, unnoticed, because falling through
//! is precisely what the resolver is supposed to do.
//!
//! So the ceiling here is a **spend ceiling**, and is deliberately NOT modelled
//! as a bad-mark/cooldown (the shape `tokens_pool` and `api_keys_pool` use):
//! a cooldown says "this credential is temporarily unusable and will heal",
//! which is false of a metered endpoint. It says "this host may hold at most N
//! metered dispatches at once", which is true of one.
//!
//! # It is a resource bound, never an approval gate
//!
//! When the ceiling refuses, the tap behaves **exactly as an unavailable tap**:
//! the walk records a [`SkipReason::Ceiling`] and continues to the next tap,
//! failing closed (no dispatch) if nothing below it qualifies. Nothing waits on
//! a human, nothing is queued for approval, and a recovered higher tap is still
//! preferred the moment it can serve the work. The bound is sized by machine
//! resources and spend — never by token counts, per the standing direction
//! behind #5270 (`capacity.rs` removed the token axis for the same reason).
//!
//! # Per-host, so the state is machine-wide
//!
//! Like [`crate::build_slot`]'s slots, leases live at
//! `~/.loom/leases/backstop/` (override: [`LEASE_DIR_ENV`]), *not* under a
//! repo's `.loom/`: a host runs several workspaces and one ceiling governs all
//! of them. A **fleet-wide** ceiling over a metered key shared between hosts is
//! explicitly out of scope — per-host state cannot govern a shared credential;
//! that needs provider-side budget controls (issue #8556).
//!
//! # Lease shape: PID-liveness, but attached to the spawned worker
//!
//! One JSON file per live backstop dispatch, counted with
//! [`crate::live_claim::pid_is_live_process`] and reaped lazily — the same
//! primitive as [`crate::api_keys_pool::inflight`]. One thing differs, and it
//! matters: `inflight` can key a lease on the *selecting* process because a
//! native harness spawn `exec`s, so selection and run share a PID. Dispatch
//! does not exec — the daemon selects and then **spawns a child** that outlives
//! the selection — so a lease keyed on the daemon's PID would be immortal.
//!
//! The lifecycle is therefore two-step:
//!
//! 1. [`admit`] takes a **reservation** under a `mkdir` control lock (count and
//!    write are atomic against a peer doing the same), owned by the resolving
//!    process and short-lived ([`DEFAULT_RESERVATION_STALE_SECS`]).
//! 2. The launch path calls [`Reservation::attach`] with the spawned worker's
//!    PID, converting it into a lease that lives exactly as long as that
//!    process (backstopped by [`DEFAULT_LEASE_STALE_SECS`]).
//!
//! A reservation that is never attached is released **on drop**, so a caller
//! that resolves and then decides not to launch — or panics — cannot leak a
//! slot. (This is the opposite of `inflight`'s deliberately non-RAII lease, and
//! for the opposite reason: there, dropping happens microseconds before `exec`
//! hands the PID over; here, dropping means the launch never happened.)
//!
//! # Fails closed, unlike the in-flight counter it borrows from
//!
//! `api_keys_pool::inflight` degrades **open** when its store is unusable: it
//! is a politeness throttle, and refusing to spawn over a broken counter
//! directory would convert a throttle into an outage. This one degrades
//! **closed** ([`CeilingSkip::Unknown`]) — an unknown metered-concurrency count
//! must never read as "there is room", because the cost of guessing wrong is
//! real money rather than lost politeness, and the fallback is merely to prefer
//! some other tap or hold. This mirrors
//! [`crate::api_keys_pool::limits`]'s rule for the *declared* half of the same
//! problem: absent means unbounded, unreadable means unknown.
//!
//! **Absent config reads nothing at all.** With no `runtimes.backstopCeiling`
//! key and no [`MAX_CONCURRENT_ENV`] override, no directory is created, no
//! lease is written, and resolution is byte-identical to a build without this
//! module.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::resolve::{CeilingSkip, SkipReason, Tap};
use crate::tokens_pool::locking::MkdirLock;

/// Env override for the concurrency ceiling (`env > config > default`, the
/// daemon's standing precedence). Must parse as a non-negative integer;
/// anything else is ignored in favour of the configured value.
pub const MAX_CONCURRENT_ENV: &str = "LOOM_BACKSTOP_MAX_CONCURRENT";

/// Override for the machine-wide lease directory (default
/// `~/.loom/leases/backstop`). Primarily a test seam; also lets an operator
/// relocate the state onto a specific filesystem.
pub const LEASE_DIR_ENV: &str = "LOOM_BACKSTOP_LEASE_DIR";

/// Env override (whole seconds, must parse `> 0`) for
/// [`DEFAULT_LEASE_STALE_SECS`].
pub const LEASE_STALE_SECS_ENV: &str = "LOOM_BACKSTOP_LEASE_STALE_SECS";

/// Age backstop for an **attached** lease whose owner PID cannot be trusted
/// (recycled, or never recorded): 4 hours, matching
/// [`crate::api_keys_pool::inflight::DEFAULT_STALE_SECS`]. A lease describes a
/// whole sweep, so a short threshold would let a peer over-admit against a
/// dispatch that is genuinely still running — the ceiling breach this module
/// exists to prevent.
pub const DEFAULT_LEASE_STALE_SECS: u64 = 14_400;

/// Age backstop for an **unattached** reservation: 5 minutes. Short on purpose
/// — the window between resolving a runtime and spawning the worker is
/// seconds, so a reservation still unattached minutes later belongs to a
/// launch that died between the two steps, and holding a metered slot for it
/// would throttle the host for nothing.
pub const DEFAULT_RESERVATION_STALE_SECS: u64 = 300;

/// Default first tier the ceiling governs: `1`, i.e. every tap the walk *falls
/// through* to — the same predicate [`super::Resolution::fell_through`] uses.
pub const DEFAULT_APPLIES_FROM: usize = 1;

/// Lock directory serialising count-then-reserve against a peer.
const CONTROL_LOCK: &str = ".ceiling.lock";

// ---------------------------------------------------------------------------
// Complexity eligibility
// ---------------------------------------------------------------------------

/// The Curator's `<!-- loom:complexity=<tier> -->` strata, ordered.
///
/// The optional eligibility filter is what keeps *low-value* work off the
/// metered tap entirely: a mechanical one-line fix is not worth paying
/// per-token for when the free tiers are down, while a complex change may well
/// be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ComplexityTier {
    Mechanical,
    Routine,
    Complex,
}

impl ComplexityTier {
    /// The vocabulary `require-complexity-marker.sh` enforces. Unknown text is
    /// `None` — callers treat that as "unmarked", never as an error, because a
    /// marker is a Curator convention and dispatch must not fail on its
    /// absence.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "mechanical" => Some(Self::Mechanical),
            "routine" => Some(Self::Routine),
            "complex" => Some(Self::Complex),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mechanical => "mechanical",
            Self::Routine => "routine",
            Self::Complex => "complex",
        }
    }

    /// The tier an unmarked dispatch is treated as: `routine`, the same
    /// default the rest of the daemon applies to a missing marker (see
    /// `work_finder`'s `complexity.unwrap_or("routine")`). Deliberately *not*
    /// the lowest tier — an unmarked issue is ordinary work, not proven
    /// low-value, and treating it as `mechanical` would silently exclude most
    /// of the queue from the backstop the moment a filter is configured.
    #[must_use]
    pub fn of(complexity: Option<&str>) -> Self {
        complexity.and_then(Self::parse).unwrap_or(Self::Routine)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The configured bound, as `runtimes.backstopCeiling` declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackstopCeiling {
    /// Most concurrent backstop-tier dispatches this host may hold. `None` is
    /// unbounded (no lease store is touched); `Some(0)` switches the metered
    /// tier off entirely without having to edit the preference list.
    pub max_concurrent: Option<u32>,
    /// First tier the ceiling governs. `1` (the default) is "every tap below
    /// the most-preferred one". A fleet whose tier 1 is another flat-rate
    /// subscription (a Codex seat) raises this so the ceiling starts at the
    /// tier that actually costs per token.
    pub applies_from: usize,
    /// Least complexity tier allowed to reach a governed tap. `None` is "every
    /// tier is eligible".
    pub min_complexity: Option<ComplexityTier>,
}

impl BackstopCeiling {
    /// Nothing to enforce: no count and no filter. Such a ceiling is reported
    /// as absent so the no-ceiling fast path stays byte-identical.
    #[must_use]
    fn is_inert(&self) -> bool {
        self.max_concurrent.is_none() && self.min_complexity.is_none()
    }

    /// Does this ceiling govern `tier`?
    #[must_use]
    pub fn governs(&self, tier: usize) -> bool {
        tier >= self.applies_from
    }
}

fn env_max_concurrent() -> Option<u32> {
    std::env::var(MAX_CONCURRENT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
}

/// Parse `runtimes.backstopCeiling`, applying the [`MAX_CONCURRENT_ENV`]
/// override on top of it.
///
/// `Ok(None)` means "no ceiling" — the fast path that reads and writes
/// nothing.
///
/// Shape validation is **fail-closed**, in the same spirit as
/// [`super::preference_for`]: an unknown key, a wrong type, or a bad tier name
/// is an error rather than a silently-dropped bound. A spend ceiling that
/// degraded silently to "unbounded" on a typo would fail in exactly the
/// direction this feature exists to prevent.
///
/// # Errors
/// A formatted message naming the offending key.
pub fn configured(config: &Value) -> Result<Option<BackstopCeiling>, String> {
    let mut ceiling = match crate::config_resolver::get_path(config, "runtimes.backstopCeiling") {
        None | Some(Value::Null) => BackstopCeiling {
            max_concurrent: None,
            applies_from: DEFAULT_APPLIES_FROM,
            min_complexity: None,
        },
        Some(value) => parse(value)?,
    };
    if let Some(from_env) = env_max_concurrent() {
        ceiling.max_concurrent = Some(from_env);
    }
    Ok((!ceiling.is_inert()).then_some(ceiling))
}

const PATH: &str = "runtimes.backstopCeiling";
const KNOWN_KEYS: [&str; 3] = ["maxConcurrent", "appliesFrom", "minComplexity"];

fn parse(value: &Value) -> Result<BackstopCeiling, String> {
    let Some(map) = value.as_object() else {
        return Err(format!("{PATH} must be an object (known keys: {})", KNOWN_KEYS.join(", ")));
    };
    let unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|key| !KNOWN_KEYS.contains(key))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{PATH} has unknown key(s): {} (known: {})",
            unknown.join(", "),
            KNOWN_KEYS.join(", ")
        ));
    }
    let max_concurrent = match map.get("maxConcurrent") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(
            raw.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    format!("{PATH}.maxConcurrent must be a non-negative integer (0 switches the backstop tier off)")
                })?,
        ),
    };
    let applies_from = match map.get("appliesFrom") {
        None | Some(Value::Null) => DEFAULT_APPLIES_FROM,
        Some(raw) => {
            let tier = raw
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| {
                    format!("{PATH}.appliesFrom must be a non-negative integer tier index")
                })?;
            if tier == 0 {
                // Tier 0 is the most-preferred tap — the subscription the
                // fleet already pays for. A "backstop" ceiling that bounded it
                // would throttle free capacity, which is the exact inversion
                // of this feature's purpose.
                return Err(format!(
                    "{PATH}.appliesFrom must be >= 1: tier 0 is the most-preferred tap, and a \
                     backstop ceiling must never bound it. Remove a tap from the preference list \
                     to stop using it."
                ));
            }
            tier
        }
    };
    let min_complexity = match map.get("minComplexity") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => Some(ComplexityTier::parse(raw).ok_or_else(|| {
            format!(
                "{PATH}.minComplexity must be one of mechanical, routine, complex (got {raw:?})"
            )
        })?),
        Some(_) => {
            return Err(format!(
                "{PATH}.minComplexity must be a string (mechanical, routine, complex)"
            ))
        }
    };
    Ok(BackstopCeiling {
        max_concurrent,
        applies_from,
        min_complexity,
    })
}

/// Proactively check a resolved config's ceiling key, for `loom-daemon
/// validate` — the counterpart of
/// [`super::check_runtimes_preference_config`].
#[must_use]
pub fn check_config(config: &Value) -> Vec<String> {
    match configured(config) {
        Ok(_) => Vec::new(),
        Err(message) => vec![format!("runtimes backstop ceiling: {message}")],
    }
}

// ---------------------------------------------------------------------------
// The lease store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Holder {
    /// The process whose liveness keeps this lease alive: the resolving
    /// process while unattached, the spawned worker once attached.
    pid: u32,
    started_at: u64,
    /// `false` until [`Reservation::attach`] hands the lease to the worker.
    /// Unattached leases age out on the much shorter reservation threshold.
    attached: bool,
    /// Diagnostics only — which tap and role took the slot.
    tap: String,
    role: String,
}

fn epoch_now() -> u64 {
    crate::api_keys_pool::bad_marks::epoch_now()
}

fn resolve_stale(attached: bool) -> u64 {
    if !attached {
        return DEFAULT_RESERVATION_STALE_SECS;
    }
    std::env::var(LEASE_STALE_SECS_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_LEASE_STALE_SECS)
}

/// The machine-wide lease directory, or `None` when neither
/// [`LEASE_DIR_ENV`] nor a home directory resolves.
#[must_use]
pub fn lease_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(LEASE_DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    Some(
        dirs::home_dir()?
            .join(".loom")
            .join("leases")
            .join("backstop"),
    )
}

/// How many live backstop dispatches this host currently holds, reaping dead
/// and over-age leases as it counts.
///
/// # Errors
/// An existing directory that cannot be read. An **absent** directory is
/// `Ok(0)` — nothing has ever taken a slot on this host.
pub fn live_count(dir: &Path) -> Result<u32, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("cannot read {} ({:?})", dir.display(), e.kind())),
    };
    let now = epoch_now();
    let mut live = 0u32;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let holder = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| serde_json::from_str::<Holder>(&body).ok());
        let alive = match &holder {
            // An unparsable lease file proves a dispatch touched this host but
            // nothing about whose PID, so it is reaped on age alone — counted
            // meanwhile, because over-counting is the safe direction for a
            // spend ceiling.
            None => path
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() < resolve_stale(true)),
            Some(holder) => {
                crate::live_claim::pid_is_live_process(holder.pid)
                    && now.saturating_sub(holder.started_at) < resolve_stale(holder.attached)
            }
        };
        if alive {
            live = live.saturating_add(1);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(live)
}

/// A held backstop slot. **Released on drop** until [`Self::attach`] hands it
/// to the spawned worker — see the module docs.
#[derive(Debug)]
pub struct Reservation {
    path: PathBuf,
    attached: bool,
    live: u32,
    limit: u32,
}

impl Reservation {
    /// The lease file, for a caller that wants to log or assert on it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many slots were held *including* this one, and the ceiling.
    #[must_use]
    pub fn usage(&self) -> (u32, u32) {
        (self.live, self.limit)
    }

    /// `live/limit`, for the preference log marker.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{}/{}", self.live, self.limit)
    }

    /// Hand the slot to the spawned worker, whose liveness now governs it.
    ///
    /// # Errors
    /// The lease file could not be rewritten. The reservation is released
    /// (ages out within [`DEFAULT_RESERVATION_STALE_SECS`] at worst), so the
    /// launch proceeds **uncounted** rather than being killed after the fact —
    /// the one place this module degrades open, because refusing a dispatch
    /// that is already running achieves nothing.
    pub fn attach(mut self, pid: u32) -> Result<(), String> {
        let body = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("cannot read {}: {e}", self.path.display()))?;
        let mut holder: Holder = serde_json::from_str(&body)
            .map_err(|e| format!("cannot parse {}: {e}", self.path.display()))?;
        holder.pid = pid;
        holder.attached = true;
        holder.started_at = epoch_now();
        let body = serde_json::to_string(&holder).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, body)
            .map_err(|e| format!("cannot update {}: {e}", self.path.display()))?;
        self.attached = true;
        Ok(())
    }

    /// Release explicitly. Identical to dropping it; spelled out for a caller
    /// that wants the intent in the code.
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.attached {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The outcome of asking the ceiling about one governed tap.
#[derive(Debug)]
pub enum Verdict {
    /// No count applies (no `maxConcurrent`), so no slot is held and nothing
    /// was written.
    Unbounded,
    /// Admitted; the reservation holds a slot until attached or dropped.
    Admitted(Reservation),
    /// Passed over, with the reason to record on the skipped tap.
    Refused(SkipReason),
}

fn refuse(skip: CeilingSkip, detail: String) -> Verdict {
    Verdict::Refused(SkipReason::Ceiling { skip, detail })
}

/// Ask the ceiling whether this dispatch may take a governed (backstop) tap.
///
/// `complexity` is the issue's `<!-- loom:complexity=<tier> -->` stratum, as
/// dispatch already carries it; `None` is treated as `routine` (see
/// [`ComplexityTier::of`]).
///
/// Counting and reserving happen under a `mkdir` control lock so two
/// concurrent dispatches on the same host cannot both read `live == limit - 1`
/// and both admit.
#[must_use]
pub fn admit(
    ceiling: &BackstopCeiling,
    role: &str,
    tap: &Tap,
    complexity: Option<&str>,
) -> Verdict {
    if let Some(minimum) = ceiling.min_complexity {
        let tier = ComplexityTier::of(complexity);
        if tier < minimum {
            return refuse(
                CeilingSkip::Ineligible,
                format!(
                    "{} work is below the metered tier's minimum complexity ({})",
                    tier.as_str(),
                    minimum.as_str()
                ),
            );
        }
    }
    let Some(limit) = ceiling.max_concurrent else {
        return Verdict::Unbounded;
    };
    if limit == 0 {
        return refuse(
            CeilingSkip::AtCapacity { live: 0, limit: 0 },
            "the backstop ceiling is 0 — the metered tier is switched off on this host".to_string(),
        );
    }
    let Some(dir) = lease_dir() else {
        return refuse(
            CeilingSkip::Unknown,
            format!("no home directory to resolve the backstop lease dir (set {LEASE_DIR_ENV})"),
        );
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return refuse(
            CeilingSkip::Unknown,
            format!("cannot create the backstop lease dir {}: {e}", dir.display()),
        );
    }
    let Ok(_lock) = MkdirLock::acquire(&dir.join(CONTROL_LOCK)) else {
        return refuse(
            CeilingSkip::Unknown,
            format!("cannot lock {}", dir.join(CONTROL_LOCK).display()),
        );
    };
    let live = match live_count(&dir) {
        Ok(live) => live,
        Err(detail) => return refuse(CeilingSkip::Unknown, detail),
    };
    if live >= limit {
        return refuse(
            CeilingSkip::AtCapacity { live, limit },
            format!("{live}/{limit} concurrent backstop dispatches already held on this host"),
        );
    }
    let holder = Holder {
        pid: std::process::id(),
        started_at: epoch_now(),
        attached: false,
        tap: tap.to_string(),
        role: role.to_string(),
    };
    let path = dir.join(format!("{}-{}.json", std::process::id(), uuid::Uuid::new_v4()));
    let Ok(body) = serde_json::to_string(&holder) else {
        return refuse(CeilingSkip::Unknown, "cannot serialize a backstop lease".to_string());
    };
    if let Err(e) = std::fs::write(&path, body) {
        return refuse(
            CeilingSkip::Unknown,
            format!("cannot write the backstop lease {}: {e}", path.display()),
        );
    }
    Verdict::Admitted(Reservation {
        path,
        attached: false,
        live: live + 1,
        limit,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
