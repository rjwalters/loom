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
mod tests;
