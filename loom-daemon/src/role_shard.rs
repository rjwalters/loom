//! Role-runner **host sharding** (Issue #6374).
//!
//! ## The problem this solves
//!
//! The role runner ([`crate::role_runner`]) ticks every registered workspace's
//! role rotation (curator, champion, judge, …) on a fixed cadence. It is a
//! **per-host** loop with no cross-host coordination, so on a fleet of N
//! dispatchers every workspace's rotation runs N times per interval — N
//! `claude` sessions doing the same pass over the same forge queue.
//!
//! That is not merely wasteful, it is actively harmful:
//!
//! * **Token exhaustion.** On the 2AMLogic fleet (4 dispatchers × 27
//!   workspaces × 900s) the token pool hit 2/17 available and role ticks were
//!   failing ~20/hour on exhausted accounts. The token draw scaled with
//!   *workspaces × hosts*, not workspaces.
//! * **Cross-host duplication bugs.** The #6332 docs-PR race and the #6352
//!   narration duplication were both role ticks racing their own duplicates on
//!   a peer host.
//!
//! The operator mitigation was a blunt `LOOM_ROLE_RUNNER=0` on some hosts —
//! dispatch/build everywhere, role rotation only on a subset. That works, but
//! it is all-or-nothing per host: it cannot spread 27 workspaces over 4 hosts.
//!
//! This module is the first-class version of that knob. Today's
//! `LOOM_ROLE_RUNNER=0` is the degenerate case of it.
//!
//! ## The mechanism
//!
//! Each host is assigned a **shard index** in `0..count`. A workspace's role
//! rotation runs on the single host whose index equals
//! `fnv1a64(shard_key) % count`, where `shard_key` is a **cross-host-stable**
//! identity for the workspace (see [`resolve_shard_key`]).
//!
//! Because every host computes the same hash over the same key and the indices
//! partition `0..count`, exactly one host owns each workspace *by
//! construction* — there is no election, no lease, and therefore no window in
//! which a workspace has zero or two owners. That is the whole point of
//! choosing a deterministic assignment over a coordinated one: the invariant is
//! a property of arithmetic, not of a protocol that can race.
//!
//! ## Two knobs with deliberately opposite homes
//!
//! | Knob | Where it belongs | Why |
//! |------|------------------|-----|
//! | `shardIndex` | **host-local only** — [`SHARD_INDEX_ENV`], or an untracked config tier (`.loom-local/local.json`) | It must **differ** per host. Two hosts with the same index own the same slice and *nobody* owns the rest. |
//! | `shardCount` | either — [`SHARD_COUNT_ENV`] or the tracked `.loom/config.json` | It must be **identical** fleet-wide, so the committed config is a fine home for it. |
//! | `shardKey` | tracked `.loom/config.json` | Same reason: it must be identical fleet-wide, and a committed file is identical fleet-wide *by construction*. |
//!
//! Putting `shardIndex` in the **tracked** `.loom/config.json` is the one
//! misconfiguration that silently breaks the fleet in the worst direction:
//! every host reads the same file, resolves the same index, and every workspace
//! that does not hash to that index gets **zero** role ticks anywhere. So this
//! module detects that case specifically and **refuses to shard**, falling back
//! to the pre-#6374 unsharded behavior with an `error!` — see
//! [`UnshardedReason::IndexFromTrackedConfig`].
//!
//! ## Fail-safe direction
//!
//! Every malformed / incomplete / contradictory configuration resolves to
//! [`ShardPosture::Unsharded`], which owns **every** workspace. That reproduces
//! today's duplicate-per-host behavior — wasteful, but it is the status quo the
//! fleet already survives. The opposite failure (owning nothing, or leaving a
//! slice unowned) silently stops role rotation entirely, which is strictly
//! worse and much harder to notice. When in doubt, duplicate; never drop.
//!
//! ## The dynamic half: the roster (Issue #6704)
//!
//! By itself the assignment above is **static**: derived from `(shardIndex,
//! shardCount)`, not from a live roster, so killing a host does **not**
//! reassign its slice — its workspaces simply stop rotating until an operator
//! lowers `shardCount` (or points a survivor at the vacated index).
//!
//! The [`roster`] submodule closes that gap, behind
//! `autonomous.roleRunner.roster.enabled` (default **`false`**, so everything
//! above is unchanged unless a fleet opts in). It is a forge-backed host
//! roster — one marker comment per host on a designated issue, liveness from
//! the comment's forge-assigned `updated_at`, exactly as with lease records —
//! refreshed by a per-daemon heartbeat task ([`crate::role_runner`]'s roster
//! loop), plus a **generation-fenced ring**: a host acts only under the newest
//! membership generation it has observed, and only once that generation has
//! been settled for a full role-tick interval, so a membership disagreement
//! *yields* instead of duplicating. `defaults/docs/role-runner-roster.md` is
//! the design record; [`roster::admission`] is its fencing rule verbatim.
//!
//! Roster mode changes only the **source** of `(index, count)` in
//! [`ShardPosture::Sharded`] (a new [`ValueSource::Roster`]) and adds the
//! [`ShardDecision::admits_role_tick`] gate. [`hash_key`], [`owns`],
//! [`resolve_shard_key`], the status rendering, and the static env pair — which
//! stays the *higher-precedence* escape hatch — are untouched.
//!
//! [`owns`]: ShardPosture::owns
//!
//! ## The fail-safe direction inverts under roster mode — deliberately
//!
//! Above, every ambiguity resolves to "duplicate, never drop", because the
//! doubt is about *configuration* and the fallback is the pre-#6374 status quo
//! the fleet already survives. A roster ambiguity is about *liveness*, and the
//! two errors are not symmetric: a brief gap is one idempotent periodic pass
//! running an interval later, while a brief duplicate is two `claude` sessions
//! racing the same forge queue (#6332 / #6352) and is not self-healing. So
//! every roster-mode ambiguity resolves toward **yield**
//! ([`roster::RosterYield`]).
//!
//! The one case that keeps the #6374 direction is "this host never got a
//! roster at all" ([`RosterOff::NeverJoined`]): an unreachable or unconfigured
//! roster at startup falls back to the static posture, because yielding there
//! would let a typo in `roster.issue` silently stop role rotation fleet-wide.
//! Only a host that successfully **joined and then lost** the roster yields.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

pub mod roster;

// ============================================================================
// Constants
// ============================================================================

/// Env var carrying **this host's** shard index (0-based). This is the
/// host-local half of the knob and has no config fallback that is safe to
/// commit — see the module docs' table. Set it in the service unit
/// (systemd `Environment=` / launchd `EnvironmentVariables`) next to wherever
/// `LOOM_ROLE_RUNNER` is set today.
pub const SHARD_INDEX_ENV: &str = "LOOM_ROLE_RUNNER_SHARD_INDEX";

/// Env var carrying the **fleet-wide** number of role-runner shards. Must be
/// identical on every host. Falls through to
/// `autonomous.roleRunner.shardCount`, which is safe to commit precisely
/// because it is fleet-wide.
pub const SHARD_COUNT_ENV: &str = "LOOM_ROLE_RUNNER_SHARD_COUNT";

/// Config key (under `autonomous.roleRunner`) for the fleet-wide shard count.
pub const SHARD_COUNT_KEY: &str = "shardCount";

/// Config key (under `autonomous.roleRunner`) for this host's shard index.
/// Legal only in an **untracked** tier; see
/// [`UnshardedReason::IndexFromTrackedConfig`].
pub const SHARD_INDEX_KEY: &str = "shardIndex";

/// Config key (under `autonomous.roleRunner`) for an explicit, fleet-wide
/// shard key overriding the derived one. See [`KeySource::ConfigExplicit`].
pub const SHARD_KEY_KEY: &str = "shardKey";

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// ============================================================================
// Hashing
// ============================================================================

/// FNV-1a over `key`'s UTF-8 bytes.
///
/// **Deliberately hand-rolled, not [`std::collections::hash_map::DefaultHasher`].**
/// The whole correctness argument for this module is that two *different
/// processes on different machines, possibly on different architectures and
/// different Rust versions*, compute the identical value for the identical
/// key. `DefaultHasher` explicitly does not guarantee that (its algorithm and
/// its seeding are both unspecified and have changed across releases), so
/// using it would make the "exactly one owner" invariant depend on every host
/// in the fleet running a byte-identical binary. FNV-1a is fully specified,
/// endian-independent, and pinned by the known-vector test below.
#[must_use]
pub fn hash_key(key: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The shard index that owns `key` given `count` shards. Returns `None` for
/// `count == 0` (no shard can own anything).
#[must_use]
pub fn owning_shard(key: &str, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    // `count` is small (a host count); the cast is lossless in practice and
    // saturates rather than wrapping on a pathological value.
    let count_u64 = u64::try_from(count).unwrap_or(u64::MAX);
    usize::try_from(hash_key(key) % count_u64).ok()
}

// ============================================================================
// Shard key
// ============================================================================

/// Which tier produced a workspace's shard key. Surfaced in status/logs
/// because a **mismatch between hosts here is the one way the "exactly one
/// owner" invariant can still break**: if host A resolves `rjwalters/loom`
/// from its git remote and host B falls back to the basename `loom`, the two
/// hash differently and can both (or neither) own it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `autonomous.roleRunner.shardKey` from the workspace's own config. The
    /// most robust source: a tracked config file is identical on every host by
    /// construction, so this cannot diverge.
    ConfigExplicit,
    /// `owner/repo` parsed from the workspace's `origin` remote. Identical on
    /// every host that cloned the same repo, regardless of local path.
    GitRemote,
    /// The workspace root's final path component. The last-resort fallback,
    /// used when the workspace has no resolvable `origin` remote. Diverges if
    /// two hosts cloned the same repo into differently-named directories, so
    /// it is logged and surfaced rather than used silently.
    Basename,
}

impl KeySource {
    /// A short, stable label for logs and status output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ConfigExplicit => "config",
            Self::GitRemote => "git-remote",
            Self::Basename => "basename",
        }
    }

    /// Whether this source is guaranteed identical across hosts. `false` for
    /// [`Self::Basename`], which depends on the local clone's directory name.
    #[must_use]
    pub const fn is_cross_host_stable(self) -> bool {
        matches!(self, Self::ConfigExplicit | Self::GitRemote)
    }
}

/// A workspace's resolved shard key plus the tier it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    /// The string actually hashed.
    pub key: String,
    /// Which tier produced it.
    pub source: KeySource,
}

/// Process-lifetime cache for [`nwo_for_root`]. A workspace's `origin` remote
/// is effectively immutable for the daemon's lifetime, and this is on the
/// per-(root, role) tick path, so re-forking `git remote get-url` every tick
/// would be pure overhead.
fn nwo_cache() -> &'static Mutex<HashMap<PathBuf, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cached [`crate::credential_preflight::nwo_from_git_remote`] for `root`.
fn nwo_for_root(root: &Path) -> Option<String> {
    let mut cache = nwo_cache().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(hit) = cache.get(root) {
        return hit.clone();
    }
    let resolved = crate::credential_preflight::nwo_from_git_remote(root);
    cache.insert(root.to_path_buf(), resolved.clone());
    resolved
}

/// Clear the [`nwo_for_root`] cache. Test seam only.
#[cfg(test)]
fn clear_nwo_cache() {
    nwo_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// Resolve the cross-host-stable shard key for `root`.
///
/// Precedence, highest first:
/// 1. `explicit` — `autonomous.roleRunner.shardKey` from the workspace's own
///    config (blank/whitespace-only is ignored).
/// 2. `owner/repo` from the workspace's `origin` remote.
/// 3. The root's final path component.
///
/// Never fails: an empty/rootless path degrades to the full lossy path string
/// so the caller always has *something* to hash.
#[must_use]
pub fn resolve_shard_key(root: &Path, explicit: Option<&str>) -> ResolvedKey {
    if let Some(raw) = explicit {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return ResolvedKey {
                key: trimmed.to_string(),
                source: KeySource::ConfigExplicit,
            };
        }
    }
    if let Some(nwo) = nwo_for_root(root) {
        return ResolvedKey {
            key: nwo,
            source: KeySource::GitRemote,
        };
    }
    let basename = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    ResolvedKey {
        key: basename,
        source: KeySource::Basename,
    }
}

// ============================================================================
// Posture
// ============================================================================

/// Which tier supplied a resolved shard number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    /// From [`SHARD_INDEX_ENV`] / [`SHARD_COUNT_ENV`].
    Env,
    /// From `autonomous.roleRunner.{shardIndex,shardCount}`.
    Config,
    /// From the **live roster** (Issue #7691, Phase B of #6704): this host's
    /// ordinal in, and the size of, [`roster::ring`] for this workspace's
    /// key. Unlike the other two tiers this one is *dynamic* — it changes as
    /// hosts join and expire — which is why it is only ever reached through
    /// the generation fence ([`roster::admission`]).
    Roster,
}

impl ValueSource {
    /// A short, stable label for logs and status output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Config => "config",
            Self::Roster => "roster",
        }
    }
}

/// Why sharding is not in effect. Every variant means "this host owns every
/// workspace", i.e. the pre-#6374 behavior — see the module docs' fail-safe
/// note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnshardedReason {
    /// Neither the index nor the count is set anywhere. The default, and the
    /// only variant that is not worth logging: an unsharded single-host
    /// install is the normal case.
    NotConfigured,
    /// `shardCount == 1`. A one-shard fleet is unsharded by definition; this
    /// is distinct from [`Self::NotConfigured`] because the operator *did*
    /// configure it, so status should say so.
    SingleShard,
    /// Exactly one of the two knobs is set. Sharding needs both, and guessing
    /// the missing one either duplicates work or drops it.
    Incomplete {
        /// Whether an index was resolved.
        have_index: bool,
        /// Whether a count was resolved.
        have_count: bool,
    },
    /// `shardCount == 0`: no shard could own anything.
    ZeroCount,
    /// `shardIndex >= shardCount`: this host's index is outside the ring, so
    /// no key could ever hash to it and this host would rotate nothing.
    IndexOutOfRange {
        /// The out-of-range index.
        index: usize,
        /// The count it was compared against.
        count: usize,
    },
    /// A knob was set to something unparseable.
    Malformed {
        /// Which knob (`shardIndex` / `shardCount`), by env-var or config-key
        /// name.
        field: String,
        /// The raw value, for the operator to recognize.
        raw: String,
    },
    /// **The dangerous misconfiguration** (see the module docs): `shardIndex`
    /// was declared in the workspace's **tracked** `.loom/config.json`, which
    /// every host in the fleet reads identically. Honoring it would give every
    /// host the same index, so every workspace not hashing to that index would
    /// get zero role ticks anywhere in the fleet. Refused rather than honored.
    IndexFromTrackedConfig {
        /// The index the tracked config declared, for the error message.
        index: usize,
    },
}

/// This host's resolved role-runner sharding posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardPosture {
    /// Sharding is off; this host owns every workspace's role rotation.
    Unsharded(UnshardedReason),
    /// This host owns the workspaces whose key hashes to `index` mod `count`.
    Sharded {
        /// This host's index within `0..count`.
        index: usize,
        /// The fleet-wide shard count.
        count: usize,
        /// Where `index` came from.
        index_source: ValueSource,
        /// Where `count` came from.
        count_source: ValueSource,
    },
}

impl ShardPosture {
    /// Whether this host runs role ticks for the workspace identified by
    /// `key`.
    ///
    /// An [`Self::Unsharded`] posture owns everything (the pre-#6374
    /// behavior); a [`Self::Sharded`] posture owns exactly the keys that hash
    /// into its own index.
    #[must_use]
    pub fn owns(&self, key: &str) -> bool {
        match self {
            Self::Unsharded(_) => true,
            Self::Sharded { index, count, .. } => owning_shard(key, *count) == Some(*index),
        }
    }

    /// Whether sharding is actually in effect.
    #[must_use]
    pub const fn is_sharded(&self) -> bool {
        matches!(self, Self::Sharded { .. })
    }

    /// This host's index, or `None` when unsharded.
    #[must_use]
    pub const fn index(&self) -> Option<usize> {
        match self {
            Self::Unsharded(_) => None,
            Self::Sharded { index, .. } => Some(*index),
        }
    }

    /// The fleet-wide shard count, or `None` when unsharded.
    #[must_use]
    pub const fn count(&self) -> Option<usize> {
        match self {
            Self::Unsharded(_) => None,
            Self::Sharded { count, .. } => Some(*count),
        }
    }

    /// Whether the operator configured sharding at all — `true` for a working
    /// shard AND for every *misconfigured* one, `false` only for
    /// [`UnshardedReason::NotConfigured`].
    ///
    /// The distinction status rendering needs: an unconfigured single-host
    /// install should not grow a line about a feature it does not use, but a
    /// configuration the operator asked for and did not get must never be
    /// silent — invisibility is exactly what made the pre-#6374
    /// `LOOM_ROLE_RUNNER=0` mitigation hard to reason about.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        !matches!(self, Self::Unsharded(UnshardedReason::NotConfigured))
    }

    /// A one-line human description for status output and boot logs.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Unsharded(reason) => describe_unsharded(reason),
            Self::Sharded {
                index,
                count,
                index_source,
                count_source,
            } => format!(
                "shard {index} of {count} (index from {}, count from {}) — this host runs role \
                 ticks only for workspaces whose shard key hashes to {index}",
                index_source.label(),
                count_source.label(),
            ),
        }
    }
}

/// The human description for each [`UnshardedReason`]. Every one of these ends
/// in the same operational fact — this host rotates **every** workspace — so
/// the reader never has to infer the direction of the fallback.
fn describe_unsharded(reason: &UnshardedReason) -> String {
    let tail = "this host runs role ticks for EVERY registered workspace";
    match reason {
        UnshardedReason::NotConfigured => {
            format!("off (no {SHARD_INDEX_ENV}/{SHARD_COUNT_ENV} configured) — {tail}")
        }
        UnshardedReason::SingleShard => {
            format!("off ({SHARD_COUNT_KEY}=1, a single-shard fleet) — {tail}")
        }
        UnshardedReason::Incomplete {
            have_index,
            have_count,
        } => {
            let missing = if *have_index {
                SHARD_COUNT_ENV
            } else {
                SHARD_INDEX_ENV
            };
            let _ = have_count;
            format!(
                "off (incomplete config: {missing} is not set, and sharding needs BOTH an index \
                 and a count) — {tail}"
            )
        }
        UnshardedReason::ZeroCount => {
            format!("off (invalid {SHARD_COUNT_KEY}=0) — {tail}")
        }
        UnshardedReason::IndexOutOfRange { index, count } => format!(
            "off (invalid {SHARD_INDEX_KEY}={index} is outside 0..{count}; this host would \
             otherwise rotate nothing at all) — {tail}"
        ),
        UnshardedReason::Malformed { field, raw } => {
            format!("off (malformed {field}={raw:?}) — {tail}")
        }
        UnshardedReason::IndexFromTrackedConfig { index } => format!(
            "off (REFUSED: {SHARD_INDEX_KEY}={index} is declared in the workspace's TRACKED \
             .loom/config.json, which every host in the fleet reads identically — honoring it \
             would give every host the same index and leave every other slice with zero owners \
             fleet-wide; set {SHARD_INDEX_ENV} per host instead) — {tail}"
        ),
    }
}

// ============================================================================
// Resolution
// ============================================================================

/// Parse a non-negative integer knob, distinguishing "absent" from
/// "malformed".
enum ParsedKnob {
    Absent,
    Value(usize, ValueSource),
    Malformed { field: String, raw: String },
}

fn parse_env_knob(env_name: &str) -> ParsedKnob {
    let Ok(raw) = std::env::var(env_name) else {
        return ParsedKnob::Absent;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ParsedKnob::Absent;
    }
    match trimmed.parse::<usize>() {
        Ok(v) => ParsedKnob::Value(v, ValueSource::Env),
        Err(_) => ParsedKnob::Malformed {
            field: env_name.to_string(),
            raw: raw.clone(),
        },
    }
}

fn parse_config_knob(block: Option<&serde_json::Value>, key: &str) -> ParsedKnob {
    let Some(value) = block.and_then(|b| b.get(key)) else {
        return ParsedKnob::Absent;
    };
    match value.as_u64().and_then(|v| usize::try_from(v).ok()) {
        Some(v) => ParsedKnob::Value(v, ValueSource::Config),
        None => ParsedKnob::Malformed {
            field: format!("autonomous.roleRunner.{key}"),
            raw: value.to_string(),
        },
    }
}

/// Resolve a knob with **env > config** precedence, matching every other
/// `autonomous.*` surface.
fn resolve_knob(env_name: &str, block: Option<&serde_json::Value>, key: &str) -> ParsedKnob {
    match parse_env_knob(env_name) {
        ParsedKnob::Absent => parse_config_knob(block, key),
        other => other,
    }
}

/// Whether the workspace's **tracked** `.loom/config.json` declares
/// `autonomous.roleRunner.shardIndex`.
///
/// Read directly rather than through [`crate::config_resolver`] on purpose:
/// the resolver merges the tracked tier with the untracked
/// `.loom-local/local.json` and the machine-level defaults file, and the whole
/// question here is *which tier* the value came from. A merged view cannot
/// answer it.
fn tracked_config_declares_shard_index(root: &Path) -> Option<usize> {
    let path = root.join(crate::config_resolver::LEGACY_CONFIG_REL);
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("autonomous")?
        .get("roleRunner")?
        .get(SHARD_INDEX_KEY)?
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
}

/// Resolve this host's sharding posture for `root`.
///
/// **Read per-root, but describing the host.** The index/count knobs are
/// host- and fleet-level facts, yet they are resolved from whichever root's
/// tick is asking — exactly as `autonomous.roleRunner.maxConcurrent` already
/// is (see [`crate::role_runner::RoleRunnerConfig::max_concurrent`]). On a
/// fleet host whose roots disagree, each root is gated by its own resolution;
/// in the intended deployment `shardCount` is identical everywhere and
/// `shardIndex` comes from the host's env, so they agree trivially.
#[must_use]
pub fn resolve_posture(root: &Path) -> ShardPosture {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let block = crate::config_resolver::get_path(&effective, "autonomous.roleRunner");
    resolve_posture_from(block, root)
}

/// The pure core of [`resolve_posture`], split out so tests can drive it with
/// a synthetic config block instead of a temp-dir repo.
fn resolve_posture_from(block: Option<&serde_json::Value>, root: &Path) -> ShardPosture {
    let index_knob = resolve_knob(SHARD_INDEX_ENV, block, SHARD_INDEX_KEY);
    let count_knob = resolve_knob(SHARD_COUNT_ENV, block, SHARD_COUNT_KEY);

    for knob in [&index_knob, &count_knob] {
        if let ParsedKnob::Malformed { field, raw } = knob {
            return ShardPosture::Unsharded(UnshardedReason::Malformed {
                field: field.clone(),
                raw: raw.clone(),
            });
        }
    }

    let (index, index_source) = match index_knob {
        ParsedKnob::Value(v, s) => (Some(v), Some(s)),
        _ => (None, None),
    };
    let (count, count_source) = match count_knob {
        ParsedKnob::Value(v, s) => (Some(v), Some(s)),
        _ => (None, None),
    };

    let (Some(index), Some(count)) = (index, count) else {
        if index.is_none() && count.is_none() {
            return ShardPosture::Unsharded(UnshardedReason::NotConfigured);
        }
        return ShardPosture::Unsharded(UnshardedReason::Incomplete {
            have_index: index.is_some(),
            have_count: count.is_some(),
        });
    };
    // Unwrapping is sound: `*_source` is `Some` exactly when the matching
    // value is `Some` (both come from the same `ParsedKnob::Value` arm).
    let index_source = index_source.unwrap_or(ValueSource::Config);
    let count_source = count_source.unwrap_or(ValueSource::Config);

    if count == 0 {
        return ShardPosture::Unsharded(UnshardedReason::ZeroCount);
    }
    if count == 1 {
        return ShardPosture::Unsharded(UnshardedReason::SingleShard);
    }
    if index >= count {
        return ShardPosture::Unsharded(UnshardedReason::IndexOutOfRange { index, count });
    }
    // The fleet-breaking misconfiguration (module docs): a tracked, committed
    // `shardIndex` is identical on every host. Only checked when the resolved
    // index did NOT come from the env — an env value legitimately overrides
    // whatever the tracked file says, and is per-host by construction.
    if index_source == ValueSource::Config {
        if let Some(tracked) = tracked_config_declares_shard_index(root) {
            return ShardPosture::Unsharded(UnshardedReason::IndexFromTrackedConfig {
                index: tracked,
            });
        }
    }

    ShardPosture::Sharded {
        index,
        count,
        index_source,
        count_source,
    }
}

// ============================================================================
// Decision
// ============================================================================

/// Why roster mode is **not** supplying this decision's `(index, count)` —
/// every variant means "the #6374 static/unsharded posture decided this
/// verdict", i.e. the pre-roster behavior, verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterOff {
    /// `autonomous.roleRunner.roster.enabled` is `false` — the default.
    Disabled,
    /// `roster.enabled` is true but no valid `roster.issue` is configured
    /// ([`roster::RosterState::MisconfiguredNoIssue`], which the heartbeat
    /// task already `error!`s about).
    Misconfigured,
    /// The static [`SHARD_INDEX_ENV`] + `shardCount` pair resolved to a valid
    /// [`ShardPosture::Sharded`], which **outranks** the roster (the design
    /// record's escape-hatch precedence, rung 2): an operator who needs a
    /// deterministic ring keeps #6374's behavior verbatim by setting them.
    StaticShardWins,
    /// Roster mode is enabled and configured, but this host has never
    /// completed a roster read — the "never got a roster at all" case, which
    /// per the design record's rung 4 falls back to the #6374 posture rather
    /// than yielding. Only a host that successfully **joined and then lost**
    /// the roster yields.
    NeverJoined,
    /// A snapshot exists, but it was read from a *different* roster issue
    /// than the one now configured. Same reasoning as
    /// [`Self::NeverJoined`]: this host has no observation of the roster it
    /// is now supposed to be a member of.
    SnapshotForAnotherIssue,
}

impl RosterOff {
    /// A short, stable label for logs and status output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Misconfigured => "misconfigured",
            Self::StaticShardWins => "static-shard-wins",
            Self::NeverJoined => "never-joined",
            Self::SnapshotForAnotherIssue => "snapshot-for-another-issue",
        }
    }
}

/// How the live roster (Issue #7691, Phase B of #6704) participated in one
/// [`ShardDecision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterMode {
    /// It did not — see [`RosterOff`]. `posture`/`owned` are #6374's.
    Off(RosterOff),
    /// The fence admitted and the ring supplied `(index, count)`:
    /// `posture` is [`ShardPosture::Sharded`] with both sources
    /// [`ValueSource::Roster`], and `owned` is its ordinary `owns(key)`.
    Ring {
        /// The settled membership generation the ring was computed under.
        generation: chrono::DateTime<chrono::Utc>,
    },
    /// The fence **denied** the role tick (see [`roster::RosterYield`]).
    ///
    /// `posture`/`owned` still carry the pre-roster (#6374) verdict: a yield
    /// must not reach [`crate::work_finder`]'s preferred-slice consumer as
    /// "owns nothing", because there the verdict is only a *preference* with
    /// a work-conserving fallback and turning it off would starve dispatch.
    /// The role runner reads [`ShardDecision::admits_role_tick`] instead,
    /// which is the surface this yield actually gates.
    Yield(roster::RosterYield),
}

impl RosterMode {
    /// Whether the roster fence denied a role tick here.
    #[must_use]
    pub const fn is_yield(&self) -> bool {
        matches!(self, Self::Yield(_))
    }

    /// A short, stable label for logs and status output.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off(off) => off.label(),
            Self::Ring { .. } => "ring",
            Self::Yield(y) => y.label(),
        }
    }
}

/// One workspace's resolved sharding decision: the host posture, the
/// workspace's key, and whether this host owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardDecision {
    /// This host's posture.
    pub posture: ShardPosture,
    /// The workspace's resolved key.
    pub key: ResolvedKey,
    /// Whether this host owns this workspace's slice.
    ///
    /// **Two consumers read this with deliberately different strength.** For
    /// [`crate::work_finder`]'s dispatcher slice (#6243) it is a *preference*
    /// with a work-conserving fallback, so it keeps the pre-roster semantics
    /// even when the roster fence yields. The role runner must additionally
    /// pass the fence — it calls [`Self::admits_role_tick`], never this field
    /// alone.
    pub owned: bool,
    /// Which shard owns it, or `None` when unsharded.
    pub owning_shard: Option<usize>,
    /// How the live roster participated (Issue #7691) —
    /// [`RosterMode::Off`]`(`[`RosterOff::Disabled`]`)` whenever roster mode
    /// is off, which is the default and is byte-identical to #6374.
    pub roster: RosterMode,
}

impl ShardDecision {
    /// Whether this host may run a **role tick** for this workspace: it owns
    /// the slice AND the roster fence did not yield.
    ///
    /// This is the role runner's gate on both dispatch surfaces
    /// ([`crate::role_runner::decide_root_tick`] and `plan_idle_runs`). With
    /// the roster off it is exactly `owned`, i.e. #6374 unchanged.
    #[must_use]
    pub const fn admits_role_tick(&self) -> bool {
        self.owned && !self.roster.is_yield()
    }

    /// A one-line log/status description naming `root`, the key (and its
    /// tier), the owning shard, and this host's verdict.
    ///
    /// **This string is also [`log_decision_once`]'s dedup key**, so the
    /// roster clause carries only values that are *stable between membership
    /// boundaries* — the fence's label and its generation, never a
    /// tick-by-tick countdown like "settled 61s of 900s". A changing number
    /// here would turn the edge-triggered `info!` line into a per-tick,
    /// per-root, per-role log flood for the whole settle window. The
    /// countdown lives on the `debug!` line the role runner emits instead
    /// ([`roster::RosterYield::describe`]).
    #[must_use]
    pub fn describe(&self, root: &Path) -> String {
        let verdict = if self.admits_role_tick() {
            "OWNED here"
        } else {
            "not owned here"
        };
        let roster = match &self.roster {
            RosterMode::Off(RosterOff::Disabled) => String::new(),
            RosterMode::Off(off) => format!(" [roster: {} — static ring in effect]", off.label()),
            RosterMode::Ring { generation } => {
                format!(" [roster: ring settled at generation {generation}, #6704]")
            }
            RosterMode::Yield(y) => format!(
                " [roster: YIELDING role ticks — {} (#6704); dispatcher preference unchanged]",
                y.label()
            ),
        };
        match (&self.posture, self.owning_shard) {
            (ShardPosture::Sharded { index, count, .. }, Some(owner)) => format!(
                "role_runner: shard decision for {} — key={:?} ({}), owner=shard {owner} of \
                 {count}, this host=shard {index} => {verdict} (#6374){roster}",
                root.display(),
                self.key.key,
                self.key.source.label(),
            ),
            _ => format!(
                "role_runner: shard decision for {} — sharding {} => {verdict} (#6374){roster}",
                root.display(),
                self.posture.describe(),
            ),
        }
    }
}

/// Decide whether this host runs `root`'s role rotation.
///
/// This is the entry point [`crate::role_runner`] calls once per (root, role)
/// tick, after the [`crate::role_runner::ROLE_RUNNER_ENABLE_ENV`] /
/// per-root-`enabled` gate — so `LOOM_ROLE_RUNNER=0` still short-circuits
/// everything before sharding is even consulted (Issue #6374 AC3).
///
/// ## Escape-hatch precedence (Issue #7691 / #6704 AC3), highest first
///
/// 1. **`LOOM_ROLE_RUNNER=0`** — checked by the caller, *before* this
///    function; the blunt kill switch is never weakened or second-guessed.
/// 2. **`LOOM_ROLE_RUNNER_SHARD_INDEX` + `shardCount`** — when the static
///    pair resolves to a valid [`ShardPosture::Sharded`] it **wins over the
///    roster** ([`RosterOff::StaticShardWins`]).
/// 3. **Roster** — only when `roster.enabled` is true, a valid `roster.issue`
///    is configured, this host has actually joined (a snapshot exists), and
///    no static index is set.
/// 4. **Unsharded** — everything else, including an enabled-but-never-read
///    roster ([`RosterOff::NeverJoined`]), which keeps #6374's
///    duplicate-biased fallback rather than yielding.
#[must_use]
pub fn decide(root: &Path) -> ShardDecision {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let block = crate::config_resolver::get_path(&effective, "autonomous.roleRunner");
    let posture = resolve_posture_from(block, root);
    let explicit = block
        .and_then(|b| b.get(SHARD_KEY_KEY))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let roster_config =
        roster::resolve_roster_config_from(block.and_then(|b| b.get(roster::ROSTER_BLOCK_KEY)));
    decide_with_roster(
        posture,
        root,
        explicit.as_deref(),
        &roster_config,
        roster::roster_snapshot().as_ref(),
        chrono::Utc::now(),
    )
}

/// The [`decide`] core with the posture and explicit key already resolved and
/// **the roster deliberately out of the picture** — the pre-#7691 seam, kept
/// verbatim so every #6374 test still drives exactly the behavior it pinned.
#[must_use]
pub fn decide_with(
    posture: ShardPosture,
    root: &Path,
    explicit_key: Option<&str>,
) -> ShardDecision {
    static_decision(posture, root, explicit_key, RosterMode::Off(RosterOff::Disabled))
}

/// Build a decision from the #6374 static posture, tagged with why the roster
/// did not supply it.
fn static_decision(
    posture: ShardPosture,
    root: &Path,
    explicit_key: Option<&str>,
    roster_mode: RosterMode,
) -> ShardDecision {
    let key = resolve_shard_key(root, explicit_key);
    let owning_shard = posture
        .count()
        .and_then(|count| owning_shard(&key.key, count));
    let owned = posture.owns(&key.key);
    ShardDecision {
        posture,
        key,
        owned,
        owning_shard,
        roster: roster_mode,
    }
}

/// The full [`decide`] core: the static posture, the roster config, the last
/// roster snapshot (if any), and `now`, with no I/O of its own — the seam the
/// split-view / kill-host / self-fence / join-fence tests drive.
///
/// Implements the precedence documented on [`decide`].
#[must_use]
pub fn decide_with_roster(
    static_posture: ShardPosture,
    root: &Path,
    explicit_key: Option<&str>,
    roster_config: &roster::RosterConfig,
    snapshot: Option<&roster::RosterSnapshot>,
    now: chrono::DateTime<chrono::Utc>,
) -> ShardDecision {
    // Rung 2: a valid static pair outranks the roster, verbatim #6374.
    if static_posture.is_sharded() {
        return static_decision(
            static_posture,
            root,
            explicit_key,
            RosterMode::Off(RosterOff::StaticShardWins),
        );
    }
    let off = |reason: RosterOff| RosterMode::Off(reason);
    let Some(issue) = roster_config.issue() else {
        let reason = match roster_config.state {
            roster::RosterState::Disabled => RosterOff::Disabled,
            _ => RosterOff::Misconfigured,
        };
        return static_decision(static_posture, root, explicit_key, off(reason));
    };
    // Rung 4's "never got a roster at all" case: fall back to the #6374
    // posture (duplicate-biased), NOT to a yield. Only a host that joined and
    // then lost the roster yields — and such a host has a snapshot.
    let Some(snapshot) = snapshot else {
        return static_decision(static_posture, root, explicit_key, off(RosterOff::NeverJoined));
    };
    if snapshot.issue != *issue {
        return static_decision(
            static_posture,
            root,
            explicit_key,
            off(RosterOff::SnapshotForAnotherIssue),
        );
    }

    // Rung 3: the fence decides.
    let key = resolve_shard_key(root, explicit_key);
    match roster::admit(snapshot, hash_key(&key.key), now) {
        roster::RosterAdmission::Yield(reason) => {
            // The dispatcher keeps the pre-roster verdict (see
            // `ShardDecision::owned`); only `admits_role_tick` flips.
            static_decision(static_posture, root, explicit_key, RosterMode::Yield(reason))
        }
        roster::RosterAdmission::Ring {
            index,
            count,
            generation,
        } => {
            let posture = ShardPosture::Sharded {
                index,
                count,
                index_source: ValueSource::Roster,
                count_source: ValueSource::Roster,
            };
            let owning_shard = owning_shard(&key.key, count);
            let owned = posture.owns(&key.key);
            ShardDecision {
                posture,
                key,
                owned,
                owning_shard,
                roster: RosterMode::Ring { generation },
            }
        }
    }
}

// ============================================================================
// Logging
// ============================================================================

/// Per-root last-logged shard decision, so the line below is emitted on the
/// **edge** (first sighting, or a changed decision) rather than every tick.
///
/// Process-global, deliberately — unlike [`crate::role_runner`]'s other
/// dedup maps, which live on each per-role loop's own stack. The shard
/// decision is a property of the *(host, workspace)* pair, identical for
/// every role, so per-loop dedup would emit the same line once per role in
/// `DEFAULT_ROLES` (currently ~10 identical lines per root at boot). One
/// shared map collapses them to one.
fn decision_logged() -> &'static Mutex<HashMap<PathBuf, String>> {
    static LOGGED: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    LOGGED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Log `decision` for `root` at `info!` on the edge and `debug!` thereafter.
///
/// A **refused** posture ([`UnshardedReason::IndexFromTrackedConfig`]) is
/// escalated to `error!` on the edge instead: it is an active
/// misconfiguration that would have broken the fleet, and it must not be
/// discoverable only by reading `info` logs.
pub fn log_decision_once(root: &Path, decision: &ShardDecision) {
    let line = decision.describe(root);
    let mut logged = decision_logged()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if logged.get(root).is_some_and(|prev| *prev == line) {
        drop(logged);
        log::debug!("{line}");
        return;
    }
    logged.insert(root.to_path_buf(), line.clone());
    drop(logged);
    if matches!(
        decision.posture,
        ShardPosture::Unsharded(UnshardedReason::IndexFromTrackedConfig { .. })
    ) {
        log::error!("{line}");
    } else {
        log::info!("{line}");
    }
}

/// Clear the [`log_decision_once`] dedup state. Test seam only.
#[cfg(test)]
fn clear_decision_log() {
    decision_logged()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Restore both shard env vars to whatever they were, so a `#[serial]`
    /// test that sets them cannot leak into the next one.
    struct EnvGuard {
        index: Option<String>,
        count: Option<String>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            let g = Self {
                index: std::env::var(SHARD_INDEX_ENV).ok(),
                count: std::env::var(SHARD_COUNT_ENV).ok(),
            };
            std::env::remove_var(SHARD_INDEX_ENV);
            std::env::remove_var(SHARD_COUNT_ENV);
            g
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.index {
                Some(v) => std::env::set_var(SHARD_INDEX_ENV, v),
                None => std::env::remove_var(SHARD_INDEX_ENV),
            }
            match &self.count {
                Some(v) => std::env::set_var(SHARD_COUNT_ENV, v),
                None => std::env::remove_var(SHARD_COUNT_ENV),
            }
        }
    }

    fn sharded(index: usize, count: usize) -> ShardPosture {
        ShardPosture::Sharded {
            index,
            count,
            index_source: ValueSource::Env,
            count_source: ValueSource::Config,
        }
    }

    /// The 27-workspace fleet from the issue's own incident report.
    fn fleet_keys() -> Vec<String> {
        (0..27).map(|i| format!("2amlogic/repo-{i}")).collect()
    }

    // ---- Hashing ----

    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        // The canonical FNV-1a 64 test vectors. These pin the algorithm so a
        // future "optimization" cannot silently change every host's shard
        // assignment (which would be invisible on any single host and would
        // break the fleet only once the hosts disagreed).
        assert_eq!(hash_key(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash_key("a"), 0xaf63_dc4c_8601_ec8c);
        // NB: 0x8506_7b17_8119_5929 is FNV-**1** (multiply-then-xor) for the
        // same input — a near-miss that is easy to copy by mistake. This is
        // the FNV-**1a** (xor-then-multiply) vector, which is what
        // [`hash_key`] computes.
        assert_eq!(hash_key("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn owning_shard_is_none_for_a_zero_count() {
        assert_eq!(owning_shard("rjwalters/loom", 0), None);
    }

    #[test]
    fn owning_shard_is_always_within_range() {
        for key in fleet_keys() {
            for count in 1..=8 {
                let owner = owning_shard(&key, count).expect("count > 0");
                assert!(owner < count, "{key} -> {owner} out of 0..{count}");
            }
        }
    }

    // ---- AC1: exactly one owner per workspace per interval, fleet-wide ----

    #[test]
    fn every_workspace_is_owned_by_exactly_one_host_in_a_two_host_fleet() {
        // The issue's headline acceptance criterion, at its stated size.
        for key in fleet_keys() {
            let owners: Vec<usize> = (0..2).filter(|i| sharded(*i, 2).owns(&key)).collect();
            assert_eq!(owners.len(), 1, "{key} owned by {owners:?}, expected exactly one host");
        }
    }

    #[test]
    fn every_workspace_is_owned_by_exactly_one_host_across_fleet_sizes() {
        // The invariant is arithmetic, so it must hold at every fleet size,
        // not just the one the incident happened at.
        for count in 2..=8 {
            for key in fleet_keys() {
                let owners: Vec<usize> = (0..count)
                    .filter(|i| sharded(*i, count).owns(&key))
                    .collect();
                assert_eq!(
                    owners.len(),
                    1,
                    "{key} owned by {owners:?} in a {count}-host fleet, expected exactly one"
                );
            }
        }
    }

    // ---- AC2: token draw scales with workspaces, not workspaces x hosts ----

    #[test]
    fn total_role_ticks_per_interval_equal_the_workspace_count_not_workspaces_times_hosts() {
        let keys = fleet_keys();
        for count in 2..=8 {
            let ticks: usize = (0..count)
                .map(|i| keys.iter().filter(|k| sharded(i, count).owns(k)).count())
                .sum();
            assert_eq!(
                ticks,
                keys.len(),
                "a {count}-host fleet drew {ticks} role ticks for {} workspaces; sharding must \
                 make the draw scale with workspaces alone",
                keys.len()
            );
            // And the pre-#6374 behavior it replaces, for contrast.
            let unsharded: usize = (0..count)
                .map(|_| {
                    keys.iter()
                        .filter(|k| ShardPosture::Unsharded(UnshardedReason::NotConfigured).owns(k))
                        .count()
                })
                .sum();
            assert_eq!(unsharded, keys.len() * count);
        }
    }

    #[test]
    fn the_assignment_is_not_degenerate_across_a_four_host_fleet() {
        // A hash that mapped everything to one shard would satisfy "exactly
        // one owner" while delivering none of the point. Assert every shard
        // gets a real share of the 27-workspace fleet.
        let keys = fleet_keys();
        let loads: Vec<usize> = (0..4)
            .map(|i| keys.iter().filter(|k| sharded(i, 4).owns(k)).count())
            .collect();
        for (shard, load) in loads.iter().enumerate() {
            assert!(*load > 0, "shard {shard} owns nothing; loads={loads:?}");
            assert!(
                *load <= keys.len() / 2,
                "shard {shard} owns {load} of {} workspaces; loads={loads:?}",
                keys.len()
            );
        }
    }

    #[test]
    fn assignment_is_stable_across_repeated_resolution() {
        // Two "hosts" resolving independently must agree. This is the whole
        // cross-host correctness argument, expressed locally.
        let key = "rjwalters/loom";
        let first = sharded(0, 4).owns(key);
        for _ in 0..100 {
            assert_eq!(sharded(0, 4).owns(key), first);
        }
    }

    // ---- Unsharded fallbacks own everything (fail-safe direction) ----

    #[test]
    fn an_unsharded_posture_owns_every_workspace() {
        let posture = ShardPosture::Unsharded(UnshardedReason::NotConfigured);
        for key in fleet_keys() {
            assert!(posture.owns(&key));
        }
        assert!(!posture.is_sharded());
        assert_eq!(posture.index(), None);
        assert_eq!(posture.count(), None);
    }

    #[test]
    #[serial]
    fn an_out_of_range_index_falls_back_to_unsharded_rather_than_owning_nothing() {
        let _env = EnvGuard::capture();
        std::env::set_var(SHARD_INDEX_ENV, "4");
        std::env::set_var(SHARD_COUNT_ENV, "4");
        let posture = resolve_posture_from(None, Path::new("/repos/loom"));
        assert_eq!(
            posture,
            ShardPosture::Unsharded(UnshardedReason::IndexOutOfRange { index: 4, count: 4 })
        );
        assert!(posture.owns("rjwalters/loom"), "must not silently rotate nothing");
    }

    #[test]
    #[serial]
    fn a_zero_count_falls_back_to_unsharded() {
        let _env = EnvGuard::capture();
        std::env::set_var(SHARD_INDEX_ENV, "0");
        std::env::set_var(SHARD_COUNT_ENV, "0");
        assert_eq!(
            resolve_posture_from(None, Path::new("/repos/loom")),
            ShardPosture::Unsharded(UnshardedReason::ZeroCount)
        );
    }

    #[test]
    #[serial]
    fn a_single_shard_fleet_is_unsharded() {
        let _env = EnvGuard::capture();
        std::env::set_var(SHARD_INDEX_ENV, "0");
        std::env::set_var(SHARD_COUNT_ENV, "1");
        assert_eq!(
            resolve_posture_from(None, Path::new("/repos/loom")),
            ShardPosture::Unsharded(UnshardedReason::SingleShard)
        );
    }

    #[test]
    #[serial]
    fn an_index_without_a_count_falls_back_to_unsharded() {
        let _env = EnvGuard::capture();
        std::env::set_var(SHARD_INDEX_ENV, "1");
        assert_eq!(
            resolve_posture_from(None, Path::new("/repos/loom")),
            ShardPosture::Unsharded(UnshardedReason::Incomplete {
                have_index: true,
                have_count: false,
            })
        );
    }

    #[test]
    #[serial]
    fn a_count_without_an_index_falls_back_to_unsharded() {
        let _env = EnvGuard::capture();
        std::env::set_var(SHARD_COUNT_ENV, "4");
        assert_eq!(
            resolve_posture_from(None, Path::new("/repos/loom")),
            ShardPosture::Unsharded(UnshardedReason::Incomplete {
                have_index: false,
                have_count: true,
            })
        );
    }

    #[test]
    #[serial]
    fn a_malformed_knob_falls_back_to_unsharded_and_names_the_field() {
        let _env = EnvGuard::capture();
        std::env::set_var(SHARD_INDEX_ENV, "two");
        std::env::set_var(SHARD_COUNT_ENV, "4");
        let posture = resolve_posture_from(None, Path::new("/repos/loom"));
        let ShardPosture::Unsharded(UnshardedReason::Malformed { field, raw }) = &posture else {
            panic!("expected Malformed, got {posture:?}");
        };
        assert_eq!(field, SHARD_INDEX_ENV);
        assert_eq!(raw, "two");
        assert!(posture.owns("rjwalters/loom"));
    }

    #[test]
    #[serial]
    fn nothing_configured_resolves_to_not_configured() {
        let _env = EnvGuard::capture();
        assert_eq!(
            resolve_posture_from(None, Path::new("/repos/loom")),
            ShardPosture::Unsharded(UnshardedReason::NotConfigured)
        );
    }

    // ---- Precedence ----

    #[test]
    #[serial]
    fn env_overrides_config_for_both_knobs() {
        let _env = EnvGuard::capture();
        let block = serde_json::json!({ "shardIndex": 3, "shardCount": 4 });
        std::env::set_var(SHARD_INDEX_ENV, "1");
        std::env::set_var(SHARD_COUNT_ENV, "2");
        assert_eq!(
            resolve_posture_from(Some(&block), Path::new("/repos/loom")),
            ShardPosture::Sharded {
                index: 1,
                count: 2,
                index_source: ValueSource::Env,
                count_source: ValueSource::Env,
            }
        );
    }

    #[test]
    #[serial]
    fn the_count_may_come_from_config_while_the_index_comes_from_env() {
        // The intended deployment shape: a fleet-wide count committed to the
        // repo, a per-host index in the service unit.
        let _env = EnvGuard::capture();
        let block = serde_json::json!({ "shardCount": 4 });
        std::env::set_var(SHARD_INDEX_ENV, "2");
        assert_eq!(
            resolve_posture_from(Some(&block), Path::new("/repos/loom")),
            ShardPosture::Sharded {
                index: 2,
                count: 4,
                index_source: ValueSource::Env,
                count_source: ValueSource::Config,
            }
        );
    }

    // ---- The fleet-breaking misconfiguration ----

    #[test]
    #[serial]
    fn a_tracked_config_shard_index_is_refused_rather_than_honored() {
        let _env = EnvGuard::capture();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".loom")).expect("mkdir .loom");
        std::fs::write(
            root.join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"autonomous":{"roleRunner":{"shardIndex":1,"shardCount":4}}}"#,
        )
        .expect("write config");

        let block = serde_json::json!({ "shardIndex": 1, "shardCount": 4 });
        let posture = resolve_posture_from(Some(&block), root);
        assert_eq!(
            posture,
            ShardPosture::Unsharded(UnshardedReason::IndexFromTrackedConfig { index: 1 })
        );
        // Fail-safe: refusing must not stop role rotation, only un-shard it.
        assert!(posture.owns("rjwalters/loom"));
        assert!(posture.describe().contains("REFUSED"));
    }

    #[test]
    #[serial]
    fn an_env_index_still_shards_even_when_the_tracked_config_also_declares_one() {
        // The env value is per-host by construction, so it is legitimate and
        // overrides the (ignored) tracked one rather than tripping the refusal.
        let _env = EnvGuard::capture();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".loom")).expect("mkdir .loom");
        std::fs::write(
            root.join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"autonomous":{"roleRunner":{"shardIndex":1,"shardCount":4}}}"#,
        )
        .expect("write config");

        std::env::set_var(SHARD_INDEX_ENV, "2");
        let block = serde_json::json!({ "shardIndex": 1, "shardCount": 4 });
        assert_eq!(
            resolve_posture_from(Some(&block), root),
            ShardPosture::Sharded {
                index: 2,
                count: 4,
                index_source: ValueSource::Env,
                count_source: ValueSource::Config,
            }
        );
    }

    // ---- Shard key ----

    #[test]
    #[serial]
    fn an_explicit_config_key_wins_over_every_derived_source() {
        clear_nwo_cache();
        let resolved = resolve_shard_key(Path::new("/repos/loom"), Some("  rjwalters/loom  "));
        assert_eq!(resolved.key, "rjwalters/loom");
        assert_eq!(resolved.source, KeySource::ConfigExplicit);
        assert!(resolved.source.is_cross_host_stable());
    }

    #[test]
    #[serial]
    fn a_blank_explicit_key_is_ignored_and_falls_through() {
        clear_nwo_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("my-workspace");
        std::fs::create_dir_all(&root).expect("mkdir");
        let resolved = resolve_shard_key(&root, Some("   "));
        assert_eq!(resolved.key, "my-workspace");
        assert_eq!(resolved.source, KeySource::Basename);
    }

    #[test]
    #[serial]
    fn a_workspace_with_no_git_remote_falls_back_to_its_basename() {
        clear_nwo_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("lean-genius");
        std::fs::create_dir_all(&root).expect("mkdir");
        let resolved = resolve_shard_key(&root, None);
        assert_eq!(resolved.key, "lean-genius");
        assert_eq!(resolved.source, KeySource::Basename);
        assert!(
            !resolved.source.is_cross_host_stable(),
            "the basename fallback must advertise that it can diverge across hosts"
        );
    }

    #[test]
    fn key_source_labels_are_stable() {
        assert_eq!(KeySource::ConfigExplicit.label(), "config");
        assert_eq!(KeySource::GitRemote.label(), "git-remote");
        assert_eq!(KeySource::Basename.label(), "basename");
        assert_eq!(ValueSource::Env.label(), "env");
        assert_eq!(ValueSource::Config.label(), "config");
    }

    // ---- Decision ----

    #[test]
    #[serial]
    fn decide_with_reports_the_owning_shard_and_this_hosts_verdict() {
        clear_nwo_cache();
        let key = "rjwalters/loom";
        let owner = owning_shard(key, 4).expect("count > 0");
        let root = Path::new("/repos/loom");

        let mine = decide_with(sharded(owner, 4), root, Some(key));
        assert!(mine.owned);
        assert_eq!(mine.owning_shard, Some(owner));
        assert!(mine.describe(root).contains("OWNED here"));

        let theirs = decide_with(sharded((owner + 1) % 4, 4), root, Some(key));
        assert!(!theirs.owned);
        assert_eq!(theirs.owning_shard, Some(owner));
        assert!(theirs.describe(root).contains("not owned here"));
    }

    #[test]
    #[serial]
    fn decide_with_owns_everything_when_unsharded() {
        clear_nwo_cache();
        let root = Path::new("/repos/loom");
        let decision = decide_with(
            ShardPosture::Unsharded(UnshardedReason::NotConfigured),
            root,
            Some("rjwalters/loom"),
        );
        assert!(decision.owned);
        assert_eq!(decision.owning_shard, None);
        assert!(decision.describe(root).contains("OWNED here"));
    }

    #[test]
    fn every_unsharded_reason_describes_the_fallback_direction() {
        // Whatever went wrong, the operator must be able to read off that
        // this host is still rotating everything.
        let reasons = [
            UnshardedReason::NotConfigured,
            UnshardedReason::SingleShard,
            UnshardedReason::Incomplete {
                have_index: true,
                have_count: false,
            },
            UnshardedReason::Incomplete {
                have_index: false,
                have_count: true,
            },
            UnshardedReason::ZeroCount,
            UnshardedReason::IndexOutOfRange { index: 9, count: 4 },
            UnshardedReason::Malformed {
                field: SHARD_COUNT_ENV.to_string(),
                raw: "four".to_string(),
            },
            UnshardedReason::IndexFromTrackedConfig { index: 1 },
        ];
        for reason in reasons {
            let text = ShardPosture::Unsharded(reason.clone()).describe();
            assert!(
                text.contains("EVERY registered workspace"),
                "{reason:?} described as {text:?} without naming the fallback direction"
            );
        }
    }

    #[test]
    fn a_sharded_posture_describes_both_sources() {
        let text = sharded(2, 4).describe();
        assert!(text.contains("shard 2 of 4"), "{text}");
        assert!(text.contains("index from env"), "{text}");
        assert!(text.contains("count from config"), "{text}");
    }

    #[test]
    #[serial]
    fn log_decision_once_is_idempotent_for_an_unchanged_decision() {
        clear_decision_log();
        clear_nwo_cache();
        let root = Path::new("/repos/loom");
        let decision = decide_with(sharded(0, 4), root, Some("rjwalters/loom"));
        log_decision_once(root, &decision);
        let first = decision_logged()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(root)
            .cloned();
        log_decision_once(root, &decision);
        let second = decision_logged()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(root)
            .cloned();
        assert_eq!(first, second);
        assert_eq!(first, Some(decision.describe(root)));
    }

    #[test]
    #[serial]
    fn log_decision_once_re_records_a_changed_decision() {
        clear_decision_log();
        clear_nwo_cache();
        let root = Path::new("/repos/loom");
        let owned = decide_with(sharded(0, 4), root, Some("rjwalters/loom"));
        log_decision_once(root, &owned);
        let unsharded = decide_with(
            ShardPosture::Unsharded(UnshardedReason::NotConfigured),
            root,
            Some("rjwalters/loom"),
        );
        log_decision_once(root, &unsharded);
        assert_eq!(
            decision_logged()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(root)
                .cloned(),
            Some(unsharded.describe(root))
        );
    }

    // ---- The static ring is untouched by an enabled roster (#7690 Phase A's
    // invariant, which #7691 preserves: the static env pair OUTRANKS the
    // roster, so a host that sets it keeps #6374 verbatim) ----

    #[test]
    #[serial]
    fn decide_verdict_is_unchanged_whether_the_roster_is_enabled_or_not() {
        clear_nwo_cache();
        let root = Path::new("/repos/loom");
        let posture = sharded(1, 4);

        let baseline = decide_with(posture.clone(), root, Some("rjwalters/loom"));

        // Enabling the roster (even with a fully valid issue) must not change
        // `decide`'s verdict at all when a static shard index is in effect.
        std::env::set_var(roster::ROSTER_ENABLED_ENV, "1");
        std::env::set_var(roster::ROSTER_ISSUE_ENV, "rjwalters/loom#1234");
        let with_roster = decide_with(posture.clone(), root, Some("rjwalters/loom"));
        std::env::remove_var(roster::ROSTER_ENABLED_ENV);
        std::env::remove_var(roster::ROSTER_ISSUE_ENV);

        assert_eq!(baseline, with_roster);

        // Same check for the misconfigured (enabled, no issue) roster state.
        std::env::set_var(roster::ROSTER_ENABLED_ENV, "1");
        let with_misconfigured_roster = decide_with(posture, root, Some("rjwalters/loom"));
        std::env::remove_var(roster::ROSTER_ENABLED_ENV);

        assert_eq!(baseline, with_misconfigured_roster);
    }

    // ========================================================================
    // Roster-driven ring (Issue #7691, Phase B of #6704)
    // ========================================================================

    mod roster_mode {
        use super::*;
        use chrono::{DateTime, Duration as ChronoDuration, Utc};
        use std::collections::BTreeSet;

        const NOW: &str = "2026-01-01T10:00:00Z";
        const FLEET_CREATED: &str = "2026-01-01T00:00:00Z";
        const TTL: u64 = 900;
        const SETTLE: u64 = 900;
        const KEY: &str = "rjwalters/loom";

        fn dt(s: &str) -> DateTime<Utc> {
            DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
        }

        fn issue() -> roster::RosterIssueRef {
            roster::RosterIssueRef::parse("rjwalters/loom#1234").expect("valid ref")
        }

        fn active_config() -> roster::RosterConfig {
            roster::RosterConfig {
                state: roster::RosterState::Active(issue()),
                heartbeat_secs: 300,
                ttl_secs: TTL,
                settle_secs: SETTLE,
            }
        }

        fn disabled_config() -> roster::RosterConfig {
            roster::RosterConfig {
                state: roster::RosterState::Disabled,
                heartbeat_secs: 300,
                ttl_secs: TTL,
                settle_secs: SETTLE,
            }
        }

        /// One live roster record: created at `created`, last beat at `beat`,
        /// serving every key in `keys`.
        fn record(
            id: u64,
            host: &str,
            keys: &[&str],
            created: DateTime<Utc>,
            beat: DateTime<Utc>,
        ) -> roster::RosterComment {
            roster::RosterComment {
                id,
                host: host.to_string(),
                serves: keys.iter().map(|k| hash_key(k)).collect::<BTreeSet<u64>>(),
                created_at: created,
                updated_at: beat,
            }
        }

        /// A live three-host fleet at `now`: all created long ago, all still
        /// beating (last beat 60s back), all serving `keys`.
        fn live_fleet(now: DateTime<Utc>, keys: &[&str]) -> Vec<roster::RosterComment> {
            ["host-a", "host-b", "host-c"]
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    record(
                        u64::try_from(i).unwrap() + 1,
                        h,
                        keys,
                        dt(FLEET_CREATED),
                        now - ChronoDuration::seconds(60),
                    )
                })
                .collect()
        }

        fn snapshot(
            host: &str,
            comments: Vec<roster::RosterComment>,
            now: DateTime<Utc>,
        ) -> roster::RosterSnapshot {
            roster::RosterSnapshot {
                issue: issue(),
                host: host.to_string(),
                comments,
                ttl_secs: TTL,
                settle_secs: SETTLE,
                fetched_at: now,
            }
        }

        fn unsharded() -> ShardPosture {
            ShardPosture::Unsharded(UnshardedReason::NotConfigured)
        }

        // ---- Rank and size come from the roster, and say so ----

        #[test]
        #[serial]
        fn the_ring_rank_and_size_are_derived_from_the_live_roster() {
            clear_nwo_cache();
            roster::clear_generation_fence_for_tests();
            let now = dt(NOW);
            let root = Path::new("/repos/loom");
            let snap = snapshot("host-b", live_fleet(now, &[KEY]), now);

            let decision = decide_with_roster(
                unsharded(),
                root,
                Some(KEY),
                &active_config(),
                Some(&snap),
                now,
            );

            assert_eq!(
                decision.posture,
                ShardPosture::Sharded {
                    // host-b is second in the id-sorted ring of three.
                    index: 1,
                    count: 3,
                    index_source: ValueSource::Roster,
                    count_source: ValueSource::Roster,
                },
                "with no static index and a settled roster, (index, count) must come from the ring"
            );
            // `status` must be able to say where the numbers came from (AC).
            let summary = decision.posture.describe();
            assert!(summary.contains("index from roster"), "{summary}");
            assert!(summary.contains("count from roster"), "{summary}");
            // Ownership is still the ordinary arithmetic over the same hash.
            assert_eq!(decision.owning_shard, owning_shard(KEY, 3));
            assert_eq!(decision.owned, decision.owning_shard == Some(1));
            assert_eq!(decision.owned, decision.admits_role_tick());
            roster::clear_generation_fence_for_tests();
        }

        #[test]
        #[serial]
        fn exactly_one_live_member_owns_each_key_under_a_settled_ring() {
            clear_nwo_cache();
            let now = dt(NOW);
            let keys: Vec<String> = (0..27).map(|i| format!("2amlogic/repo-{i}")).collect();
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let comments = live_fleet(now, &key_refs);
            for key in &keys {
                let owners: Vec<&str> = ["host-a", "host-b", "host-c"]
                    .into_iter()
                    .filter(|h| roster_owns(&comments, h, key, now, TTL, SETTLE))
                    .collect();
                assert_eq!(owners.len(), 1, "{key} owned by {owners:?} under a settled ring");
            }
        }

        // ---- Escape-hatch precedence (AC3) ----

        #[test]
        #[serial]
        fn a_static_shard_index_beats_an_enabled_roster() {
            clear_nwo_cache();
            let now = dt(NOW);
            let root = Path::new("/repos/loom");
            let snap = snapshot("host-b", live_fleet(now, &[KEY]), now);

            let decision = decide_with_roster(
                sharded(1, 4),
                root,
                Some(KEY),
                &active_config(),
                Some(&snap),
                now,
            );

            assert_eq!(decision.roster, RosterMode::Off(RosterOff::StaticShardWins));
            assert_eq!(
                decision.posture,
                sharded(1, 4),
                "a resolved static pair must keep #6374's ring verbatim — it is the documented \
                 escape hatch for a roster outage"
            );
            // ...and it is byte-identical to the pre-roster decision.
            let mut pre_roster = decide_with(sharded(1, 4), root, Some(KEY));
            pre_roster.roster = RosterMode::Off(RosterOff::StaticShardWins);
            assert_eq!(decision, pre_roster);
        }

        #[test]
        #[serial]
        fn a_disabled_roster_is_byte_identical_to_the_static_decision() {
            clear_nwo_cache();
            let now = dt(NOW);
            let root = Path::new("/repos/loom");
            // Even with a live snapshot sitting in the cache, a disabled
            // roster must not be consulted at all.
            let snap = snapshot("host-b", live_fleet(now, &[KEY]), now);
            assert_eq!(
                decide_with_roster(
                    unsharded(),
                    root,
                    Some(KEY),
                    &disabled_config(),
                    Some(&snap),
                    now
                ),
                decide_with(unsharded(), root, Some(KEY)),
            );
        }

        #[test]
        #[serial]
        fn an_enabled_roster_this_host_has_never_read_falls_back_to_the_static_posture() {
            // Design record rung 4: "never got a roster at all" keeps #6374's
            // duplicate-biased fallback. Yielding here would let one typo in
            // `roster.issue` silently stop role rotation fleet-wide.
            clear_nwo_cache();
            let root = Path::new("/repos/loom");
            let decision =
                decide_with_roster(unsharded(), root, Some(KEY), &active_config(), None, dt(NOW));
            assert_eq!(decision.roster, RosterMode::Off(RosterOff::NeverJoined));
            assert!(decision.owned, "an unreachable roster must not stop role rotation");
            assert!(decision.admits_role_tick());
        }

        #[test]
        #[serial]
        fn a_snapshot_read_from_a_different_roster_issue_is_not_used() {
            clear_nwo_cache();
            let now = dt(NOW);
            let mut snap = snapshot("host-b", live_fleet(now, &[KEY]), now);
            snap.issue = roster::RosterIssueRef::parse("someone/else#7").unwrap();
            let decision = decide_with_roster(
                unsharded(),
                Path::new("/repos/loom"),
                Some(KEY),
                &active_config(),
                Some(&snap),
                now,
            );
            assert_eq!(decision.roster, RosterMode::Off(RosterOff::SnapshotForAnotherIssue));
            assert!(decision.admits_role_tick());
        }

        #[test]
        #[serial]
        fn a_misconfigured_roster_falls_back_to_the_static_posture() {
            clear_nwo_cache();
            let config = roster::RosterConfig {
                state: roster::RosterState::MisconfiguredNoIssue,
                ..active_config()
            };
            let decision = decide_with_roster(
                unsharded(),
                Path::new("/repos/loom"),
                Some(KEY),
                &config,
                None,
                dt(NOW),
            );
            assert_eq!(decision.roster, RosterMode::Off(RosterOff::Misconfigured));
            assert!(decision.admits_role_tick());
        }

        // ---- The inverted fail-safe, and the dispatcher's exemption from it ----

        #[test]
        #[serial]
        fn a_host_that_joined_and_then_lost_the_roster_yields_role_ticks() {
            clear_nwo_cache();
            roster::clear_generation_fence_for_tests();
            let now = dt(NOW);
            // This host's own heartbeat is 20m stale: it cannot know whether
            // the fleet has evicted it, so it yields (the inverted fail-safe).
            let mut comments = live_fleet(now, &[KEY]);
            comments[1].updated_at = now - ChronoDuration::seconds(1200);
            let snap = snapshot("host-b", comments, now);

            let decision = decide_with_roster(
                unsharded(),
                Path::new("/repos/loom"),
                Some(KEY),
                &active_config(),
                Some(&snap),
                now,
            );

            assert!(
                matches!(decision.roster, RosterMode::Yield(roster::RosterYield::SelfStale { .. })),
                "got {:?}",
                decision.roster
            );
            assert!(!decision.admits_role_tick(), "a fenced-out host must run NO role ticks");
            // ...but the DISPATCHER's preferred-slice consumer (#6243) keeps
            // the pre-roster verdict, or a fence yield would starve dispatch
            // instead of merely pausing role rotation.
            assert!(
                decision.owned,
                "a roster yield must degrade to work_finder's work-conserving fallback, never to \
                 `owns nothing`"
            );
            assert_eq!(
                decision.posture,
                unsharded(),
                "the dispatcher must see exactly the posture it saw before the roster existed"
            );
            roster::clear_generation_fence_for_tests();
        }

        #[test]
        #[serial]
        fn the_describe_line_names_the_fence_state() {
            clear_nwo_cache();
            roster::clear_generation_fence_for_tests();
            let now = dt(NOW);
            let root = Path::new("/repos/loom");
            let mut comments = live_fleet(now, &[KEY]);
            comments[1].updated_at = now - ChronoDuration::seconds(1200);
            let yielded = decide_with_roster(
                unsharded(),
                root,
                Some(KEY),
                &active_config(),
                Some(&snapshot("host-b", comments, now)),
                now,
            );
            let line = yielded.describe(root);
            assert!(line.contains("YIELDING"), "{line}");
            assert!(line.contains("not owned here"), "{line}");

            // Fresh process state for the second half: the eviction boundary
            // the yielding view above carried has already ratcheted this
            // process's high-water mark, and a host whose record expired
            // rejoins with a NEW comment id in production (see
            // `resolve_publish_action`), never by silently re-freshening the
            // same one.
            roster::clear_generation_fence_for_tests();
            let admitted = decide_with_roster(
                unsharded(),
                root,
                Some(KEY),
                &active_config(),
                Some(&snapshot("host-b", live_fleet(now, &[KEY]), now)),
                now,
            );
            assert!(admitted.describe(root).contains("roster: ring settled"));
            roster::clear_generation_fence_for_tests();
        }

        // ====================================================================
        // Adversarial scenarios
        //
        // These drive `roster::admission` + `ShardPosture::owns` directly
        // rather than `decide_with_roster`, for one reason: the generation
        // high-water mark is process-global (one daemon = one process), so two
        // *simulated* hosts sharing this test process would contaminate each
        // other's fence. The arithmetic below is exactly what
        // `decide_with_roster` does with an admitted ring — pinned by
        // `the_ring_rank_and_size_are_derived_from_the_live_roster` above —
        // and passing `None` for the high-water mark is the *weaker*
        // assumption, so a fence that holds here holds a fortiori in
        // production.
        // ====================================================================

        fn roster_owns(
            view: &[roster::RosterComment],
            host: &str,
            key: &str,
            now: DateTime<Utc>,
            ttl: u64,
            settle: u64,
        ) -> bool {
            match roster::admission(view, host, hash_key(key), now, ttl, settle, None) {
                roster::RosterAdmission::Ring { index, count, .. } => ShardPosture::Sharded {
                    index,
                    count,
                    index_source: ValueSource::Roster,
                    count_source: ValueSource::Roster,
                }
                .owns(key),
                roster::RosterAdmission::Yield(_) => false,
            }
        }

        /// SPLIT VIEW (AC): two hosts reading the same roster at different
        /// staleness must never both own the same key at the same instant.
        ///
        /// A lagging host is modelled as reading the true comment set as of
        /// `t - lag` while evaluating at `t` — which is what an ETag-cached or
        /// replica-lagged read actually looks like, including the fact that
        /// the host's own record looks `lag` seconds staler to itself.
        #[test]
        #[serial]
        fn a_split_view_never_gives_one_key_two_owners() {
            let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let join = dt("2026-01-01T02:00:00Z");

            // The true, forge-side comment set at instant `t`.
            let truth = |t: DateTime<Utc>| {
                let mut c = live_fleet(t, &key_refs);
                if t >= join {
                    c.push(record(4, "host-d", &key_refs, join, t - ChronoDuration::seconds(60)));
                }
                c
            };

            // Deliberately divergent read staleness, including one host
            // lagging far enough that only the SELF-LIVENESS condition can
            // save the invariant.
            let hosts = [
                ("host-a", 0i64),
                ("host-b", 120),
                ("host-c", 600),
                ("host-d", 1500),
            ];

            let start = join - ChronoDuration::seconds(1800);
            for step in 0..240 {
                let t = start + ChronoDuration::seconds(step * 30);
                for key in &keys {
                    let owners: Vec<&str> = hosts
                        .iter()
                        .filter(|(host, lag)| {
                            let view = truth(t - ChronoDuration::seconds(*lag));
                            roster_owns(&view, host, key, t, TTL, SETTLE)
                        })
                        .map(|(host, _)| *host)
                        .collect();
                    assert!(
                        owners.len() <= 1,
                        "{key} owned by {owners:?} at {t} — a membership disagreement must YIELD, \
                         never duplicate (#6704)"
                    );
                }
            }
        }

        /// SPLIT VIEW, second half (AC): outside the settle window every key
        /// still has an owner — the fence trades a bounded gap for the
        /// duplicate, it does not strand work indefinitely.
        #[test]
        #[serial]
        fn every_key_has_exactly_one_owner_outside_the_settle_window() {
            let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let join = dt("2026-01-01T02:00:00Z");
            let truth = |t: DateTime<Utc>| {
                let mut c = live_fleet(t, &key_refs);
                if t >= join {
                    c.push(record(4, "host-d", &key_refs, join, t - ChronoDuration::seconds(60)));
                }
                c
            };
            // Both hosts read within the TTL, as a healthy fleet does.
            let hosts = [
                ("host-a", 0i64),
                ("host-b", 120),
                ("host-c", 240),
                ("host-d", 60),
            ];
            let settle_window = join..(join + ChronoDuration::seconds(SETTLE as i64 + 240));

            let start = join - ChronoDuration::seconds(1800);
            for step in 0..240 {
                let t = start + ChronoDuration::seconds(step * 30);
                if settle_window.contains(&t) {
                    continue;
                }
                for key in &keys {
                    let owners: Vec<&str> = hosts
                        .iter()
                        .filter(|(host, lag)| {
                            if t < join && *host == "host-d" {
                                return false; // not a member yet
                            }
                            let view = truth(t - ChronoDuration::seconds(*lag));
                            roster_owns(&view, host, key, t, TTL, SETTLE)
                        })
                        .map(|(host, _)| *host)
                        .collect();
                    assert_eq!(
                        owners.len(),
                        1,
                        "{key} owned by {owners:?} at {t} (outside the settle window every key \
                         must have exactly one owner)"
                    );
                }
            }
        }

        /// KILL HOST (AC): when a member's record expires, a survivor picks up
        /// its slice within `ttl + settleSecs` (+ one role interval for tick
        /// alignment, which is the cadence, not the fence) — and **not
        /// before**.
        #[test]
        #[serial]
        fn a_dead_hosts_slice_is_reassigned_after_ttl_plus_settle_and_not_before() {
            let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let death = dt("2026-01-01T02:00:00Z");
            let survivors = ["host-a", "host-b"];

            // host-c's last beat is at `death`; a, b keep beating.
            let view = |t: DateTime<Utc>| {
                let mut c = live_fleet(t, &key_refs);
                c[2].updated_at = death;
                c
            };

            // Its slice: the keys host-c owned while it was alive.
            let just_before_death = death - ChronoDuration::seconds(1);
            let orphaned: Vec<&String> = keys
                .iter()
                .filter(|k| {
                    roster_owns(
                        &view(just_before_death),
                        "host-c",
                        k,
                        just_before_death,
                        TTL,
                        SETTLE,
                    )
                })
                .collect();
            assert!(!orphaned.is_empty(), "precondition: host-c must own part of the ring");

            // NOT BEFORE: for the whole `ttl + settle` window, no survivor
            // touches the orphaned slice.
            let reassigned_at = death + ChronoDuration::seconds((TTL + SETTLE) as i64);
            let mut t = death;
            while t < reassigned_at {
                for key in &orphaned {
                    for host in survivors {
                        assert!(
                            !roster_owns(&view(t), host, key, t, TTL, SETTLE),
                            "{host} picked up {key} at {t}, before ttl+settle had elapsed — the \
                             reassignment window must be bounded BELOW as well as above (#6704)"
                        );
                    }
                }
                t += ChronoDuration::seconds(30);
            }

            // AND NOT NEVER: at `death + ttl + settle` every orphaned key has
            // exactly one live owner again.
            for key in &orphaned {
                let owners: Vec<&str> = survivors
                    .into_iter()
                    .filter(|h| {
                        roster_owns(&view(reassigned_at), h, key, reassigned_at, TTL, SETTLE)
                    })
                    .collect();
                assert_eq!(
                    owners.len(),
                    1,
                    "{key} had owners {owners:?} at ttl+settle after its host died; a dead host's \
                     slice must be reassigned within a bounded window (#6704 AC2)"
                );
            }
            // And the whole ring is covered again, not just the orphans.
            for key in &keys {
                let owners: Vec<&str> = survivors
                    .into_iter()
                    .filter(|h| {
                        roster_owns(&view(reassigned_at), h, key, reassigned_at, TTL, SETTLE)
                    })
                    .collect();
                assert_eq!(owners.len(), 1, "{key} owned by {owners:?} after reassignment");
            }
        }

        /// SELF-FENCE (AC), at the surface that spends tokens: a host whose
        /// own heartbeat is stale runs no roster-gated role tick for ANY key.
        #[test]
        #[serial]
        fn a_host_with_a_stale_heartbeat_runs_no_roster_gated_role_ticks() {
            clear_nwo_cache();
            roster::clear_generation_fence_for_tests();
            let now = dt(NOW);
            let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let mut comments = live_fleet(now, &key_refs);
            comments[0].updated_at = now - ChronoDuration::seconds(1200);
            let snap = snapshot("host-a", comments, now);

            for key in &keys {
                let decision = decide_with_roster(
                    unsharded(),
                    Path::new("/repos/loom"),
                    Some(key),
                    &active_config(),
                    Some(&snap),
                    now,
                );
                assert!(
                    !decision.admits_role_tick(),
                    "{key}: a self-fenced host must run no role ticks at all"
                );
                assert!(decision.owned, "{key}: dispatch preference must be unaffected");
            }
            roster::clear_generation_fence_for_tests();
        }

        /// JOIN FENCE (AC) at the same surface: a host that has just joined
        /// runs nothing until its own record is `ttl` old.
        #[test]
        #[serial]
        fn a_newly_joined_host_runs_nothing_until_its_record_is_ttl_old() {
            clear_nwo_cache();
            roster::clear_generation_fence_for_tests();
            let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let join = dt(NOW);

            let view = |t: DateTime<Utc>| {
                let mut c = live_fleet(t, &key_refs);
                c.push(record(4, "host-d", &key_refs, join, t - ChronoDuration::seconds(60)));
                c
            };

            // Anywhere inside its first ttl, the joiner is fenced out for
            // every key.
            let mut t = join;
            while t < join + ChronoDuration::seconds(TTL as i64) {
                for key in &keys {
                    let decision = decide_with_roster(
                        unsharded(),
                        Path::new("/repos/loom"),
                        Some(key),
                        &active_config(),
                        Some(&snapshot("host-d", view(t), t)),
                        t,
                    );
                    assert!(
                        !decision.admits_role_tick(),
                        "{key}: a joiner must not act until its own record is a full ttl old \
                         (t={t})"
                    );
                }
                t += ChronoDuration::seconds(120);
            }

            // Once both the join fence and the settle window have passed, it
            // takes up its share.
            let after = join + ChronoDuration::seconds((TTL + SETTLE) as i64);
            let owned: Vec<&String> = keys
                .iter()
                .filter(|key| {
                    decide_with_roster(
                        unsharded(),
                        Path::new("/repos/loom"),
                        Some(key),
                        &active_config(),
                        Some(&snapshot("host-d", view(after), after)),
                        after,
                    )
                    .admits_role_tick()
                })
                .collect();
            assert!(
                !owned.is_empty(),
                "a joined, settled host must eventually carry part of the ring"
            );
            roster::clear_generation_fence_for_tests();
        }

        // ---- End-to-end through `decide` (config + snapshot cache) ----

        #[test]
        #[serial]
        fn decide_reads_the_roster_from_config_and_the_snapshot_cache() {
            let _env = EnvGuard::capture();
            let _roster_env = RosterEnvGuard::capture();
            clear_nwo_cache();
            roster::clear_generation_fence_for_tests();

            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            std::fs::create_dir_all(root.join(".loom")).expect("mkdir .loom");
            std::fs::write(
                root.join(crate::config_resolver::LEGACY_CONFIG_REL),
                r#"{"autonomous":{"roleRunner":{"enabled":true,"shardKey":"rjwalters/loom",
                   "roster":{"enabled":true,"issue":"rjwalters/loom#1234"}}}}"#,
            )
            .expect("write config");

            // No snapshot yet: the never-joined fallback, NOT a yield.
            roster::clear_roster_snapshot_for_tests();
            let before = decide(root);
            assert_eq!(before.roster, RosterMode::Off(RosterOff::NeverJoined));
            assert!(before.admits_role_tick());

            // The heartbeat task publishes a snapshot; now the ring is live.
            let now = chrono::Utc::now();
            roster::set_roster_snapshot(snapshot("host-c", live_fleet(now, &[KEY]), now));
            let after = decide(root);
            assert!(matches!(after.roster, RosterMode::Ring { .. }), "got {:?}", after.roster);
            assert_eq!(after.posture.count(), Some(3));
            assert_eq!(after.posture.index(), Some(2), "host-c is third in the ring");
            assert!(after.posture.describe().contains("from roster"));

            roster::clear_roster_snapshot_for_tests();
            roster::clear_generation_fence_for_tests();
        }

        /// Restore the roster env knobs, so a stray `LOOM_ROLE_RUNNER_ROSTER`
        /// in the ambient environment cannot steer the config-driven test.
        struct RosterEnvGuard {
            saved: Vec<(&'static str, Option<String>)>,
        }

        impl RosterEnvGuard {
            fn capture() -> Self {
                let names = [
                    roster::ROSTER_ENABLED_ENV,
                    roster::ROSTER_ISSUE_ENV,
                    roster::ROSTER_HEARTBEAT_SECS_ENV,
                    roster::ROSTER_TTL_SECS_ENV,
                    roster::ROSTER_SETTLE_SECS_ENV,
                ];
                let saved = names.iter().map(|n| (*n, std::env::var(*n).ok())).collect();
                for n in names {
                    std::env::remove_var(n);
                }
                Self { saved }
            }
        }

        impl Drop for RosterEnvGuard {
            fn drop(&mut self) {
                for (name, value) in &self.saved {
                    match value {
                        Some(v) => std::env::set_var(name, v),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }
}
