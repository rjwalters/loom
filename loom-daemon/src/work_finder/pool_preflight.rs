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
//! # Broadcast to peers, keyed by ACCOUNT SET — not by directory (#8001)
//!
//! This hold is broadcast over the peer-claim room
//! ([`crate::peer_claims::ClaimKind::PoolHoldArmed`]/`PoolHoldCleared`), so a
//! peer host does not have to re-discover a dead pool the expensive way —
//! one doomed dispatch, one label flip and one permanent lease comment at a
//! time. That is the remaining half of #7708's "do not let four hosts pick up
//! the slack four times over".
//!
//! The reason it was **not** broadcast when this module first landed is real
//! and is preserved, not discarded: each host resolves its *own* pool
//! (repo-local shadow pool if it holds `.token` files, else shared —
//! #3938/#7527), so one host's exhaustion says nothing about a peer holding a
//! genuinely different pool, and suppressing that peer would be a silent
//! fleet-wide stall. What changed is the **key**, not the risk appetite:
//!
//! - The hold set here stays keyed by the resolved pool **directory**, which
//!   is correct for a within-host key (it is exactly what "same pool" means to
//!   one filesystem) and wrong for a cross-host one — the identical path can
//!   name different pools on two hosts, and the same pool has different paths
//!   on two hosts.
//! - The *broadcast* is keyed by
//!   [`crate::tokens_pool::select::pool_account_fingerprint`] — a hash of the
//!   pool's account names. Exhaustion is a property of the **accounts** (an
//!   upstream rate-limit state every host holding that credential shares), not
//!   of a directory, so two hosts with the same accounts match (suppression is
//!   correct) and a repo-local shadow pool holding different accounts does not
//!   (no suppression — the hazard above, structurally excluded rather than
//!   merely documented).
//!
//! Fail-open throughout: a dropped ad, a peer without safehouse, or a host
//! that simply has not received the ad yet degrades byte-for-byte to the
//! pre-#8001 local-only pre-flight, which still stops that host on its own
//! next tick.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::sweep_registry::PreflightDispatchGate;
use crate::tokens_pool::select::{
    pool_account_fingerprint, pool_clear_estimate, spawnable_pool_state,
};
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

/// A transition in this host's hold set worth telling peers about (Issue
/// #8001) — the *edge*, never the steady state. Returned by
/// [`PoolHoldState::observe_root_edge`]/[`PoolHoldState::note_pool_dead`] so
/// the caller (which owns a `SweepRegistry`, and therefore the outbound
/// peer-claim channel) can publish it; this module stays free of transport,
/// matching the `decide`/`plan` split the rest of the daemon uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolHoldEdge {
    /// This pool just became unspawnable on this host. `remaining` is the
    /// hold's own `pool_clear_estimate` horizon (already capped at 900 s),
    /// carried so a receiving peer can compute its local expiry.
    Armed { remaining: Duration },
    /// This pool just recovered on this host — release peers early rather
    /// than leaving them suppressed for the remainder of a TTL this host has
    /// already stopped honouring.
    Cleared,
}

/// One root's full pre-flight verdict (Issue #8001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolObservation {
    /// `true` when this host's own live read says dispatch to the root must
    /// be held this tick — the pre-#8001 [`PoolHoldState::observe_root`]
    /// return value, unchanged.
    pub held: bool,
    /// The root's pool identity for cross-host matching
    /// ([`pool_account_fingerprint`]), or `None` when the resolved pool holds
    /// no accounts at all (the ABSENT-pool condition, #4642 — nothing to
    /// broadcast and nothing a peer's hold could be about).
    pub pool_key: Option<String>,
    /// The arm/clear edge crossed by *this* call, if any. `None` on every
    /// steady-state tick, which is what keeps a multi-hour outage to two ads
    /// rather than one per tick.
    pub edge: Option<PoolHoldEdge>,
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
    /// Pool keys currently held **by a peer** and already edge-logged by this
    /// host (Issue #8001). Purely a log-deduplication set: the authoritative
    /// peer state lives in [`crate::peer_claims::PeerClaimView`], which has
    /// its own TTL. Without this, a peer-sourced hold would log once per tick
    /// per root for the whole outage — precisely the noise
    /// [`PoolHoldState::observe_root`]'s edge-logging discipline exists to
    /// avoid.
    peer_held_logged: Mutex<HashSet<String>>,
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
    ///
    /// **Preference-aware since #8554.** When `runtimes.preference` /
    /// `runtimes.rolePreference.sweep-lifecycle` is configured (and no
    /// operator pin disables it), the verdict is no longer "is the Claude
    /// pool dry" but "is the WHOLE ordered list unavailable" — a lower tap
    /// being spawnable must dispatch on that tap instead of holding, which
    /// `sweep_registry::dispatch` then resolves onto through
    /// [`crate::runtime_preference::resolve_for_dispatch`]. With no
    /// preference configured this is byte-identical to the pre-#8554
    /// Claude-only check below.
    ///
    /// Two deliberate conservatisms remain, both in the **over**-hold (safe)
    /// direction, because an under-hold costs the label flip + lease comment
    /// #7708 exists to prevent:
    ///
    /// - A **post-mortem** hold (`wrapper_observed`, armed by a real
    ///   token-selection death) still outranks a spawnable lower tap for its
    ///   bounded TTL. The wrapper proved *that pool* cannot select an
    ///   account; re-deciding that verdict per tap is not something this
    ///   pre-flight can do from a pool-keyed hold.
    /// - The hold set stays keyed by **pool directory**, not by (root, list).
    ///   Two roots resolving to one pool with *different* preference lists
    ///   therefore share one hold key, so one root's list recovering clears
    ///   (and the other's re-arms) the shared key. Only the per-root return
    ///   value gates dispatch, so this affects the edge-logging, never
    ///   whether a root with a spawnable tap is dispatched.
    pub fn observe_root(&self, root: &Path, now: DateTime<Utc>) -> bool {
        self.observe_root_edge(root, now).held
    }

    /// [`Self::observe_root`] plus the cross-host broadcast payload (Issue
    /// #8001): the root's [`pool_account_fingerprint`] and the arm/clear
    /// **edge** this call crossed, if any.
    ///
    /// Identical hold semantics — `observe_root` is a thin projection of this
    /// — so every pre-#8001 caller keeps its exact behaviour. The extra work
    /// is one directory listing for the fingerprint, on a path that already
    /// reads that directory twice (`total` and `usable`).
    pub fn observe_root_edge(&self, root: &Path, now: DateTime<Utc>) -> PoolObservation {
        let now_epoch = u64::try_from(now.timestamp()).unwrap_or(0);
        let preference =
            crate::runtime_preference::resolve_runtime(root, "sweep-lifecycle", None, now_epoch);
        let preference_resolution = match &preference {
            Ok(crate::runtime_preference::Decision::Preference { resolution, .. }) => {
                Some(resolution)
            }
            // No preference configured, an operator pin is in force, or the
            // preference config itself is malformed: byte-identical to
            // before #8554 — native runtimes never touch the Claude pool,
            // everything else is gated on it exactly as before.
            _ => None,
        };
        if preference_resolution.is_none() && crate::worker_spawn::uses_native_sweep(root) {
            // A native runtime never touches the Claude pool, so this root
            // has no pool identity to broadcast about and no peer's hold can
            // be about it.
            return PoolObservation {
                held: false,
                pool_key: None,
                edge: None,
            };
        }
        let pool = spawnable_pool_state(root);
        let pool_key = pool_account_fingerprint(&pool.dir);
        let (exhausted, preference_diagnostic) = match preference_resolution {
            Some(resolution) => match &resolution.chosen {
                Some(_) => (false, None),
                // Every tap in the list was skipped — the #7708 hold now
                // covers the whole preference list, not just Claude (#8554).
                None => (true, Some(resolution.exhausted_diagnostic("sweep-lifecycle"))),
            },
            // `total == 0` is the ABSENT-pool condition (#4642), not this
            // one — see the module doc. Falling through to the clear path
            // below is deliberate: a pool that was drained to empty should
            // not keep a pre-flight hold alive under a stale key.
            None => (pool.total > 0 && pool.usable == 0, None),
        };
        let mut holds = self.lock();

        if exhausted {
            let next_clear_at = pool_clear_estimate(&pool.dir);
            let mut edge = None;
            match holds.entry(pool.dir.clone()) {
                Entry::Occupied(mut existing) => {
                    let hold = existing.get_mut();
                    hold.total = pool.total;
                    hold.next_clear_at = next_clear_at;
                }
                Entry::Vacant(slot) => {
                    // #8001: the arming edge — the one tick per outage that
                    // both logs and broadcasts. Every later tick refreshes
                    // the Occupied arm above and stays silent on both.
                    edge = Some(PoolHoldEdge::Armed {
                        remaining: remaining_until(next_clear_at, now),
                    });
                    if let Some(diagnostic) = &preference_diagnostic {
                        log::warn!(
                            "work_finder: every runtime in the sweep-lifecycle preference list \
                             is UNAVAILABLE. Holding ALL sweep dispatch for every workspace \
                             resolving to this pool until at least one tap recovers (~{}); no \
                             claim label will be flipped and no lease comment posted while held \
                             (#7708/#8554).\n{diagnostic}",
                            next_clear_at.to_rfc3339()
                        );
                    } else {
                        log::warn!(
                            "work_finder: token pool {} is EXHAUSTED — 0/{} accounts spawnable \
                             (every account bad-marked in .bad_tokens or hard-excluded by \
                             .ranking). Holding ALL sweep dispatch for every workspace resolving \
                             to this pool until at least one account returns (~{}); no claim \
                             label will be flipped and no lease comment posted while held. Run \
                             `loom-daemon tokens check --ranking` or `loom-daemon tokens unblock \
                             <name>` (#7708)",
                            pool.dir.display(),
                            pool.total,
                            next_clear_at.to_rfc3339()
                        );
                    }
                    slot.insert(PoolExhaustionHold {
                        dir: pool.dir.clone(),
                        total: pool.total,
                        since: now,
                        next_clear_at,
                        wrapper_observed: false,
                    });
                }
            }
            return PoolObservation {
                held: true,
                pool_key,
                edge,
            };
        }

        // The live read says at least one account is spawnable. A hold armed
        // by a REAL token-selection death outranks that read until its TTL:
        // the wrapper proved a spawn cannot select an account here, and the
        // whole reason #7708 happened is that the daemon's read and the
        // wrapper's can disagree. Everything else clears immediately — that
        // is the one-tick self-heal.
        let (held, edge) = match holds.get(&pool.dir) {
            Some(hold) if hold.wrapper_observed && now < hold.next_clear_at => (true, None),
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
                // #8001: the clearing edge. Broadcast so peers release this
                // host's advertised hold NOW rather than sitting out the rest
                // of a TTL this host has already stopped honouring.
                (false, Some(PoolHoldEdge::Cleared))
            }
            None => (false, None),
        };
        PoolObservation {
            held,
            pool_key,
            edge,
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
    ///
    /// Returns the [`PoolObservation`] describing the pool it armed (Issue
    /// #8001) so the reaper can broadcast the arming edge. A post-mortem hold
    /// is the *strongest* evidence this daemon ever gets that a pool cannot
    /// spawn — a real wrapper proved it — which makes it the most valuable
    /// thing to tell peers: every peer that honours it skips a dispatch that
    /// was certain to die at token selection.
    pub fn note_pool_dead(&self, root: &Path, now: DateTime<Utc>) -> PoolObservation {
        let pool = spawnable_pool_state(root);
        let pool_key = pool_account_fingerprint(&pool.dir);
        let next_clear_at = pool_clear_estimate(&pool.dir);
        let mut edge = None;
        let mut holds = self.lock();
        match holds.entry(pool.dir.clone()) {
            Entry::Occupied(mut existing) => {
                let hold = existing.get_mut();
                hold.total = pool.total;
                hold.next_clear_at = next_clear_at;
                // #8001: a wrapper-confirmed death STRENGTHENS an existing
                // pre-flight hold (`wrapper_observed` flips false -> true),
                // so it is an edge worth broadcasting even though this host
                // was already holding — a peer that has not received (or has
                // since expired) the original arm ad gets a fresh, longer
                // window out of the strongest evidence available. Re-arming
                // an already-post-mortem hold is a no-op edge.
                if !hold.wrapper_observed {
                    edge = Some(PoolHoldEdge::Armed {
                        remaining: remaining_until(next_clear_at, now),
                    });
                }
                hold.wrapper_observed = true;
            }
            Entry::Vacant(slot) => {
                edge = Some(PoolHoldEdge::Armed {
                    remaining: remaining_until(next_clear_at, now),
                });
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
        PoolObservation {
            held: true,
            pool_key,
            edge,
        }
    }

    /// Note that a **peer** reports `pool_key` dead, returning `true` only on
    /// the arming edge — i.e. the first call since this host last saw the
    /// pool free (Issue #8001). The caller logs on `true` only, giving the
    /// peer-sourced hold the same once-per-outage log discipline
    /// [`Self::observe_root`] gives the local one.
    pub fn note_peer_hold(&self, pool_key: &str) -> bool {
        self.peer_held_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pool_key.to_owned())
    }

    /// [`Self::note_peer_hold`]'s clearing edge: `true` only on the first
    /// call after a peer-sourced hold on `pool_key` goes away.
    pub fn note_peer_clear(&self, pool_key: &str) -> bool {
        self.peer_held_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(pool_key)
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
        self.peer_held_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// Seconds from `now` until `next_clear_at`, as a [`Duration`] (Issue #8001)
/// — the `remaining` a [`PoolHoldEdge::Armed`] advertises.
///
/// Saturates at zero for an estimate already in the past: an ad advertising a
/// zero window is the same as no ad at all to a receiver, which is the safe
/// direction. The upper bound is `pool_clear_estimate`'s own 900 s cap, and
/// the receiver re-applies that cap itself
/// ([`crate::peer_claims::MAX_PEER_POOL_HOLD_TTL`]) rather than trusting it.
fn remaining_until(next_clear_at: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (next_clear_at - now)
        .to_std()
        .unwrap_or(Duration::from_secs(0))
}

/// Human-readable "held for" duration used in the recovery line.
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
///
/// # The fleet half (Issue #8001)
///
/// This is also where a pool hold becomes fleet-visible, in both directions:
///
/// - **Publish.** Each root's arm/clear *edge* is broadcast over the
///   peer-claim room, keyed by the pool's account fingerprint.
/// - **Consult.** A root whose own live read says "healthy" is still held
///   when a **peer** advertises a live hold on the very same pool. That is
///   the point: the peer already paid for the discovery (a doomed dispatch,
///   a label flip, a permanent lease comment), so this host should not pay
///   for it again. It is a pure *addition* to the local verdict — a peer can
///   only ever add a hold, never clear one this host's own read armed.
///
/// Fail-open: without safehouse coordination `peer_pool_hold` is always
/// `(false, [])` and every line below collapses to the pre-#8001 behaviour.
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
            let observation = state.observe_root_edge(root, now);
            let registry = workspaces.get_or_provision(root);
            let mut registry = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let pool_held = fold_peer_pool_hold(state, &registry, &observation);
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

/// Publish `observation`'s arm/clear edge to peers and fold any peer-reported
/// hold on the same pool into this root's verdict (Issue #8001). Returns the
/// final "is this root's pool held" answer.
///
/// Split out of [`preflight_held_per_root`] so the peer half is unit-testable
/// against a hand-built `SweepRegistry` + `PeerClaimView` pair, without
/// needing a whole `WorkspacePool`.
pub(crate) fn fold_peer_pool_hold(
    state: &PoolHoldState,
    registry: &crate::sweep_registry::SweepRegistry,
    observation: &PoolObservation,
) -> bool {
    let Some(pool_key) = observation.pool_key.as_deref() else {
        // No accounts in the resolved pool: nothing to advertise, and no
        // peer's hold can be about a pool with no identity.
        return observation.held;
    };
    match &observation.edge {
        Some(PoolHoldEdge::Armed { remaining }) => registry.publish_peer_pool_hold_claim(
            crate::peer_claims::ClaimKind::PoolHoldArmed,
            pool_key,
            *remaining,
        ),
        Some(PoolHoldEdge::Cleared) => registry.publish_peer_pool_hold_claim(
            crate::peer_claims::ClaimKind::PoolHoldCleared,
            pool_key,
            Duration::from_secs(0),
        ),
        None => {}
    }

    // This host's own read already says hold — a peer's opinion can only
    // agree, so skip the lookup entirely and leave the peer edge-log alone
    // (the local hold has its own log line; two lines for one outage would
    // defeat the point of edge-logging).
    if observation.held {
        return true;
    }

    let (peer_held, peers) = registry.peer_pool_hold(pool_key);
    if peer_held {
        if state.note_peer_hold(pool_key) {
            log::warn!(
                "work_finder: peer host(s) {} report the token pool this workspace resolves to \
                 (account-set {pool_key}) is UNSPAWNABLE, while this host's own pre-flight read \
                 still says it is fine. Trusting the peer and holding sweep dispatch — a peer's \
                 hold is evidence THIS host has not paid for yet (a doomed dispatch, a claim \
                 label flip and a permanent lease comment). Clears on the peer's recovery ad or \
                 its hold TTL, whichever is first (#8001/#7708)",
                peers.join(", ")
            );
        }
    } else if state.note_peer_clear(pool_key) {
        log::info!(
            "work_finder: no peer host holds the token pool this workspace resolves to \
             (account-set {pool_key}) any more; sweep dispatch resuming (#8001)"
        );
    }
    peer_held
}

/// Single-workspace convenience over [`PoolHoldState::observe_root`] against
/// this host's global hold set.
///
/// Deliberately **local-only**: this is the single-workspace probe
/// (`fleet_experiment`, the work finder's per-root re-derivation) and it has
/// no `SweepRegistry` to reach the peer-claim room through. The fleet half
/// lives in [`preflight_held_per_root`]/[`fold_peer_pool_hold`], which every
/// production dispatch path already goes through.
#[must_use]
pub fn observe_root(root: &Path, now: DateTime<Utc>) -> bool {
    PoolHoldState::global().observe_root(root, now)
}

/// Arm this host's post-mortem hold for `root`'s pool — see
/// [`PoolHoldState::note_pool_dead`]. Called by the sweep reaper when a death
/// classifies as `NO_USABLE_ACCOUNT_CLASS`.
///
/// Returns the [`PoolObservation`] so the reaper (which owns the outbound
/// peer-claim channel) can broadcast the arming edge — see #8001.
pub fn note_pool_dead(root: &Path) -> PoolObservation {
    PoolHoldState::global().note_pool_dead(root, Utc::now())
}

/// Every pool this host currently holds — the read side for
/// `loom-daemon status` / `health`.
#[must_use]
pub fn active_holds() -> Vec<PoolExhaustionHold> {
    PoolHoldState::global().active_holds()
}

/// [`active_holds`] projected onto the status wire (Issue #7990).
///
/// The daemon owns the hold set, but `loom-daemon status` / `health` run in a
/// *separate CLI process* that only ever sees a
/// [`crate::types::DaemonStatusReport`] — so the holds have to ride that
/// payload. This is the projection, kept here (beside the state it reads)
/// rather than in `ipc.rs`, matching
/// [`crate::worktree_reaper::stuck_worktree_removals`]'s own shape.
///
/// Order is [`PoolHoldState::active_holds`]'s: sorted by pool directory, so
/// rendering is stable across calls.
#[must_use]
pub fn active_hold_statuses() -> Vec<crate::types::PoolExhaustionHoldStatus> {
    active_holds()
        .into_iter()
        .map(|h| crate::types::PoolExhaustionHoldStatus {
            dir: h.dir,
            total: h.total,
            since: h.since,
            next_clear_at: h.next_clear_at,
            wrapper_observed: h.wrapper_observed,
        })
        .collect()
}

#[cfg(test)]
mod tests;
