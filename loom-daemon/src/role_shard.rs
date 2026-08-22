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
//! ## What this does NOT do (deferred to Issue #6704)
//!
//! The assignment is **static**: it is derived from `(shardIndex, shardCount)`,
//! not from a live roster. Killing a host does **not** reassign its slice — its
//! workspaces simply stop rotating until an operator lowers `shardCount` (or
//! points a survivor at the vacated index). Dynamic roster membership with
//! bounded reassignment is deliberately deferred to #6704; it needs a liveness
//! protocol whose failure modes are exactly the zero-or-two-owner races this
//! static scheme avoids by construction — two hosts with different views of
//! roster membership disagree about the ring *size*, and therefore about every
//! workspace's owner, not just the departed host's slice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

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
}

impl ValueSource {
    /// A short, stable label for logs and status output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Config => "config",
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

/// One workspace's resolved sharding decision: the host posture, the
/// workspace's key, and whether this host owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardDecision {
    /// This host's posture.
    pub posture: ShardPosture,
    /// The workspace's resolved key.
    pub key: ResolvedKey,
    /// Whether this host runs role ticks for this workspace.
    pub owned: bool,
    /// Which shard owns it, or `None` when unsharded.
    pub owning_shard: Option<usize>,
}

impl ShardDecision {
    /// A one-line log/status description naming `root`, the key (and its
    /// tier), the owning shard, and this host's verdict.
    #[must_use]
    pub fn describe(&self, root: &Path) -> String {
        let verdict = if self.owned {
            "OWNED here"
        } else {
            "not owned here"
        };
        match (&self.posture, self.owning_shard) {
            (ShardPosture::Sharded { index, count, .. }, Some(owner)) => format!(
                "role_runner: shard decision for {} — key={:?} ({}), owner=shard {owner} of \
                 {count}, this host=shard {index} => {verdict} (#6374)",
                root.display(),
                self.key.key,
                self.key.source.label(),
            ),
            _ => format!(
                "role_runner: shard decision for {} — sharding {} => {verdict} (#6374)",
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
#[must_use]
pub fn decide(root: &Path) -> ShardDecision {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let block = crate::config_resolver::get_path(&effective, "autonomous.roleRunner");
    let posture = resolve_posture_from(block, root);
    let explicit = block
        .and_then(|b| b.get(SHARD_KEY_KEY))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    decide_with(posture, root, explicit.as_deref())
}

/// The [`decide`] core with the posture and explicit key already resolved —
/// the seam tests drive.
#[must_use]
pub fn decide_with(
    posture: ShardPosture,
    root: &Path,
    explicit_key: Option<&str>,
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
}
