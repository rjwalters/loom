//! One host per repo for the host-redundant read passes (#11301,
//! https://github.com/2AMLogic/2am/issues/4059).
//!
//! # The problem
//!
//! Two per-root loops do the same forge reads on every daemon of a fleet:
//! the `loom:blocked` release pass ([`crate::stale_blocked::release_gh`],
//! with the unnamed-queue pass riding it) and the `fleet.state` collector's
//! label listings ([`crate::observability::fleet_state`]). Neither has any
//! per-host input, so on N hosts each repo's pass runs N times and the forge
//! spend grows with the host count. The role-runner shard
//! ([`crate::role_shard`]) used to bound the release pass, but a fleet that
//! turned it off (it also sliced dispatch) unshards this pass with it.
//!
//! This module is a separate gate for those passes only. It never touches
//! role ticks or dispatch.
//!
//! # The mechanism
//!
//! * **Members come from the fleet store**, from the snapshot fleet-sync
//!   already keeps, read offline after each fleet-sync pass ([`refresh`]): no
//!   forge call. A host is a member iff, in the compiled `fleet.json`, its
//!   machine tier (`config.defaults` deep-merged with
//!   `config.hosts.<host>.defaults`) sets [`ENABLED_KEY`] and `fleet.autoApply`
//!   (so it registers the whole store roster), and its run state is
//!   `running`. Every host derives every peer's membership from one document,
//!   so two hosts on one store commit compute one ring.
//! * **Only store-roster repos are sharded**: a local root the roster does not
//!   name runs unsharded, so a repo is never assigned to a host lacking it.
//! * **Rendezvous hashing** ([`rank`]): each repo orders the members by
//!   [`score`]; rank 1 is the **owner**, rank 2 the **backup**. A host that
//!   leaves moves only its own repos; one that joins takes about 1/N.
//!
//! # Fail direction
//!
//! Every doubt resolves to [`Verdict::Unsharded`], which runs the pass, as
//! before this module: duplicate, never drop. A stale snapshot is *not* a
//! doubt below [`DEFAULT_MAX_STALE_SECS`]: when the forge is unreachable every
//! host keeps its last snapshot and they still agree, and unsharding then
//! would multiply the load by N exactly while GitHub is rate-limiting.
//!
//! # No lease, on purpose
//!
//! Both passes already run N-way concurrently and tolerate it, so a
//! transition's brief duplicate is the old behaviour. A per-repo lease would
//! spend forge writes on every pass, the very spend this removes.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::fleet_store::compiled::Compiled;
use crate::fleet_store::state::RunState;

/// Machine-tier switch. Read from the store for every host, this one included.
pub const ENABLED_KEY: &str = "autonomous.passShard.enabled";
/// Every how many due slots a backup runs the release pass.
pub const BACKUP_EVERY_KEY: &str = "autonomous.passShard.backupEvery";
/// The ring's maximum age (seconds since the forge last confirmed the
/// snapshot) before this host unshards.
pub const MAX_STALE_KEY: &str = "autonomous.passShard.maxStaleSecs";
/// Host-local break-glass: `0` forces this host unsharded. It can only turn
/// sharding off: peers cannot see it, so forcing it on would leave repos no
/// one picks up.
pub const KILL_ENV: &str = "LOOM_PASS_SHARD";
/// [`BACKUP_EVERY_KEY`]'s default: about 30 minutes at the 300 s cadence.
pub const DEFAULT_BACKUP_EVERY: u32 = 6;
/// [`MAX_STALE_KEY`]'s default: six hours.
pub const DEFAULT_MAX_STALE_SECS: i64 = 21_600;

// ============================================================================
// Hashing
// ============================================================================

/// The murmur3 64-bit finalizer: FNV-1a alone mixes its last bytes weakly,
/// and member names differ in one late character (`…-1` / `…-2`).
const fn fmix64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// `member`'s rendezvous score for `repo`: `fmix64(fnv1a64(member ‖ NUL ‖
/// repo))`. Specified byte for byte (FNV-1a is [`crate::role_shard::hash_key`],
/// pinned by its known-vector test), so every host and build agrees.
#[must_use]
pub fn score(member: &str, repo: &str) -> u64 {
    fmix64(crate::role_shard::hash_key(&format!("{member}\u{0}{repo}")))
}

/// `members` in rendezvous order for `repo`: highest score first, ties by
/// name. The first is the owner, the second the backup.
#[must_use]
pub fn rank<'a>(members: &'a [String], repo: &str) -> Vec<&'a str> {
    let mut ranked: Vec<(u64, &str)> = members
        .iter()
        .map(|m| (score(m, repo), m.as_str()))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    ranked.into_iter().map(|(_, m)| m).collect()
}

// ============================================================================
// The ring
// ============================================================================

/// The ring as of one store snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ring {
    /// The store commit it was computed from.
    pub commit: String,
    /// This host's store name.
    pub host: String,
    /// Every member, sorted.
    pub members: Vec<String>,
    /// The sharded repos: the store roster's dispatched/maintained,
    /// non-firewall records, as lowercase `owner/repo`.
    pub repos: BTreeSet<String>,
    /// Whether the store turns sharding on for this host.
    pub self_enabled: bool,
    /// [`BACKUP_EVERY_KEY`], this host's value.
    pub backup_every: u32,
    /// [`MAX_STALE_KEY`], this host's value.
    pub max_stale_secs: i64,
    /// When the forge last confirmed the snapshot.
    pub confirmed_at: DateTime<Utc>,
}

impl Ring {
    /// Whether this host is in the ring.
    #[must_use]
    pub fn self_member(&self) -> bool {
        self.members.contains(&self.host)
    }

    /// One line for the edge log: the commit and the members.
    #[must_use]
    pub fn describe(&self) -> String {
        let commit = &self.commit[..self.commit.len().min(12)];
        format!(
            "pass_shard: store {commit}: {} member(s) [{}], {} sharded repo(s); this host {} is {} \
             (#11301)",
            self.members.len(),
            self.members.join(", "),
            self.repos.len(),
            self.host,
            if self.self_member() {
                "a member"
            } else if self.self_enabled {
                "enabled but not a member (not running, or fleet.autoApply off)"
            } else {
                "not enabled"
            },
        )
    }
}

/// `host`'s machine tier as the store renders it, or `None` with no overlay.
fn machine_tier(doc: &Compiled, host: &str) -> Option<Value> {
    let overlay = doc.host_defaults(host)?;
    Some(crate::config_resolver::deep_merge(doc.fleet_defaults(), overlay))
}

fn tier_bool(tier: &Value, key: &str) -> bool {
    crate::config_resolver::get_path(tier, key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn tier_u64(tier: Option<&Value>, key: &str) -> Option<u64> {
    tier.and_then(|t| crate::config_resolver::get_path(t, key))
        .and_then(Value::as_u64)
}

/// The store roster's sharded repos, lowercase `owner/repo`.
fn roster_repos(doc: &Compiled) -> BTreeSet<String> {
    doc.roster()
        .get("repos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| {
            let fleet = r
                .get("fleet")
                .is_some_and(|f| f.as_bool() == Some(true) || f.as_str() == Some("maintain"));
            let firewall = r.get("firewall").and_then(Value::as_bool) == Some(true);
            fleet && !firewall
        })
        .filter_map(|r| r.get("remote").and_then(Value::as_str))
        .filter_map(crate::forge_etag_store::parse_remote_url)
        .map(|(_, slug)| slug.to_ascii_lowercase())
        .collect()
}

/// The ring `doc` gives `host`. `run_state(h)` resolves a host's desired
/// state from the same snapshot (`None` when it cannot be read: not
/// `running`, so not a member). Pure.
#[must_use]
pub fn ring_from_compiled(
    doc: &Compiled,
    host: &str,
    commit: &str,
    confirmed_at: DateTime<Utc>,
    run_state: &dyn Fn(&str) -> Option<RunState>,
) -> Ring {
    let mut members: Vec<String> = doc
        .host_names()
        .into_iter()
        .filter(|h| {
            machine_tier(doc, h).is_some_and(|tier| {
                tier_bool(&tier, ENABLED_KEY)
                    && crate::fleet_sync::resolve_auto_apply(&tier, &|_| None)
            })
        })
        .filter(|h| run_state(h) == Some(RunState::Running))
        .collect();
    members.sort();
    let own = machine_tier(doc, host);
    Ring {
        commit: commit.to_string(),
        host: host.to_string(),
        members,
        repos: roster_repos(doc),
        self_enabled: own.as_ref().is_some_and(|t| tier_bool(t, ENABLED_KEY)),
        backup_every: tier_u64(own.as_ref(), BACKUP_EVERY_KEY)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_BACKUP_EVERY),
        max_stale_secs: tier_u64(own.as_ref(), MAX_STALE_KEY)
            .and_then(|v| i64::try_from(v).ok())
            .unwrap_or(DEFAULT_MAX_STALE_SECS),
        confirmed_at,
    }
}

// ============================================================================
// Verdict
// ============================================================================

/// Why a pass runs here without sharding. Every variant runs the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsharded {
    /// No ring: fleet-sync is off or has not finished a pass.
    NoRing,
    /// [`KILL_ENV`] is `0`.
    KillSwitch,
    /// The store does not enable sharding for this host.
    Off,
    /// Enabled, but not a member (not `running`, or `fleet.autoApply` off).
    NotMember,
    /// Fewer than two members: nothing to share.
    TooFewMembers,
    /// The ring is older than its `maxStaleSecs`.
    Stale,
    /// The repo is not in the store roster, or its slug is unknown.
    NotInRoster,
}

impl Unsharded {
    /// A short, stable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoRing => "no-ring",
            Self::KillSwitch => "kill-switch",
            Self::Off => "off",
            Self::NotMember => "not-member",
            Self::TooFewMembers => "too-few-members",
            Self::Stale => "stale",
            Self::NotInRoster => "not-in-roster",
        }
    }
}

/// This host's verdict for one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run the pass, as without sharding.
    Unsharded(Unsharded),
    /// This host owns the repo: run the pass.
    Owner,
    /// This host is the repo's backup: the release pass runs on every
    /// `every`-th due slot; listings do not run.
    Backup {
        /// The owner.
        owner: String,
        /// The ring's `backupEvery`.
        every: u32,
    },
    /// Another host owns the repo: skip.
    NotOwner {
        /// The owner.
        owner: String,
    },
}

impl Verdict {
    /// Whether the pass runs every time it is due here (no backup thinning).
    #[must_use]
    pub const fn runs(&self) -> bool {
        matches!(self, Self::Unsharded(_) | Self::Owner)
    }
}

/// The verdict for `slug` under `ring` at `now`. `kill` is [`KILL_ENV`] set to
/// `0`. Pure.
#[must_use]
pub fn decide_with(
    ring: Option<&Ring>,
    slug: Option<&str>,
    kill: bool,
    now: DateTime<Utc>,
) -> Verdict {
    let Some(ring) = ring else {
        return Verdict::Unsharded(Unsharded::NoRing);
    };
    let unsharded = if kill {
        Some(Unsharded::KillSwitch)
    } else if !ring.self_enabled {
        Some(Unsharded::Off)
    } else if !ring.self_member() {
        Some(Unsharded::NotMember)
    } else if ring.members.len() < 2 {
        Some(Unsharded::TooFewMembers)
    } else if (now - ring.confirmed_at).num_seconds() > ring.max_stale_secs {
        Some(Unsharded::Stale)
    } else {
        None
    };
    if let Some(reason) = unsharded {
        return Verdict::Unsharded(reason);
    }
    let Some(slug) = slug
        .map(str::to_ascii_lowercase)
        .filter(|s| ring.repos.contains(s))
    else {
        return Verdict::Unsharded(Unsharded::NotInRoster);
    };
    let ranked = rank(&ring.members, &slug);
    match ranked.iter().position(|m| *m == ring.host) {
        Some(0) => Verdict::Owner,
        Some(1) => Verdict::Backup {
            owner: ranked[0].to_string(),
            every: ring.backup_every,
        },
        _ => Verdict::NotOwner {
            owner: ranked[0].to_string(),
        },
    }
}

// ============================================================================
// Process state
// ============================================================================

fn ring_cell() -> &'static Mutex<Option<Ring>> {
    static CELL: OnceLock<Mutex<Option<Ring>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// The ring of the last fleet-sync pass, if any.
#[must_use]
pub fn current() -> Option<Ring> {
    ring_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Replace the ring, logging when its members, repos or this host's switch
/// changed (not on every pass).
pub fn publish(ring: Option<Ring>) {
    let mut cell = ring_cell().lock().unwrap_or_else(PoisonError::into_inner);
    let shape = |r: &Ring| (r.members.clone(), r.repos.clone(), r.self_enabled);
    let changed = match (cell.as_ref(), ring.as_ref()) {
        (Some(was), Some(now)) => shape(was) != shape(now),
        (None, Some(_)) => true,
        _ => false,
    };
    if let Some(now) = ring.as_ref().filter(|_| changed) {
        if now.self_enabled || cell.as_ref().is_some_and(|w| w.self_enabled) {
            log::info!("{}", now.describe());
        } else {
            log::debug!("{}", now.describe());
        }
    }
    *cell = ring;
}

fn kill_switch() -> bool {
    std::env::var(KILL_ENV).is_ok_and(|v| v.trim() == "0")
}

/// This host's verdict for `slug` now.
#[must_use]
pub fn decide_slug(slug: &str) -> Verdict {
    decide_with(current().as_ref(), Some(slug), kill_switch(), Utc::now())
}

/// This host's verdict for the workspace at `root` now (its `origin` slug).
#[must_use]
pub fn decide(root: &Path) -> Verdict {
    let slug = crate::forge_etag_store::remote_identity(root).map(|(_, slug)| slug);
    decide_with(current().as_ref(), slug.as_deref(), kill_switch(), Utc::now())
}

/// Per-root count of due slots a backup skipped since it last ran.
fn backup_slots() -> &'static Mutex<HashMap<String, u32>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether a backup runs this due slot for `key`: the first slot after it
/// became backup, then every `every`-th. Records the slot.
#[must_use]
pub fn backup_slot_due(key: &str, every: u32) -> bool {
    let mut slots = backup_slots()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let skipped = slots.entry(key.to_string()).or_insert(0);
    if *skipped == 0 || *skipped >= every.max(1) {
        *skipped = 1;
        true
    } else {
        *skipped += 1;
        false
    }
}

/// Whether the release pass runs for `root` on this due slot: always unless
/// sharded away; a backup runs every `backupEvery`-th slot. Records the slot.
#[must_use]
pub fn admits_due_slot(root: &Path) -> bool {
    match decide(root) {
        Verdict::Unsharded(_) | Verdict::Owner => true,
        Verdict::Backup { every, .. } => backup_slot_due(&root.to_string_lossy(), every),
        Verdict::NotOwner { .. } => false,
    }
}

/// Warn, once per root, when this host is a ring member but does not serve
/// `root`'s release pass: its peers skip the repos it owns, so nobody runs
/// them. Set the flag only on hosts that run the work finder.
pub fn warn_if_member_not_serving(root: &Path) {
    static WARNED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    if !current().is_some_and(|r| r.self_enabled && r.self_member()) {
        return;
    }
    let key = root.to_string_lossy().into_owned();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if warned.insert(key) {
        log::error!(
            "pass_shard: this host is a pass-shard member but does not serve {}'s release pass \
             (work finder and role runner off here); the repos it owns go unswept until \
             {ENABLED_KEY} is removed for it (#11301)",
            root.display()
        );
    }
}

/// Recompute the ring from the cached store snapshot after a fleet-sync pass.
/// Offline: never contacts the forge. A snapshot that cannot be read or is
/// not a compiled `fleet.json` clears the ring (every pass then unshards).
pub fn refresh(
    cache_dir: &Path,
    location: &crate::fleet_store::StoreLocation,
    host: &str,
    now: DateTime<Utc>,
) {
    publish(read_ring(cache_dir, location, host, now));
}

fn read_ring(
    cache_dir: &Path,
    location: &crate::fleet_store::StoreLocation,
    host: &str,
    now: DateTime<Utc>,
) -> Option<Ring> {
    use crate::fleet_store::fetch::{self, Policy, Reply, Transport};
    struct Offline;
    impl Transport for Offline {
        fn get(&self, path: &str, _: Option<&str>, _: Option<&str>) -> anyhow::Result<Reply> {
            anyhow::bail!("pass_shard reads offline ({path})")
        }
    }
    let loaded = match fetch::load(&Offline, cache_dir, location, Policy::Offline, now) {
        Ok(loaded) => loaded,
        Err(e) => {
            log::debug!("pass_shard: no ring — {e:#}");
            return None;
        }
    };
    let snapshot = &loaded.snapshot;
    let doc = match crate::fleet_store::compiled::from_snapshot(snapshot) {
        Ok(Some(doc)) => doc,
        Ok(None) => return None,
        Err(e) => {
            log::warn!("pass_shard: no ring — {e:#}");
            return None;
        }
    };
    let run_state = |h: &str| {
        crate::fleet_store::state::resolve_value(doc.state(), h, "fleet.json `state`")
            .ok()
            .map(|s| s.state)
    };
    Some(ring_from_compiled(
        &doc,
        host,
        &snapshot.manifest.commit,
        snapshot.manifest.confirmed_at,
        &run_state,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pass_shard/tests.rs"]
mod tests;
