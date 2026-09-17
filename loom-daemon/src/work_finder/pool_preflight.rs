//! Host-level token-pool exhaustion hold for the work finder (Issue #7708).
//!
//! # The failure this exists to stop
//!
//! When every account in the pool a sweep would actually spawn from is
//! bad-marked (`.bad_tokens`) or `.ranking`-hard-excluded, `spawn-claude.sh`'s
//! own token-selection step exits `78` (`EX_CONFIG`) ~25–35 s in, *before*
//! the CLI is ever exec'd. Nothing useful happened — but the dispatch that
//! produced it had already flipped `loom:issue` → `loom:building` and posted
//! a `loom:lease` record comment on a real issue.
//!
//! Before this module the work finder had no way to know that in advance. Its
//! only view of pool health was `.ranking` (`capacity::read_ranking` →
//! `available`), which does **not** consult `.bad_tokens` TTL marks at all —
//! so a pool whose `.ranking` still reported six `available` accounts, every
//! one of them carrying a live six-hour exhaustion cooldown, looked perfectly
//! healthy to the dispatcher. The observed consequence (2026-09-15, four
//! fleet hosts, 4.3 h): 228 insta-crashes, 20 dispatches of a single issue,
//! and 39 permanent lease-record comments on one public issue.
//!
//! # Why a *pool*-keyed hold, and not more per-issue backoff
//!
//! The #4485 per-issue dispatch-backoff ladder *did* arm on every one of
//! those deaths, and it structurally cannot fix this shape: it is keyed per
//! issue, capped at 900 s, so ~10 ready issues each behaving perfectly still
//! aggregate to ~40 doomed spawns an hour. A pool-wide fault needs a
//! pool-wide hold. This module is that hold — keyed by the **resolved pool
//! directory**, so every workspace root whose
//! [`resolve_tokens_dir`](crate::tokens_pool::paths::resolve_tokens_dir)
//! lands on the same directory shares one hold and one log line, and a daemon
//! owning one host therefore holds that host.
//!
//! # Two ways a hold arms
//!
//! 1. **Pre-flight (the primary path).** Every tick, [`PoolHoldState::observe_root`]
//!    re-reads the root's effective pool through
//!    [`spawnable_pool_state`] — the *same* resolution and usability logic
//!    `spawn-claude.sh`'s `loom-daemon tokens select` performs, so this
//!    pre-flight can never disagree with what a real spawn would discover.
//!    `total > 0 && usable == 0` ⇒ hold. This is the exact shape #7607/#7621
//!    added one level down for role ticks; it is deliberately reused rather
//!    than forked.
//! 2. **Post-mortem (the backstop).** A sweep that nonetheless died at token
//!    selection proves the pool was unspawnable *whatever* this daemon's own
//!    read said. The reaper calls [`PoolHoldState::note_pool_dead`], which
//!    arms a hold that outranks the live read until its TTL expires. This is
//!    what bounds the divergence case — the one where the wrapper and the
//!    daemon resolve pool health differently — to at most one doomed dispatch
//!    per host per TTL instead of one per tick.
//!
//! # Self-healing, by construction
//!
//! Nothing here caches a verdict. A pre-flight hold is re-derived from the
//! live pool on every tick, so an operator readmitting one account (or a
//! `.bad_tokens` cooldown simply aging out) clears the hold on the very next
//! tick with no restart and no manual action — the same property
//! `.loom/docs/token-pool.md` § "Role ticks pre-flight the pool instead of
//! spawning into it" describes for #7607. A post-mortem hold expires on its
//! own TTL, which is bounded by
//! [`pool_clear_estimate`]'s own 900 s cap.
//!
//! # What this is NOT
//!
//! - **Not a per-issue brake.** It never consults, arms, or clears the #4485
//!   ladder, the #3939 quarantine tally, or any issue-scoped state. A pool
//!   fault is not the issue's fault and must never be reported as one.
//!   (The reaper's own carve-out for this is in `sweep_registry::reaper`.)
//! - **Not an empty-pool check.** `total == 0` — no pool provisioned at all —
//!   is a different condition with a different fix (`loom-daemon tokens
//!   bootstrap`) and its own detection (#4642). This module holds only for
//!   "a pool exists and every account in it is unspawnable".
//! - **Not broadcast to peers.** Each host resolves its *own* pool (repo-local
//!   shadow pool if it holds `.token` files, else shared — #3938/#7527), so
//!   one host's exhaustion says nothing about a peer's. Broadcasting it would
//!   suppress a peer whose pool is healthy.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::sweep_registry::PreflightDispatchGate;
use crate::tokens_pool::select::{pool_clear_estimate, spawnable_pool_state};
use crate::workspace_pool::WorkspacePool;

/// A live "this pool cannot spawn anything" hold, keyed by resolved pool
/// directory (see the module doc for why the pool directory, not the
/// workspace root, is the key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolExhaustionHold {
    /// The resolved pool directory this hold covers — the same directory
    /// `spawn-claude.sh` would select an account from for any root that
    /// resolves here.
    pub dir: PathBuf,
    /// Total `*.token` files in [`dir`](Self::dir) when the hold was last
    /// refreshed. Always `> 0` for a pre-flight-armed hold; `0` is possible
    /// for a post-mortem hold whose pool has since been emptied.
    pub total: usize,
    /// When this hold first armed. Preserved across refreshes so an operator
    /// can see how long the pool has been dead, not just when it was last
    /// re-observed.
    pub since: DateTime<Utc>,
    /// Best-effort estimate of when the pool might regain a spawnable
    /// account, from [`pool_clear_estimate`]. For a pre-flight hold this is
    /// **diagnostic only** (the next tick's live read decides); for a
    /// post-mortem hold it is the actual TTL, which is why it is capped.
    pub next_clear_at: DateTime<Utc>,
    /// `true` when a real sweep's death at token selection armed (or last
    /// refreshed) this hold. Such a hold outranks the live pre-flight read
    /// until [`next_clear_at`](Self::next_clear_at): the wrapper proved a
    /// spawn cannot select an account, which is stronger evidence than this
    /// daemon's own read of the same directory.
    pub wrapper_observed: bool,
}

/// The set of currently-held pools.
///
/// Production uses the process-global [`PoolHoldState::global`]; the type is
/// public and independently constructible so a test can model **N separate
/// hosts** in one process (each host being one daemon, i.e. one
/// `PoolHoldState`) — which is exactly what the #7708 incident's four-host
/// shape needs to reproduce.
#[derive(Debug, Default)]
pub struct PoolHoldState {
    holds: Mutex<HashMap<PathBuf, PoolExhaustionHold>>,
}

impl PoolHoldState {
    /// A fresh, empty hold set — one simulated host.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-global hold set: this daemon, i.e. this host.
    #[must_use]
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<PoolHoldState> = OnceLock::new();
        GLOBAL.get_or_init(PoolHoldState::new)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, PoolExhaustionHold>> {
        self.holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Re-derive `root`'s pool-exhaustion verdict from the live pool and
    /// update this host's hold set accordingly. Returns `true` when dispatch
    /// to `root` must be held this tick.
    ///
    /// Arming and clearing are **edge-logged exactly once each** — one WARN
    /// when a pool first becomes unspawnable, one INFO when it recovers —
    /// never once per tick. That is the "logs one hold line" the issue asks
    /// for, and it is what keeps a multi-hour outage from producing one log
    /// line per tick per root.
    pub fn observe_root(&self, root: &Path, now: DateTime<Utc>) -> bool {
        let pool = spawnable_pool_state(root);
        // `total == 0` is the ABSENT-pool condition (#4642), not this one —
        // see the module doc. Falling through to the clear path below is
        // deliberate: a pool that was drained to empty should not keep a
        // pre-flight hold alive under a stale key.
        let exhausted = pool.total > 0 && pool.usable == 0;
        let mut holds = self.lock();

        if exhausted {
            let next_clear_at = pool_clear_estimate(&pool.dir);
            match holds.entry(pool.dir.clone()) {
                Entry::Occupied(mut existing) => {
                    let hold = existing.get_mut();
                    hold.total = pool.total;
                    hold.next_clear_at = next_clear_at;
                }
                Entry::Vacant(slot) => {
                    log::warn!(
                        "work_finder: token pool {} is EXHAUSTED — 0/{} accounts spawnable \
                         (every account bad-marked in .bad_tokens or hard-excluded by \
                         .ranking). Holding ALL sweep dispatch for every workspace resolving \
                         to this pool until at least one account returns (~{}); no claim label \
                         will be flipped and no lease comment posted while held. Run \
                         `loom-daemon tokens check --ranking` or `loom-daemon tokens unblock \
                         <name>` (#7708)",
                        pool.dir.display(),
                        pool.total,
                        next_clear_at.to_rfc3339()
                    );
                    slot.insert(PoolExhaustionHold {
                        dir: pool.dir.clone(),
                        total: pool.total,
                        since: now,
                        next_clear_at,
                        wrapper_observed: false,
                    });
                }
            }
            return true;
        }

        // The live read says at least one account is spawnable. A hold armed
        // by a REAL token-selection death outranks that read until its TTL:
        // the wrapper proved a spawn cannot select an account here, and the
        // whole reason #7708 happened is that the daemon's read and the
        // wrapper's can disagree. Everything else clears immediately — that
        // is the one-tick self-heal.
        match holds.get(&pool.dir) {
            Some(hold) if hold.wrapper_observed && now < hold.next_clear_at => true,
            Some(hold) => {
                log::info!(
                    "work_finder: token pool {} recovered — {}/{} accounts spawnable after {} \
                     held; sweep dispatch resuming (#7708)",
                    pool.dir.display(),
                    pool.usable,
                    pool.total,
                    format_held_for(now - hold.since)
                );
                holds.remove(&pool.dir);
                false
            }
            None => false,
        }
    }

    /// Record that a real sweep spawned from `root`'s pool died in
    /// `spawn-claude.sh`'s token-selection step — the post-mortem arming path
    /// described in the module doc.
    ///
    /// Unlike [`observe_root`](Self::observe_root) this trusts the wrapper
    /// over this daemon's own read of the same directory, so the resulting
    /// hold survives a live read that claims the pool is fine. It is bounded:
    /// [`pool_clear_estimate`] never reports further than 900 s out, so the
    /// worst case is one doomed dispatch per host per 15 minutes — not one
    /// per tick.
    pub fn note_pool_dead(&self, root: &Path, now: DateTime<Utc>) {
        let pool = spawnable_pool_state(root);
        let next_clear_at = pool_clear_estimate(&pool.dir);
        let mut holds = self.lock();
        match holds.entry(pool.dir.clone()) {
            Entry::Occupied(mut existing) => {
                let hold = existing.get_mut();
                hold.total = pool.total;
                hold.next_clear_at = next_clear_at;
                hold.wrapper_observed = true;
            }
            Entry::Vacant(slot) => {
                log::warn!(
                    "work_finder: a sweep died in spawn-claude.sh's TOKEN SELECTION step — pool \
                     {} held no usable account, so none was ever selected. This daemon's own \
                     read of that pool reports {}/{} spawnable, so the two disagree; trusting \
                     the wrapper and holding sweep dispatch for every workspace resolving to \
                     this pool until ~{} (#7708)",
                    pool.dir.display(),
                    pool.usable,
                    pool.total,
                    next_clear_at.to_rfc3339()
                );
                slot.insert(PoolExhaustionHold {
                    dir: pool.dir.clone(),
                    total: pool.total,
                    since: now,
                    next_clear_at,
                    wrapper_observed: true,
                });
            }
        }
    }

    /// Every pool currently held, for the status/health surface. Sorted by
    /// pool directory so the rendering is stable across calls.
    #[must_use]
    pub fn active_holds(&self) -> Vec<PoolExhaustionHold> {
        let mut out: Vec<PoolExhaustionHold> = self.lock().values().cloned().collect();
        out.sort_by(|a, b| a.dir.cmp(&b.dir));
        out
    }

    /// How many distinct pools this host currently holds. `0` is the healthy
    /// steady state.
    #[must_use]
    pub fn held_pool_count(&self) -> usize {
        self.lock().len()
    }

    /// Drop every hold. Test-only reset for the process-global instance; never
    /// called in production, where a hold must only ever clear by recovering.
    #[cfg(test)]
    pub fn clear_all(&self) {
        self.lock().clear();
    }
}

/// Human-readable "held for" duration used in the recovery log line.
fn format_held_for(held: chrono::Duration) -> String {
    let secs = held.num_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m{}s", secs / 60, secs % 60);
    }
    format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
}

/// Compute this tick's per-root dispatch-hold slice, folding the #7708
/// pool-exhaustion hold together with the #5030 claude-wrapper pre-flight
/// advisory gate.
///
/// Returns `(held, probe_roots)` parallel to / drawn from `roots`:
/// `held[i] == true` means no new dispatch may go to `roots[i]` this tick;
/// `probe_roots` lists the roots whose #5030 half-open breaker granted a
/// single recovery probe dispatch.
///
/// **A pool hold outranks a #5030 recovery probe.** The probe exists to test
/// whether a broken workspace recovered, but a probe dispatched into a pool
/// with zero spawnable accounts tests nothing: it dies at token selection
/// every time, costing exactly the label flip and lease comment #7708 is
/// about. So a held root yields no probe at all.
///
/// Both holds are strictly per root (the #3930 isolation contract): a repo
/// whose pool is dead never holds a sibling repo whose pool is healthy — even
/// though, in the common single-pool deployment, every root resolves to the
/// same pool and so they hold together.
pub fn preflight_held_per_root(
    workspaces: &WorkspacePool,
    roots: &[PathBuf],
    now: DateTime<Utc>,
) -> (Vec<bool>, Vec<PathBuf>) {
    let state = PoolHoldState::global();
    let mut probe_roots: Vec<PathBuf> = Vec::new();
    let held = roots
        .iter()
        .map(|root| {
            let pool_held = state.observe_root(root, now);
            let registry = workspaces.get_or_provision(root);
            let mut registry = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match registry.preflight_dispatch_gate(now) {
                PreflightDispatchGate::Open => pool_held,
                PreflightDispatchGate::Held => true,
                PreflightDispatchGate::Probe => {
                    if pool_held {
                        true
                    } else {
                        probe_roots.push(root.clone());
                        false
                    }
                }
            }
        })
        .collect();
    (held, probe_roots)
}

/// Single-workspace convenience over [`PoolHoldState::observe_root`] against
/// this host's global hold set.
#[must_use]
pub fn observe_root(root: &Path, now: DateTime<Utc>) -> bool {
    PoolHoldState::global().observe_root(root, now)
}

/// Arm this host's post-mortem hold for `root`'s pool — see
/// [`PoolHoldState::note_pool_dead`]. Called by the sweep reaper when a death
/// classifies as `NO_USABLE_ACCOUNT_CLASS`.
pub fn note_pool_dead(root: &Path) {
    PoolHoldState::global().note_pool_dead(root, Utc::now());
}

/// Every pool this host currently holds — the read side for
/// `loom-daemon status` / `health`.
#[must_use]
pub fn active_holds() -> Vec<PoolExhaustionHold> {
    PoolHoldState::global().active_holds()
}

#[cfg(test)]
mod tests;
