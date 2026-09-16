//! Forge-backed role-runner host roster (Issue #7690 Phase A + #7691 Phase B
//! of #6704).
//!
//! This module publishes and expires a roster comment per host, exposes pure
//! `members`/`ring`/`gen` functions over a fixed comment set + instant, feeds
//! `loom-daemon status`'s roster section (Phase A), and implements the
//! **generation-fenced admission rule** ([`admission`]) that
//! [`super::decide`] consumes to derive `(index, count)` from the live roster
//! (Phase B). See `defaults/docs/role-runner-roster.md` for the full design
//! record this implements — that document is the spec; this module is its
//! implementation.
//!
//! Everything here is inert unless `autonomous.roleRunner.roster.enabled` is
//! `true` (default `false`): with the roster off, [`super::decide`] never
//! calls [`admission`] and its verdict is byte-identical to #6374's.
//!
//! ## Record shape
//!
//! One comment per host on a designated **roster issue**, whose literal first
//! line is:
//!
//! ```text
//! <!-- loom:roster host=<opaque-host-id> serves=<digest>,<digest>,... -->
//! ```
//!
//! `host` is [`crate::sweep_registry::opaque_host_id`] of
//! [`crate::sweep_registry::host_identity`] — the same id lease records
//! publish. `serves` is the set of this host's registered workspace shard
//! keys ([`super::hash_key`]), as lowercase hex, sorted ascending. Everything
//! after the marker's closing `-->` is free-form prose; machine readers must
//! never depend on it, only on `.starts_with(ROSTER_MARKER_PREFIX)` — the same
//! contract `defaults/docs/lease-record.md` establishes for the lease marker.
//!
//! ## Membership and generation are PURE functions
//!
//! [`members`], [`ring`], and [`generation`] take a comment set and an
//! instant — no forge, no clock, no I/O — so two hosts (or a test) holding the
//! identical comment set always compute the identical answer. This is the
//! property the fencing rule rests on, and [`admission`] (Phase B) is pure in
//! the same way: the only stateful part of the fence, the
//! generation high-water mark, is passed in as an argument.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use chrono::{DateTime, Duration as ChronoDuration, Utc};

// ============================================================================
// Config
// ============================================================================

/// Env var: opt-in master switch for the roster (`autonomous.roleRunner.roster.enabled`).
pub const ROSTER_ENABLED_ENV: &str = "LOOM_ROLE_RUNNER_ROSTER";
/// Env var: the roster issue, `owner/repo#N` (`autonomous.roleRunner.roster.issue`).
pub const ROSTER_ISSUE_ENV: &str = "LOOM_ROLE_RUNNER_ROSTER_ISSUE";
/// Env var: heartbeat cadence in seconds (`autonomous.roleRunner.roster.heartbeatSecs`).
pub const ROSTER_HEARTBEAT_SECS_ENV: &str = "LOOM_ROLE_RUNNER_ROSTER_HEARTBEAT_SECS";
/// Env var: liveness TTL in seconds (`autonomous.roleRunner.roster.ttlSecs`).
pub const ROSTER_TTL_SECS_ENV: &str = "LOOM_ROLE_RUNNER_ROSTER_TTL_SECS";
/// Env var: settle window in seconds (`autonomous.roleRunner.roster.settleSecs`).
pub const ROSTER_SETTLE_SECS_ENV: &str = "LOOM_ROLE_RUNNER_ROSTER_SETTLE_SECS";

/// Config key (under `autonomous.roleRunner`) for the whole roster block —
/// i.e. the `roster` in `autonomous.roleRunner.roster.*`.
pub const ROSTER_BLOCK_KEY: &str = "roster";
/// Config key (under `autonomous.roleRunner.roster`) for the master switch.
pub const ROSTER_ENABLED_KEY: &str = "enabled";
/// Config key for the roster issue (`owner/repo#N`).
pub const ROSTER_ISSUE_KEY: &str = "issue";
/// Config key for the heartbeat cadence, in seconds.
pub const ROSTER_HEARTBEAT_SECS_KEY: &str = "heartbeatSecs";
/// Config key for the liveness TTL, in seconds.
pub const ROSTER_TTL_SECS_KEY: &str = "ttlSecs";
/// Config key for the settle window, in seconds.
pub const ROSTER_SETTLE_SECS_KEY: &str = "settleSecs";

/// Default heartbeat cadence (#7690's design record).
pub const ROSTER_DEFAULT_HEARTBEAT_SECS: u64 = 300;
/// Default liveness TTL — 3x the default heartbeat.
pub const ROSTER_DEFAULT_TTL_SECS: u64 = 900;
/// Default settle window — one full role-tick interval at the longest
/// built-in cadence.
pub const ROSTER_DEFAULT_SETTLE_SECS: u64 = 900;

/// A parsed `owner/repo#N` roster-issue reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterIssueRef {
    /// The repo owner (user or org).
    pub owner: String,
    /// The repo name.
    pub repo: String,
    /// The issue number.
    pub number: u32,
}

impl RosterIssueRef {
    /// Parse `owner/repo#N`. Rejects an empty owner/repo or a zero/malformed
    /// issue number — there is no sane fallback for a half-specified roster
    /// issue, so an invalid value is treated identically to an absent one
    /// (see [`RosterState::MisconfiguredNoIssue`]).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let (nwo, num) = trimmed.split_once('#')?;
        let (owner, repo) = nwo.split_once('/')?;
        let (owner, repo) = (owner.trim(), repo.trim());
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        let number: u32 = num.trim().parse().ok()?;
        if number == 0 {
            return None;
        }
        Some(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        })
    }

    /// Render back to `owner/repo#N`.
    #[must_use]
    pub fn display(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// Whether the roster is off, misconfigured, or active — mirrors
/// [`super::UnshardedReason`]'s "every variant names the fallback direction"
/// pattern so a misconfiguration is never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterState {
    /// `roster.enabled` is `false` (the default) — no roster, zero extra
    /// forge calls, `status` renders nothing new.
    Disabled,
    /// `roster.enabled` is `true` but no valid `roster.issue` (`owner/repo#N`)
    /// is configured. Per the design record: "one `error!` and no roster" —
    /// this is the state that log call fires for; see
    /// [`crate::role_runner`]'s heartbeat-task spawn.
    MisconfiguredNoIssue,
    /// Fully configured — the roster issue this host publishes to.
    Active(RosterIssueRef),
}

/// Resolved `autonomous.roleRunner.roster.*` configuration for one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterConfig {
    /// Whether/how the roster is configured.
    pub state: RosterState,
    /// Heartbeat refresh cadence, in seconds.
    pub heartbeat_secs: u64,
    /// Liveness TTL, in seconds — floored at 3x [`Self::heartbeat_secs`].
    pub ttl_secs: u64,
    /// Settle window, in seconds.
    pub settle_secs: u64,
}

impl RosterConfig {
    /// This host's roster issue, or `None` when disabled/misconfigured.
    #[must_use]
    pub fn issue(&self) -> Option<&RosterIssueRef> {
        match &self.state {
            RosterState::Active(issue) => Some(issue),
            RosterState::Disabled | RosterState::MisconfiguredNoIssue => None,
        }
    }

    /// Whether the roster is actually active (configured AND has a valid issue).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.state, RosterState::Active(_))
    }
}

fn env_str(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve a boolean knob with env > config > `default` precedence, matching
/// every other `autonomous.*` surface (truthy strings: `1`/`true`/`yes`/`on`,
/// case-insensitive; anything else in the env is falsy).
fn resolve_bool_knob(
    env_name: &str,
    block: Option<&serde_json::Value>,
    key: &str,
    default: bool,
) -> bool {
    if let Some(raw) = env_str(env_name) {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    block
        .and_then(|b| b.get(key))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(default)
}

fn resolve_string_knob(
    env_name: &str,
    block: Option<&serde_json::Value>,
    key: &str,
) -> Option<String> {
    if let Some(raw) = env_str(env_name) {
        return Some(raw);
    }
    block
        .and_then(|b| b.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// Resolve a `u64` knob with env > config precedence. A malformed env value
/// resolves to `None` (falls through to the caller's default) rather than
/// panicking or silently clamping — there is no "wrong but safe" number to
/// guess for a duration.
fn resolve_u64_knob(env_name: &str, block: Option<&serde_json::Value>, key: &str) -> Option<u64> {
    if let Some(raw) = env_str(env_name) {
        return raw.parse::<u64>().ok();
    }
    block
        .and_then(|b| b.get(key))
        .and_then(serde_json::Value::as_u64)
}

/// Resolve this root's roster config from its effective `.loom/config.json`
/// (`autonomous.roleRunner.roster`) plus env overrides.
#[must_use]
pub fn resolve_roster_config(root: &Path) -> RosterConfig {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let block = crate::config_resolver::get_path(&effective, "autonomous.roleRunner.roster");
    resolve_roster_config_from(block)
}

/// The pure core of [`resolve_roster_config`], over an already-resolved
/// `autonomous.roleRunner.roster` block — the seam [`super::decide`] uses (it
/// has already read `autonomous.roleRunner` for the static knobs, so it must
/// not re-resolve the whole config) and the seam tests drive.
#[must_use]
pub fn resolve_roster_config_from(block: Option<&serde_json::Value>) -> RosterConfig {
    let enabled = resolve_bool_knob(ROSTER_ENABLED_ENV, block, ROSTER_ENABLED_KEY, false);

    let heartbeat_secs =
        resolve_u64_knob(ROSTER_HEARTBEAT_SECS_ENV, block, ROSTER_HEARTBEAT_SECS_KEY)
            .filter(|&v| v > 0)
            .unwrap_or(ROSTER_DEFAULT_HEARTBEAT_SECS);
    // Floored at 3x heartbeat (design record) — a TTL any tighter than that
    // would make a single missed forge round-trip look like a death.
    let ttl_floor = heartbeat_secs.saturating_mul(3);
    let ttl_secs = resolve_u64_knob(ROSTER_TTL_SECS_ENV, block, ROSTER_TTL_SECS_KEY)
        .filter(|&v| v > 0)
        .unwrap_or(ROSTER_DEFAULT_TTL_SECS)
        .max(ttl_floor);
    // Floored at `ttl_secs` (Phase B, #7691). The design record's no-overlap
    // argument needs it: a host that stops reading the roster keeps acting
    // under its stale ring until its OWN record expires, i.e. for up to `ttl`
    // after its last read, while every host that observed the new ring resumes
    // `settle` after the boundary. With `settle >= ttl` the stale host has
    // always self-fenced (condition 1) before anyone acts under the new ring;
    // with `settle < ttl` the two windows can overlap, which is exactly the
    // two-owner race the fence exists to rule out. The defaults (900/900)
    // already satisfy it — this floor only stops a hand-tuned `settleSecs`
    // from silently breaking the invariant.
    let settle_secs = resolve_u64_knob(ROSTER_SETTLE_SECS_ENV, block, ROSTER_SETTLE_SECS_KEY)
        .filter(|&v| v > 0)
        .unwrap_or(ROSTER_DEFAULT_SETTLE_SECS)
        .max(ttl_secs);

    let state = if !enabled {
        RosterState::Disabled
    } else {
        match resolve_string_knob(ROSTER_ISSUE_ENV, block, ROSTER_ISSUE_KEY)
            .as_deref()
            .and_then(RosterIssueRef::parse)
        {
            Some(issue) => RosterState::Active(issue),
            None => RosterState::MisconfiguredNoIssue,
        }
    };

    RosterConfig {
        state,
        heartbeat_secs,
        ttl_secs,
        settle_secs,
    }
}

// ============================================================================
// Record shape: marker + parsing
// ============================================================================

/// The literal prefix of a roster comment's first line. Readers must locate a
/// roster record via `.starts_with(ROSTER_MARKER_PREFIX)` and never parse
/// anything past its closing `-->`.
pub const ROSTER_MARKER_PREFIX: &str = "<!-- loom:roster host=";

/// One roster comment, as read back from the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterComment {
    /// The forge comment id (for `PATCH` targeting).
    pub id: u64,
    /// The publishing host's opaque id.
    pub host: String,
    /// This host's registered workspace shard-key digests
    /// ([`super::hash_key`] outputs).
    pub serves: BTreeSet<u64>,
    /// The comment's forge-assigned creation time.
    pub created_at: DateTime<Utc>,
    /// The comment's forge-assigned `updated_at` — the ONLY liveness signal;
    /// never a timestamp embedded in the body (same rule as the lease record).
    pub updated_at: DateTime<Utc>,
}

/// Parse `host=`/`serves=` out of a roster comment body's literal first line.
/// Returns `None` when the line does not match the marker shape, or when
/// `host` is empty. An empty or all-malformed `serves` list parses to an
/// empty set rather than failing — a host that serves nothing yet (freshly
/// joined, no workspaces registered) is still a valid roster member.
#[must_use]
pub fn parse_roster_marker_line(body: &str) -> Option<(String, BTreeSet<u64>)> {
    let first_line = body.lines().next()?;
    let rest = first_line.strip_prefix(ROSTER_MARKER_PREFIX)?;
    let rest = rest.strip_suffix(" -->")?;
    let (host, serves_str) = rest.split_once(" serves=")?;
    if host.is_empty() {
        return None;
    }
    let mut serves = BTreeSet::new();
    for tok in serves_str.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        if let Ok(v) = u64::from_str_radix(tok, 16) {
            serves.insert(v);
        }
    }
    Some((host.to_string(), serves))
}

/// Render the `serves` digest set as sorted, comma-separated lowercase hex —
/// the exact wire shape [`parse_roster_marker_line`] parses back.
#[must_use]
pub fn render_serves(serves: &BTreeSet<u64>) -> String {
    serves
        .iter()
        .map(|d| format!("{d:016x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Build a roster comment's full body for this heartbeat cycle. The body is
/// regenerated wholesale (never edited in place) — since nothing but this
/// function ever writes it, there is no user prose to preserve, so a
/// full-body `PATCH` is both simpler and guarantees the required "the body
/// must change something" property (`defaults/docs/lease-renewal.md`) via its
/// own `at=` timestamp, on every call, even when `serves` is unchanged.
#[must_use]
pub fn build_roster_comment_body(host: &str, serves: &BTreeSet<u64>) -> String {
    format!(
        "{prefix}{host} serves={serves} -->\n\
         This is host `{host}`'s role-runner **roster record** (Issue #6704) — one comment per \
         host on this designated roster issue. Its liveness signal is this comment's own \
         forge-assigned `updated_at`, never a timestamp embedded in this text; its `created_at` \
         is a membership boundary every host computes identically, so this record is **replaced**, \
         not patched, whenever it expires or its `serves` set changes. See \
         `defaults/docs/role-runner-roster.md` for the full format contract. Do not edit or delete \
         this comment by hand — with `autonomous.roleRunner.roster.enabled` it decides which host \
         runs which workspace's role rotation.\n\n\
         <!-- loom:roster-heartbeat at={ts} -->",
        prefix = ROSTER_MARKER_PREFIX,
        host = host,
        serves = render_serves(serves),
        ts = Utc::now().to_rfc3339(),
    )
}

/// Parse the NDJSON `gh api ... --jq` roster-comments read into
/// [`RosterComment`]s — mirrors
/// `SweepRegistry::parse_lease_comments_json`'s per-line, best-effort parse
/// (a malformed line is dropped, not fatal to the batch). A comment missing
/// `created_at`/`updated_at` is also dropped: both are load-bearing for
/// [`members`]/[`generation`], so a partial record carries no usable
/// liveness signal.
#[must_use]
pub fn parse_roster_comments_json(stdout: &[u8]) -> Vec<RosterComment> {
    let raw = String::from_utf8_lossy(stdout);
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(item) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(id) = item.get("id").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Some(body) = item.get("body").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some((host, serves)) = parse_roster_marker_line(body) else {
            continue;
        };
        let created_at = item
            .get("created_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let updated_at = item
            .get("updated_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let (Some(created_at), Some(updated_at)) = (created_at, updated_at) else {
            continue;
        };
        out.push(RosterComment {
            id,
            host,
            serves,
            created_at,
            updated_at,
        });
    }
    out
}

// ============================================================================
// Membership + generation: PURE functions of (comment set, instant)
// ============================================================================

fn ttl_duration(ttl_secs: u64) -> ChronoDuration {
    ChronoDuration::seconds(i64::try_from(ttl_secs).unwrap_or(i64::MAX))
}

/// `members(C, t) = { h : created_at(c_h) <= t AND t < updated_at(c_h) + ttl }`
/// — the design record's membership function, verbatim. Pure: no forge, no
/// clock, no I/O.
#[must_use]
pub fn members(comments: &[RosterComment], now: DateTime<Utc>, ttl_secs: u64) -> BTreeSet<String> {
    let ttl = ttl_duration(ttl_secs);
    comments
        .iter()
        .filter(|c| c.created_at <= now && now < c.updated_at + ttl)
        .map(|c| c.host.clone())
        .collect()
}

/// `ring(C, t, k) = { h in members(C, t) : digest(k) in serves(c_h) }`, sorted
/// by host id ascending (a total, deterministic order every host computes
/// identically). `key_digest` is [`super::hash_key`] of the shard key — the
/// same hash `owning_shard`/`ShardPosture::owns` already use.
#[must_use]
pub fn ring(
    comments: &[RosterComment],
    now: DateTime<Utc>,
    ttl_secs: u64,
    key_digest: u64,
) -> Vec<String> {
    let live = members(comments, now, ttl_secs);
    let mut hosts: Vec<String> = comments
        .iter()
        .filter(|c| live.contains(&c.host) && c.serves.contains(&key_digest))
        .map(|c| c.host.clone())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// `boundaries(C) = { created_at(c_h) } ∪ { updated_at(c_h) + ttl }`,
/// `gen(C, t) = max { b in boundaries(C) : b <= t }` — the most recent
/// membership-boundary instant at or before `t`. `None` for an empty comment
/// set (there are no boundaries yet).
#[must_use]
pub fn generation(
    comments: &[RosterComment],
    now: DateTime<Utc>,
    ttl_secs: u64,
) -> Option<DateTime<Utc>> {
    let ttl = ttl_duration(ttl_secs);
    comments
        .iter()
        .flat_map(|c| [c.created_at, c.updated_at + ttl])
        .filter(|b| *b <= now)
        .max()
}

// ============================================================================
// The admission fence (Issue #7691, Phase B of #6704)
// ============================================================================

/// Why a roster-mode host declined to admit a role tick — one variant per
/// fence condition in the design record, so a yield is never anonymous in the
/// log or in `status`.
///
/// **Every variant means "yield", i.e. run no role tick here.** That is the
/// deliberate inversion of #6374's "when in doubt, duplicate": a brief gap is
/// a periodic idempotent pass running one interval later, while a brief
/// duplicate is two `claude` sessions racing the same forge queue (#6332 /
/// #6352) and is not self-healing. It applies **only** to role ticks — the
/// dispatcher's preferred-slice consumer keeps the pre-roster verdict (see
/// [`super::ShardDecision::owned`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterYield {
    /// Condition 1: this host has no roster comment in the observed set at
    /// all. It joined once (there is a snapshot) but its own record is gone —
    /// it cannot know whether the fleet has evicted it.
    SelfMissing,
    /// Condition 1: this host's own record is older than `ttl`, so the rest
    /// of the fleet has already evicted it (or is about to).
    SelfStale {
        /// Seconds since this host's own last successful heartbeat.
        last_beat_secs: i64,
        /// The TTL it was compared against.
        ttl_secs: u64,
    },
    /// Condition 2: the observed view yields a generation older than the
    /// newest this process has already observed — a stale read (a cached /
    /// ETag-stale response, a lagging replica). Discarded, never acted on.
    StaleGeneration {
        /// The generation this read computed.
        observed: DateTime<Utc>,
        /// The newest generation observed so far in this process.
        newest: DateTime<Utc>,
    },
    /// Condition 3: the current generation has not been quiet for
    /// `settleSecs` yet. A ring that just changed is not actionable by anyone.
    NotSettled {
        /// How long the current generation has been settled, in seconds.
        settled_secs: i64,
        /// How long it must be settled before anyone acts.
        settle_secs: u64,
    },
    /// Condition 3, degenerate case: no membership boundary is computable at
    /// all (an empty comment set, or one whose every boundary is in the
    /// future under clock skew).
    NoGeneration,
    /// Condition 4: this host's own record is younger than `ttl` — a new
    /// member waits a full TTL before acting, so that every peer has observed
    /// the join (and its settle deadline) before the joiner acts under it.
    Joining {
        /// Age of this host's own roster record, in seconds.
        age_secs: i64,
        /// The TTL it must reach.
        ttl_secs: u64,
    },
    /// Condition 5, prerequisite: this host is live and settled but does not
    /// advertise this workspace's key in its own `serves` set, so it is not in
    /// this key's ring at all. Distinct from "in the ring but not the owner",
    /// which is an ordinary [`RosterAdmission::Ring`] verdict.
    NotInRing,
}

impl RosterYield {
    /// A short, stable label for logs and status output.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::SelfMissing => "self-missing",
            Self::SelfStale { .. } => "self-stale",
            Self::StaleGeneration { .. } => "stale-generation",
            Self::NotSettled { .. } => "not-settled",
            Self::NoGeneration => "no-generation",
            Self::Joining { .. } => "joining",
            Self::NotInRing => "not-in-ring",
        }
    }

    /// A one-line human description naming the condition and its numbers.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::SelfMissing => {
                "this host has no record in the observed roster (it cannot know whether the fleet \
                 has evicted it)"
                    .to_string()
            }
            Self::SelfStale {
                last_beat_secs,
                ttl_secs,
            } => format!(
                "this host's own roster record is stale ({last_beat_secs}s since its last \
                 heartbeat, ttl {ttl_secs}s) — the fleet has evicted it"
            ),
            Self::StaleGeneration { observed, newest } => format!(
                "the observed roster generation ({observed}) is older than the newest already \
                 observed ({newest}) — stale read, discarded"
            ),
            Self::NotSettled {
                settled_secs,
                settle_secs,
            } => format!(
                "the current roster generation has been settled only {settled_secs}s of the \
                 required {settle_secs}s — the ring just changed"
            ),
            Self::NoGeneration => {
                "no roster membership boundary is computable from the observed comment set"
                    .to_string()
            }
            Self::Joining { age_secs, ttl_secs } => format!(
                "this host joined the roster {age_secs}s ago and waits a full ttl ({ttl_secs}s) \
                 before acting"
            ),
            Self::NotInRing => {
                "this host does not advertise this workspace in its own roster `serves` set"
                    .to_string()
            }
        }
    }
}

/// The fence's verdict for one (host, workspace key) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterAdmission {
    /// All five conditions hold: this host's rank in the live ring for this
    /// key is `index` of `count`, under the settled generation `generation`.
    /// Ownership itself is still the ordinary `fnv1a64(key) % count == index`
    /// check the caller applies — the ring only supplies the numbers.
    Ring {
        /// This host's ordinal in the ring, sorted by host id.
        index: usize,
        /// The ring's size (live members serving this key).
        count: usize,
        /// The settled membership generation the ring was computed under.
        generation: DateTime<Utc>,
    },
    /// A fence condition failed: yield.
    Yield(RosterYield),
}

impl RosterAdmission {
    /// The generation this verdict was computed under, when it has one.
    #[must_use]
    pub const fn generation(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Ring { generation, .. } => Some(*generation),
            Self::Yield(_) => None,
        }
    }
}

/// This host's own roster record within `comments` — the **freshest** one if a
/// (transient, see [`resolve_publish_action`]) duplicate ever exists, so a
/// leftover record can never make a live host look stale.
#[must_use]
pub fn own_comment<'a>(comments: &'a [RosterComment], host: &str) -> Option<&'a RosterComment> {
    comments
        .iter()
        .filter(|c| c.host == host)
        .max_by_key(|c| c.updated_at)
}

/// The design record's fencing rule, as a **pure** function of the observed
/// comment set, this host's id, the key digest, the instant, the two windows,
/// and the newest generation this process has already observed.
///
/// Conditions are evaluated in the design record's own order — self-liveness,
/// generation monotonicity, settle, join fence, then ring membership — so the
/// reported [`RosterYield`] always names the *first* condition that failed,
/// which is the one an operator needs to act on.
///
/// `newest_observed_generation` is the only piece of state the fence has, and
/// it is an argument rather than a global read precisely so every adversarial
/// scenario (split view, kill-host, self-fence, join fence) is testable
/// without a forge, a clock, or a daemon.
#[must_use]
pub fn admission(
    comments: &[RosterComment],
    this_host: &str,
    key_digest: u64,
    now: DateTime<Utc>,
    ttl_secs: u64,
    settle_secs: u64,
    newest_observed_generation: Option<DateTime<Utc>>,
) -> RosterAdmission {
    let ttl = ttl_duration(ttl_secs);

    // Condition 1: self-liveness. A host that cannot see a fresh record of its
    // own cannot know whether the fleet has evicted it, so it yields until its
    // next successful heartbeat. This is also what makes "total forge
    // unavailability" resolve to "nobody ticks anywhere" rather than
    // "everybody ticks everywhere": the read that locates this host's own
    // comment IS the roster read.
    let Some(own) = own_comment(comments, this_host) else {
        return RosterAdmission::Yield(RosterYield::SelfMissing);
    };
    if now >= own.updated_at + ttl {
        return RosterAdmission::Yield(RosterYield::SelfStale {
            last_beat_secs: (now - own.updated_at).num_seconds(),
            ttl_secs,
        });
    }

    // Condition 2: generation monotonicity. Never act under a view older than
    // the newest already observed in this process.
    let Some(gen) = generation(comments, now, ttl_secs) else {
        return RosterAdmission::Yield(RosterYield::NoGeneration);
    };
    if let Some(newest) = newest_observed_generation {
        if gen < newest {
            return RosterAdmission::Yield(RosterYield::StaleGeneration {
                observed: gen,
                newest,
            });
        }
    }

    // Condition 3: settle. Because `gen` is a forge-assigned instant, every
    // host that has observed this membership computes the SAME deadline
    // (`gen + settle`) regardless of when it read — that shared absolute
    // instant, not the read cadence, is what makes the switchover atomic.
    let settled_secs = (now - gen).num_seconds();
    if settled_secs < i64::try_from(settle_secs).unwrap_or(i64::MAX) {
        return RosterAdmission::Yield(RosterYield::NotSettled {
            settled_secs,
            settle_secs,
        });
    }

    // Condition 4: join fence. A new member waits a full ttl, by which time
    // every live peer has necessarily re-read the roster (heartbeat cadence is
    // ttl/3 or tighter) and is fenced by the same settle deadline.
    let age_secs = (now - own.created_at).num_seconds();
    if now < own.created_at + ttl {
        return RosterAdmission::Yield(RosterYield::Joining { age_secs, ttl_secs });
    }

    // Condition 5's prerequisite: rank within this key's ring. The ownership
    // test itself (`fnv1a64(key) % count == index`) stays in `ShardPosture`.
    let ring = ring(comments, now, ttl_secs, key_digest);
    let Some(index) = ring.iter().position(|h| h == this_host) else {
        return RosterAdmission::Yield(RosterYield::NotInRing);
    };
    RosterAdmission::Ring {
        index,
        count: ring.len(),
        generation: gen,
    }
}

// ----------------------------------------------------------------------------
// Generation high-water mark (condition 2's only state)
// ----------------------------------------------------------------------------

/// The newest generation this process has observed, plus the comment ids it
/// was derived from.
#[derive(Debug, Clone)]
struct GenerationFence {
    ids: BTreeSet<u64>,
    newest: DateTime<Utc>,
}

fn generation_fence_cell() -> &'static Mutex<Option<GenerationFence>> {
    static CELL: OnceLock<Mutex<Option<GenerationFence>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Record `raw_gen` (computed from the comment set `ids`) as observed, and
/// return the newest generation observed so far — the value condition 2
/// compares against.
///
/// **The ratchet resets when a previously observed comment disappears.** A
/// vanished record (an operator tidying the roster issue, or a host
/// republishing after eviction — see [`resolve_publish_action`]) invalidates
/// the boundaries the high-water mark was derived from, and without the reset
/// the host would yield **forever** against a generation no live data can ever
/// reach again. Resetting is safe because the settle check still gates on the
/// forge-assigned `gen`, which every host computes identically from the same
/// comment set, so a reset cannot make two hosts act under different rings.
pub fn observe_generation(ids: &BTreeSet<u64>, raw_gen: DateTime<Utc>) -> DateTime<Utc> {
    let mut cell = generation_fence_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let newest = match cell.as_ref() {
        Some(prev) if prev.ids.is_subset(ids) => prev.newest.max(raw_gen),
        // First observation, or the comment set shrank — take the fresh
        // reading as authoritative.
        _ => raw_gen,
    };
    *cell = Some(GenerationFence {
        ids: ids.clone(),
        newest,
    });
    newest
}

/// Clear the generation high-water mark. Test seam only.
#[cfg(test)]
pub(crate) fn clear_generation_fence_for_tests() {
    *generation_fence_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
}

/// Evaluate [`admission`] against a published [`RosterSnapshot`], folding the
/// process-global generation high-water mark in. This is the seam
/// [`super::decide`] calls; [`admission`] itself stays pure.
///
/// The snapshot's own `ttl_secs`/`settle_secs` are used rather than a
/// re-resolved config, so the fence and `status` always describe the same
/// read with the same windows.
#[must_use]
pub fn admit(snapshot: &RosterSnapshot, key_digest: u64, now: DateTime<Utc>) -> RosterAdmission {
    let ids: BTreeSet<u64> = snapshot.comments.iter().map(|c| c.id).collect();
    let newest = generation(&snapshot.comments, now, snapshot.ttl_secs)
        .map(|raw| observe_generation(&ids, raw));
    admission(
        &snapshot.comments,
        &snapshot.host,
        key_digest,
        now,
        snapshot.ttl_secs,
        snapshot.settle_secs,
        newest,
    )
}

// ----------------------------------------------------------------------------
// Write-side: how this host publishes its own record
// ----------------------------------------------------------------------------

/// What the heartbeat should do with this host's roster record this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterPublish {
    /// No record exists yet — `POST` a new comment (this host's first
    /// heartbeat on this issue).
    Create,
    /// A live record exists whose `serves` set is unchanged — `PATCH` it in
    /// place, which advances `updated_at` without moving any boundary.
    Patch {
        /// The comment id to `PATCH`.
        id: u64,
    },
    /// The record must be **replaced** (delete, then `POST` a fresh one) so
    /// that its `created_at` becomes a new, forge-assigned membership
    /// boundary that every host observes identically.
    Republish {
        /// The stale comment id to delete.
        id: u64,
        /// Why (for the log line).
        reason: RosterRepublishReason,
    },
}

/// Why this host is replacing its roster record rather than patching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterRepublishReason {
    /// This host's record had expired — the fleet evicted it, so coming back
    /// is a **join**, and a join needs a fresh `created_at` for the
    /// generation fence (and the join fence) to see it. Patching in place
    /// would resurrect the host into every peer's ring with no boundary and
    /// no settle window, i.e. exactly the unfenced membership change the
    /// design record rules out.
    Expired,
    /// This host's `serves` set changed (a workspace was registered,
    /// unregistered, or had its role runner toggled), which changes the ring
    /// for those keys. Same argument: a ring change must coincide with a
    /// boundary, or peers switch rings at different instants.
    ServesChanged,
}

impl RosterRepublishReason {
    /// A short, stable label for logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Expired => "record expired (rejoin)",
            Self::ServesChanged => "serves set changed",
        }
    }
}

/// Decide how this host publishes its record this cycle, given the record it
/// found (if any), the `serves` set it is about to advertise, and the
/// liveness TTL. Pure — the forge calls live in [`crate::role_runner`].
#[must_use]
pub fn resolve_publish_action(
    existing: Option<&RosterComment>,
    serves: &BTreeSet<u64>,
    now: DateTime<Utc>,
    ttl_secs: u64,
) -> RosterPublish {
    let Some(existing) = existing else {
        return RosterPublish::Create;
    };
    if now >= existing.updated_at + ttl_duration(ttl_secs) {
        return RosterPublish::Republish {
            id: existing.id,
            reason: RosterRepublishReason::Expired,
        };
    }
    if existing.serves != *serves {
        return RosterPublish::Republish {
            id: existing.id,
            reason: RosterRepublishReason::ServesChanged,
        };
    }
    RosterPublish::Patch { id: existing.id }
}

// ============================================================================
// Status rendering (AC4)
// ============================================================================

/// One roster member's row for `status` — mirrors the design record's sample
/// output line (`host-a3f9c1d2  fresh   (last beat 41s ago)   serves 27`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterMemberView {
    /// The member's opaque host id.
    pub host: String,
    /// Whether this member is currently live (in [`members`]'s result).
    pub fresh: bool,
    /// Seconds since this member's last heartbeat, floored at 0 (a comment
    /// whose `updated_at` is in the future relative to `now`, e.g. from clock
    /// skew, reports 0 rather than a negative age).
    pub last_beat_secs_ago: i64,
    /// How many workspace shard keys this member currently serves.
    pub serves_count: usize,
    /// Whether this row is the host `status` is running on.
    pub is_this_host: bool,
}

/// The roster section for `status`/`--json` — the issue, live/seen counts,
/// the current generation and how long it has been settled, and one row per
/// member (sorted by host id for a stable diff between two hosts' output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterStatusView {
    /// `owner/repo#N` of the roster issue.
    pub issue: String,
    /// How many members are currently live ([`members`]'s count).
    pub live_count: usize,
    /// How many roster comments exist at all, live or expired.
    pub seen_count: usize,
    /// [`generation`]'s result, `None` for an empty comment set.
    pub generation: Option<DateTime<Utc>>,
    /// Seconds since [`Self::generation`], floored at 0. `None` alongside
    /// `generation: None`.
    pub settled_secs: Option<i64>,
    /// Per-member rows, sorted by host id.
    pub members: Vec<RosterMemberView>,
}

/// Build the pure [`RosterStatusView`] from an already-fetched comment set —
/// no I/O. `this_host` is this process's own opaque id (for the `← this
/// host` marker); it need not currently be a member (the row for it will
/// simply say `fresh: false` / be absent if it has never published).
#[must_use]
pub fn build_roster_status(
    issue: &RosterIssueRef,
    comments: &[RosterComment],
    this_host: &str,
    now: DateTime<Utc>,
    ttl_secs: u64,
) -> RosterStatusView {
    let live = members(comments, now, ttl_secs);
    let gen = generation(comments, now, ttl_secs);
    let settled_secs = gen.map(|g| (now - g).num_seconds().max(0));
    let mut members: Vec<RosterMemberView> = comments
        .iter()
        .map(|c| RosterMemberView {
            host: c.host.clone(),
            fresh: live.contains(&c.host),
            last_beat_secs_ago: (now - c.updated_at).num_seconds().max(0),
            serves_count: c.serves.len(),
            is_this_host: c.host == this_host,
        })
        .collect();
    members.sort_by(|a, b| a.host.cmp(&b.host));
    RosterStatusView {
        issue: issue.display(),
        live_count: live.len(),
        seen_count: comments.len(),
        generation: gen,
        settled_secs,
        members,
    }
}

// ============================================================================
// Process-global snapshot cache — populated by the heartbeat task
// (`crate::role_runner`), read (no I/O) by `status`.
// ============================================================================

/// The most recently fetched roster read, cached so `status` never triggers
/// its own forge call — it only ever reads what the heartbeat task already
/// fetched. `None` until the first successful heartbeat cycle (or forever,
/// when the roster is disabled — the zero-extra-forge-calls case).
#[derive(Debug, Clone)]
pub struct RosterSnapshot {
    /// The roster issue this snapshot was read from.
    pub issue: RosterIssueRef,
    /// This host's own opaque id, resolved once per cycle.
    pub host: String,
    /// Every roster comment observed on the last successful read.
    pub comments: Vec<RosterComment>,
    /// This root's resolved liveness TTL, in seconds — cached alongside the
    /// comments so `status` renders with the config that produced this
    /// snapshot rather than the config at read time.
    pub ttl_secs: u64,
    /// This root's resolved settle window, in seconds.
    pub settle_secs: u64,
    /// When this snapshot was fetched (this host's own clock).
    pub fetched_at: DateTime<Utc>,
}

fn roster_snapshot_cell() -> &'static Mutex<Option<RosterSnapshot>> {
    static CELL: OnceLock<Mutex<Option<RosterSnapshot>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Publish a fresh snapshot (called by the heartbeat task after a successful
/// read-back).
pub fn set_roster_snapshot(snapshot: RosterSnapshot) {
    *roster_snapshot_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
}

/// Read the last published snapshot, if any. Pure cache read — no I/O.
#[must_use]
pub fn roster_snapshot() -> Option<RosterSnapshot> {
    roster_snapshot_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Clear the snapshot cache. Test seam only.
#[cfg(test)]
pub(crate) fn clear_roster_snapshot_for_tests() {
    *roster_snapshot_cell()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn comment(host: &str, serves: &[u64], created: &str, updated: &str) -> RosterComment {
        RosterComment {
            id: 1,
            host: host.to_string(),
            serves: serves.iter().copied().collect(),
            created_at: dt(created),
            updated_at: dt(updated),
        }
    }

    fn comment_id(
        id: u64,
        host: &str,
        serves: &[u64],
        created: &str,
        updated: &str,
    ) -> RosterComment {
        RosterComment {
            id,
            ..comment(host, serves, created, updated)
        }
    }

    // ---- RosterIssueRef ----

    #[test]
    fn parses_a_well_formed_issue_ref() {
        let r = RosterIssueRef::parse("rjwalters/loom#1234").expect("parses");
        assert_eq!(r.owner, "rjwalters");
        assert_eq!(r.repo, "loom");
        assert_eq!(r.number, 1234);
        assert_eq!(r.display(), "rjwalters/loom#1234");
    }

    #[test]
    fn rejects_malformed_issue_refs() {
        assert!(RosterIssueRef::parse("").is_none());
        assert!(RosterIssueRef::parse("loom#123").is_none());
        assert!(RosterIssueRef::parse("rjwalters/loom").is_none());
        assert!(RosterIssueRef::parse("rjwalters/loom#0").is_none());
        assert!(RosterIssueRef::parse("rjwalters/loom#abc").is_none());
        assert!(RosterIssueRef::parse("/loom#123").is_none());
        assert!(RosterIssueRef::parse("rjwalters/#123").is_none());
    }

    // ---- Config resolution ----

    #[test]
    #[serial]
    fn disabled_by_default() {
        let _e = EnvGuard::capture();
        let config = resolve_roster_config_from(None);
        assert_eq!(config.state, RosterState::Disabled);
        assert!(!config.is_active());
        assert_eq!(config.heartbeat_secs, ROSTER_DEFAULT_HEARTBEAT_SECS);
        assert_eq!(config.ttl_secs, ROSTER_DEFAULT_TTL_SECS);
        assert_eq!(config.settle_secs, ROSTER_DEFAULT_SETTLE_SECS);
    }

    #[test]
    #[serial]
    fn enabled_with_no_issue_is_misconfigured_not_silently_unsharded() {
        let _e = EnvGuard::capture();
        let block = serde_json::json!({ "enabled": true });
        let config = resolve_roster_config_from(Some(&block));
        assert_eq!(config.state, RosterState::MisconfiguredNoIssue);
        assert!(!config.is_active());
        assert!(config.issue().is_none());
    }

    #[test]
    #[serial]
    fn enabled_with_a_valid_issue_is_active() {
        let _e = EnvGuard::capture();
        let block = serde_json::json!({ "enabled": true, "issue": "rjwalters/loom#42" });
        let config = resolve_roster_config_from(Some(&block));
        assert!(config.is_active());
        assert_eq!(config.issue().unwrap().display(), "rjwalters/loom#42");
    }

    #[test]
    #[serial]
    fn ttl_is_floored_at_3x_heartbeat() {
        let _e = EnvGuard::capture();
        let block = serde_json::json!({
            "enabled": true,
            "issue": "rjwalters/loom#42",
            "heartbeatSecs": 100,
            "ttlSecs": 120,
        });
        let config = resolve_roster_config_from(Some(&block));
        assert_eq!(config.heartbeat_secs, 100);
        // ttlSecs=120 is below the 3x100=300 floor, so the floor wins.
        assert_eq!(config.ttl_secs, 300);
    }

    #[test]
    #[serial]
    fn an_explicit_ttl_above_the_floor_is_kept() {
        let _e = EnvGuard::capture();
        let block = serde_json::json!({
            "enabled": true,
            "issue": "rjwalters/loom#42",
            "heartbeatSecs": 100,
            "ttlSecs": 1000,
        });
        let config = resolve_roster_config_from(Some(&block));
        assert_eq!(config.ttl_secs, 1000);
    }

    #[test]
    #[serial]
    fn env_overrides_config_for_every_knob() {
        let _e = EnvGuard::capture();
        let block = serde_json::json!({
            "enabled": false,
            "issue": "someone/else#1",
            "heartbeatSecs": 999,
        });
        std::env::set_var(ROSTER_ENABLED_ENV, "1");
        std::env::set_var(ROSTER_ISSUE_ENV, "rjwalters/loom#42");
        std::env::set_var(ROSTER_HEARTBEAT_SECS_ENV, "60");
        let config = resolve_roster_config_from(Some(&block));
        assert!(config.is_active());
        assert_eq!(config.issue().unwrap().display(), "rjwalters/loom#42");
        assert_eq!(config.heartbeat_secs, 60);
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            let names = [
                ROSTER_ENABLED_ENV,
                ROSTER_ISSUE_ENV,
                ROSTER_HEARTBEAT_SECS_ENV,
                ROSTER_TTL_SECS_ENV,
                ROSTER_SETTLE_SECS_ENV,
            ];
            let saved = names.iter().map(|n| (*n, std::env::var(*n).ok())).collect();
            for n in names {
                std::env::remove_var(n);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    // ---- Marker parsing ----

    #[test]
    fn parses_a_well_formed_marker_line() {
        let body = "<!-- loom:roster host=host-abc123 serves=1a,2b,03 -->\nsome prose";
        let (host, serves) = parse_roster_marker_line(body).expect("parses");
        assert_eq!(host, "host-abc123");
        assert_eq!(serves, [0x1a, 0x2b, 0x03].into_iter().collect());
    }

    #[test]
    fn a_host_with_no_served_workspaces_parses_to_an_empty_set() {
        let body = "<!-- loom:roster host=host-abc123 serves= -->";
        let (host, serves) = parse_roster_marker_line(body).expect("parses");
        assert_eq!(host, "host-abc123");
        assert!(serves.is_empty());
    }

    #[test]
    fn rejects_bodies_that_do_not_match_the_marker_shape() {
        assert!(parse_roster_marker_line("not a marker at all").is_none());
        assert!(parse_roster_marker_line("<!-- loom:lease host=x sweep=y -->").is_none());
        assert!(parse_roster_marker_line("<!-- loom:roster host= serves=1a -->").is_none());
    }

    #[test]
    fn render_serves_round_trips_through_parse() {
        let serves: BTreeSet<u64> = [1, 2, 0xdead_beef].into_iter().collect();
        let rendered = render_serves(&serves);
        let body = format!("{}h serves={} -->", ROSTER_MARKER_PREFIX, rendered);
        let (host, parsed) = parse_roster_marker_line(&body).expect("parses");
        assert_eq!(host, "h");
        assert_eq!(parsed, serves);
    }

    #[test]
    fn build_roster_comment_body_round_trips_its_own_marker_line() {
        let serves: BTreeSet<u64> = [1, 2, 3].into_iter().collect();
        let body = build_roster_comment_body("host-abc", &serves);
        let (host, parsed) = parse_roster_marker_line(&body).expect("parses");
        assert_eq!(host, "host-abc");
        assert_eq!(parsed, serves);
    }

    #[test]
    fn build_roster_comment_body_changes_on_every_call() {
        // Even with identical `serves`, the trailing `at=` timestamp must
        // differ so a PATCH of this body always advances `updated_at`
        // (defaults/docs/lease-renewal.md's "must change something" rule).
        let serves: BTreeSet<u64> = [1].into_iter().collect();
        let a = build_roster_comment_body("host-abc", &serves);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = build_roster_comment_body("host-abc", &serves);
        assert_ne!(a, b);
    }

    // ---- NDJSON parsing ----

    #[test]
    fn parses_multiple_ndjson_lines_and_drops_malformed_ones() {
        let stdout = format!(
            "{{\"id\":1,\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:05:00Z\",\"body\":\"{p}host-a serves=1a -->\"}}\n\
             not json\n\
             {{\"id\":2,\"body\":\"no timestamps\"}}\n\
             {{\"id\":3,\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:05:00Z\",\"body\":\"unrelated comment\"}}\n",
            p = ROSTER_MARKER_PREFIX
        );
        let parsed = parse_roster_comments_json(stdout.as_bytes());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, 1);
        assert_eq!(parsed[0].host, "host-a");
    }

    // ---- members / ring / generation: AC "just-expired and just-joined" fixtures ----

    fn fixture() -> Vec<RosterComment> {
        vec![
            // A: alive at t=00:25:00 -- its last beat (00:20:00) is recent
            // enough that its boundary (00:20:00 + 15m = 00:35:00) is still
            // in the future.
            comment("host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T00:20:00Z"),
            // B: JUST EXPIRED at t=00:25:00 with ttl=900s (15m): its last beat
            // was at 00:10:00, so it expires at 00:25:00 exactly.
            comment("host-b", &[1], "2026-01-01T00:00:00Z", "2026-01-01T00:10:00Z"),
            // C: JUST JOINED at t=00:25:00 (created_at == now).
            comment("host-c", &[1], "2026-01-01T00:25:00Z", "2026-01-01T00:25:00Z"),
        ]
    }

    #[test]
    fn members_excludes_a_just_expired_host_and_includes_a_just_joined_one() {
        let comments = fixture();
        let ttl_secs = 900; // 15 minutes
        let now = dt("2026-01-01T00:25:00Z");
        let live = members(&comments, now, ttl_secs);
        // host-a is still within ttl of its last beat (00:20:00), well
        // before its boundary at 00:35:00.
        assert!(live.contains("host-a"));
        // host-b's boundary (updated_at + ttl = 00:10:00 + 15m = 00:25:00) is
        // NOT after `now` (`t < boundary` is false at equality) -- expired.
        assert!(!live.contains("host-b"), "host-b must be expired at its own boundary instant");
        // host-c's created_at == now, so `created_at <= t` holds -- a
        // just-joined host is a member from its very first instant.
        assert!(live.contains("host-c"));
    }

    #[test]
    fn a_host_created_in_the_future_is_not_yet_a_member() {
        let comments = vec![comment(
            "host-future",
            &[1],
            "2026-01-01T01:00:00Z",
            "2026-01-01T01:00:00Z",
        )];
        let now = dt("2026-01-01T00:00:00Z");
        assert!(members(&comments, now, 900).is_empty());
    }

    #[test]
    fn ring_only_includes_live_members_serving_the_key() {
        let mut comments = fixture();
        // host-d is live but does not serve key digest 1.
        comments.push(comment("host-d", &[2], "2026-01-01T00:00:00Z", "2026-01-01T00:24:00Z"));
        let now = dt("2026-01-01T00:25:00Z");
        let r = ring(&comments, now, 900, 1);
        assert_eq!(r, vec!["host-a".to_string(), "host-c".to_string()]);
    }

    #[test]
    fn ring_is_sorted_and_deterministic() {
        let comments = fixture();
        let now = dt("2026-01-01T00:25:00Z");
        let r1 = ring(&comments, now, 900, 1);
        let r2 = ring(&comments, now, 900, 1);
        assert_eq!(r1, r2);
        let mut sorted = r1.clone();
        sorted.sort();
        assert_eq!(r1, sorted);
    }

    #[test]
    fn generation_is_the_latest_boundary_at_or_before_now() {
        let comments = fixture();
        // Boundaries: host-a: created 00:00, expires 00:20+15m=00:35.
        // host-b: created 00:00, expires 00:10+15m=00:25. host-c: created
        // 00:25, expires 00:40. At t=00:26:00, the boundaries at or before
        // `now` are {00:00 (a/b create), 00:25 (b's expiry), 00:25 (c's
        // create)} -- the latest is 00:25.
        let now = dt("2026-01-01T00:26:00Z");
        let gen = generation(&comments, now, 900).expect("some boundary exists");
        assert_eq!(gen, dt("2026-01-01T00:25:00Z"));
    }

    #[test]
    fn generation_is_none_for_an_empty_comment_set() {
        assert_eq!(generation(&[], Utc::now(), 900), None);
    }

    // ---- Determinism across "hosts" (AC: identical gen + ring from identical input) ----

    #[test]
    fn two_hosts_with_the_identical_comment_set_compute_the_identical_generation_and_ring() {
        let comments = fixture();
        let now = dt("2026-01-01T00:30:00Z");
        // Simulate two independent hosts by calling the pure functions twice
        // against clones of the identical input.
        let host_a_view =
            (generation(&comments.clone(), now, 900), ring(&comments.clone(), now, 900, 1));
        let host_b_view = (generation(&comments, now, 900), ring(&comments, now, 900, 1));
        assert_eq!(host_a_view, host_b_view);
    }

    // ---- Status view ----

    #[test]
    fn build_roster_status_reports_live_seen_generation_and_members() {
        let issue = RosterIssueRef::parse("rjwalters/loom#1234").unwrap();
        let comments = fixture();
        let now = dt("2026-01-01T00:25:00Z");
        let status = build_roster_status(&issue, &comments, "host-a", now, 900);
        assert_eq!(status.issue, "rjwalters/loom#1234");
        assert_eq!(status.seen_count, 3);
        assert_eq!(status.live_count, 2); // host-a, host-c (host-b just expired)
        assert!(status.generation.is_some());
        assert!(status.settled_secs.unwrap() >= 0);
        assert_eq!(status.members.len(), 3);
        let a = status.members.iter().find(|m| m.host == "host-a").unwrap();
        assert!(a.fresh);
        assert!(a.is_this_host);
        let b = status.members.iter().find(|m| m.host == "host-b").unwrap();
        assert!(!b.fresh, "an expired member must stay visible but marked non-fresh");
        assert!(!b.is_this_host);
    }

    #[test]
    fn build_roster_status_on_an_empty_roster_reports_zero_and_no_generation() {
        let issue = RosterIssueRef::parse("rjwalters/loom#1234").unwrap();
        let status = build_roster_status(&issue, &[], "host-a", Utc::now(), 900);
        assert_eq!(status.live_count, 0);
        assert_eq!(status.seen_count, 0);
        assert!(status.generation.is_none());
        assert!(status.settled_secs.is_none());
        assert!(status.members.is_empty());
    }

    // ---- The admission fence (Issue #7691, Phase B of #6704) ----
    //
    // One test per condition, each driving the condition it names to failure
    // while every OTHER condition passes -- so a test that goes green for the
    // wrong reason (e.g. a yield that was really a settle failure) fails
    // instead.

    /// A settled, long-lived three-host roster serving key digest `1`, all
    /// created at 00:00 (an hours-old, long-settled membership) and all still
    /// beating — their last heartbeat is 60s before `now`, as a live fleet's
    /// would be at any instant. Conditions 1/3/4 therefore all pass, so each
    /// test below can break exactly one of them and nothing else.
    const NOW: &str = "2026-01-01T10:00:00Z";
    const FLEET_CREATED: &str = "2026-01-01T00:00:00Z";

    fn settled_fleet_at(now: DateTime<Utc>) -> Vec<RosterComment> {
        ["host-a", "host-b", "host-c"]
            .iter()
            .enumerate()
            .map(|(i, host)| RosterComment {
                id: u64::try_from(i).unwrap() + 1,
                host: (*host).to_string(),
                serves: [1].into_iter().collect(),
                created_at: dt(FLEET_CREATED),
                updated_at: now - ChronoDuration::seconds(60),
            })
            .collect()
    }

    #[test]
    fn a_settled_fleet_admits_every_member_with_its_ring_rank() {
        let now = dt(NOW);
        let comments = settled_fleet_at(now);
        for (expected_index, host) in ["host-a", "host-b", "host-c"].iter().enumerate() {
            match admission(&comments, host, 1, now, 900, 900, None) {
                RosterAdmission::Ring {
                    index,
                    count,
                    generation,
                } => {
                    assert_eq!(index, expected_index, "{host} ranked wrong in the ring");
                    assert_eq!(count, 3);
                    assert_eq!(generation, dt(FLEET_CREATED));
                }
                other => panic!("{host} must be admitted by a settled roster, got {other:?}"),
            }
        }
    }

    #[test]
    fn condition_1_a_host_whose_own_heartbeat_is_stale_yields() {
        // host-a stopped beating 20m ago (> ttl 15m) — e.g. its forge reads
        // are failing, so it cannot know whether the fleet has evicted it (it
        // has). Every other condition still passes for it, so the yield can
        // only come from self-liveness.
        let now = dt(NOW);
        let mut comments = settled_fleet_at(now);
        comments[0].updated_at = now - ChronoDuration::seconds(1200);
        let verdict = admission(&comments, "host-a", 1, now, 900, 900, None);
        assert!(
            matches!(verdict, RosterAdmission::Yield(RosterYield::SelfStale { .. })),
            "a host the fleet has evicted must run NO roster-gated role ticks, got {verdict:?}"
        );
        // Control: with a fresh beat, the identical call is admitted — so the
        // yield above is self-liveness and nothing else.
        assert!(matches!(
            admission(&settled_fleet_at(now), "host-a", 1, now, 900, 900, None),
            RosterAdmission::Ring { .. }
        ));
    }

    #[test]
    fn condition_1_a_host_with_no_record_at_all_yields() {
        let verdict = admission(&settled_fleet_at(dt(NOW)), "host-z", 1, dt(NOW), 900, 900, None);
        assert_eq!(verdict, RosterAdmission::Yield(RosterYield::SelfMissing));
    }

    #[test]
    fn condition_2_a_view_older_than_the_newest_observed_generation_is_discarded() {
        let now = dt(NOW);
        let comments = settled_fleet_at(now);
        // The process has already observed a NEWER generation than this view
        // can produce (gen here is 00:00:00) -- a stale read, e.g. an
        // ETag-cached response or a lagging replica.
        let newest = Some(dt("2026-01-01T05:00:00Z"));
        let verdict = admission(&comments, "host-a", 1, now, 900, 900, newest);
        assert!(
            matches!(verdict, RosterAdmission::Yield(RosterYield::StaleGeneration { .. })),
            "an older view must be discarded, not acted on: {verdict:?}"
        );
        // The identical call with no prior observation is admitted, proving
        // the yield above came from monotonicity and nothing else.
        assert!(matches!(
            admission(&comments, "host-a", 1, now, 900, 900, None),
            RosterAdmission::Ring { .. }
        ));
    }

    /// A live four-host view at `now`, where host-d joined at `join` — the
    /// incumbents keep beating, so only the join boundary distinguishes the
    /// instants under test.
    fn fleet_with_joiner_at(now: DateTime<Utc>, join: DateTime<Utc>) -> Vec<RosterComment> {
        let mut comments = settled_fleet_at(now);
        comments.push(RosterComment {
            id: 4,
            host: "host-d".to_string(),
            serves: [1].into_iter().collect(),
            created_at: join,
            updated_at: now - ChronoDuration::seconds(60),
        });
        comments
    }

    #[test]
    fn condition_3_a_ring_that_just_changed_is_not_actionable_until_it_settles() {
        // host-d joins 60s before `now`: a fresh membership boundary, so
        // NOBODY acts under either ring for settle (900s). The gap is the
        // deliberate cost of never overlapping.
        let now = dt(NOW);
        let join = now - ChronoDuration::seconds(60);
        for host in ["host-a", "host-b", "host-c", "host-d"] {
            let verdict = admission(&fleet_with_joiner_at(now, join), host, 1, now, 900, 900, None);
            assert!(
                matches!(verdict, RosterAdmission::Yield(_)),
                "{host} acted under a ring that changed 60s ago: {verdict:?}"
            );
        }
        // At the settle deadline — an ABSOLUTE instant (`gen + settle`) that
        // every host computes identically from the same forge timestamps,
        // regardless of when each of them read — the whole fleet resumes
        // together under the new 4-ring. That simultaneity is the property
        // the fence exists to provide.
        let settled = join + ChronoDuration::seconds(900);
        for host in ["host-a", "host-b", "host-c", "host-d"] {
            assert!(
                matches!(
                    admission(
                        &fleet_with_joiner_at(settled, join),
                        host,
                        1,
                        settled,
                        900,
                        900,
                        None
                    ),
                    RosterAdmission::Ring { count: 4, .. }
                ),
                "{host} must resume once the new ring has settled"
            );
        }
        // One second earlier, nobody has resumed.
        let just_before = settled - ChronoDuration::seconds(1);
        for host in ["host-a", "host-b", "host-c", "host-d"] {
            assert!(matches!(
                admission(
                    &fleet_with_joiner_at(just_before, join),
                    host,
                    1,
                    just_before,
                    900,
                    900,
                    None
                ),
                RosterAdmission::Yield(_)
            ));
        }
    }

    #[test]
    fn condition_4_a_newly_joined_host_waits_a_full_ttl_before_acting() {
        // Isolating the join fence needs settle < ttl: at the production
        // floor (settle >= ttl, see `settle_secs_is_floored_at_ttl_secs`) the
        // settle window already covers the whole join fence, so condition 3
        // would mask condition 4 and this test would prove nothing.
        let now = dt(NOW);
        let join = now - ChronoDuration::seconds(300);
        let comments = fleet_with_joiner_at(now, join);
        // gen == join, settled for 300s >= settle(60): condition 3 passes...
        for host in ["host-a", "host-b", "host-c"] {
            assert!(
                matches!(
                    admission(&comments, host, 1, now, 900, 60, None),
                    RosterAdmission::Ring { count: 4, .. }
                ),
                "{host} (an incumbent) must be admitted once the join has settled"
            );
        }
        // ...but the joiner itself is still held out: its own record is only
        // 300s old, not a full ttl (900s).
        assert!(
            matches!(
                admission(&comments, "host-d", 1, now, 900, 60, None),
                RosterAdmission::Yield(RosterYield::Joining {
                    age_secs: 300,
                    ttl_secs: 900
                })
            ),
            "a joiner must not act until its own record is a full ttl old"
        );
        // At exactly `created_at + ttl` the join fence lifts.
        let at_ttl = join + ChronoDuration::seconds(900);
        assert!(matches!(
            admission(&fleet_with_joiner_at(at_ttl, join), "host-d", 1, at_ttl, 900, 60, None),
            RosterAdmission::Ring { count: 4, .. }
        ));
        // And under the production floor (settle == ttl) the joiner is fenced
        // for at least as long — the floor can only ever hold it out longer.
        assert!(matches!(
            admission(&comments, "host-d", 1, now, 900, 900, None),
            RosterAdmission::Yield(_)
        ));
    }

    #[test]
    fn condition_5_a_host_that_does_not_serve_the_key_is_not_in_its_ring() {
        let comments = settled_fleet_at(dt(NOW));
        // Nobody serves key digest 99.
        assert_eq!(
            admission(&comments, "host-a", 99, dt(NOW), 900, 900, None),
            RosterAdmission::Yield(RosterYield::NotInRing)
        );
    }

    #[test]
    fn every_yield_reason_labels_and_describes_itself() {
        let reasons = [
            RosterYield::SelfMissing,
            RosterYield::SelfStale {
                last_beat_secs: 1200,
                ttl_secs: 900,
            },
            RosterYield::StaleGeneration {
                observed: dt(NOW),
                newest: dt(NOW),
            },
            RosterYield::NotSettled {
                settled_secs: 60,
                settle_secs: 900,
            },
            RosterYield::NoGeneration,
            RosterYield::Joining {
                age_secs: 60,
                ttl_secs: 900,
            },
            RosterYield::NotInRing,
        ];
        for reason in reasons {
            assert!(!reason.label().is_empty());
            assert!(!reason.describe().is_empty(), "{:?}", reason.label());
        }
    }

    // ---- Generation high-water mark (condition 2's only state) ----

    #[test]
    #[serial]
    fn the_generation_high_water_mark_ratchets_upward() {
        clear_generation_fence_for_tests();
        let ids: BTreeSet<u64> = [1, 2, 3].into_iter().collect();
        let newer = dt("2026-01-01T05:00:00Z");
        assert_eq!(observe_generation(&ids, newer), newer);
        // An older reading over the SAME comment set does not lower it.
        assert_eq!(observe_generation(&ids, dt("2026-01-01T00:00:00Z")), newer);
        clear_generation_fence_for_tests();
    }

    #[test]
    #[serial]
    fn the_generation_high_water_mark_resets_when_a_comment_disappears() {
        // Without this reset a host that saw a record which later vanished
        // (an operator tidying the roster issue, or a host republishing after
        // eviction) would yield FOREVER against a generation no live comment
        // set can reach again.
        clear_generation_fence_for_tests();
        let ids: BTreeSet<u64> = [1, 2, 3].into_iter().collect();
        let high = dt("2026-01-01T05:00:00Z");
        assert_eq!(observe_generation(&ids, high), high);
        let shrunk: BTreeSet<u64> = [1, 2].into_iter().collect();
        let lower = dt("2026-01-01T00:00:00Z");
        assert_eq!(
            observe_generation(&shrunk, lower),
            lower,
            "a vanished record must reset the ratchet, not deadlock the host"
        );
        clear_generation_fence_for_tests();
    }

    #[test]
    #[serial]
    fn admit_folds_the_high_water_mark_into_the_pure_fence() {
        clear_generation_fence_for_tests();
        let now = dt(NOW);
        let snapshot = |comments: Vec<RosterComment>| RosterSnapshot {
            issue: RosterIssueRef::parse("rjwalters/loom#1234").unwrap(),
            host: "host-a".to_string(),
            comments,
            ttl_secs: 900,
            settle_secs: 900,
            fetched_at: now,
        };
        let baseline = snapshot(settled_fleet_at(now));
        assert!(matches!(
            admit(&baseline, 1, now),
            RosterAdmission::Ring {
                index: 0,
                count: 3,
                ..
            }
        ));
        // host-c dies; well past its expiry + settle, the survivors act under
        // the newer generation (its eviction boundary).
        let later = now + ChronoDuration::seconds(3600);
        let mut dead_c = settled_fleet_at(later);
        dead_c[2].updated_at = later - ChronoDuration::seconds(1900);
        assert!(matches!(
            admit(&snapshot(dead_c), 1, later),
            RosterAdmission::Ring { count: 2, .. }
        ));
        // A stale read now replays the older view: discarded, never acted on.
        assert!(
            matches!(
                admit(&baseline, 1, now),
                RosterAdmission::Yield(RosterYield::StaleGeneration { .. })
            ),
            "a replayed older view must be discarded by the high-water mark"
        );
        clear_generation_fence_for_tests();
    }

    // ---- Write side: when a record is replaced rather than patched ----

    #[test]
    fn a_live_record_with_an_unchanged_serves_set_is_patched_in_place() {
        let serves: BTreeSet<u64> = [1].into_iter().collect();
        let existing =
            comment_id(7, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:59:00Z");
        assert_eq!(
            resolve_publish_action(Some(&existing), &serves, dt(NOW), 900),
            RosterPublish::Patch { id: 7 }
        );
    }

    #[test]
    fn a_first_heartbeat_creates_a_record() {
        let serves: BTreeSet<u64> = [1].into_iter().collect();
        assert_eq!(resolve_publish_action(None, &serves, dt(NOW), 900), RosterPublish::Create);
    }

    #[test]
    fn an_expired_record_is_replaced_so_the_rejoin_gets_a_fresh_boundary() {
        // Patching an expired record in place would resurrect this host into
        // every peer's ring with NO membership boundary and no settle window
        // -- an unfenced ring change, which is the one thing the generation
        // fence cannot absorb.
        let serves: BTreeSet<u64> = [1].into_iter().collect();
        let stale = comment_id(7, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:00:00Z");
        assert_eq!(
            resolve_publish_action(Some(&stale), &serves, dt(NOW), 900),
            RosterPublish::Republish {
                id: 7,
                reason: RosterRepublishReason::Expired,
            }
        );
    }

    #[test]
    fn a_changed_serves_set_is_replaced_for_the_same_reason() {
        let serves: BTreeSet<u64> = [1, 2].into_iter().collect();
        let existing =
            comment_id(7, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:59:00Z");
        assert_eq!(
            resolve_publish_action(Some(&existing), &serves, dt(NOW), 900),
            RosterPublish::Republish {
                id: 7,
                reason: RosterRepublishReason::ServesChanged,
            }
        );
    }

    #[test]
    fn own_comment_picks_the_freshest_record_for_this_host() {
        let comments = vec![
            comment_id(1, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:00:00Z"),
            comment_id(2, "host-a", &[1], "2026-01-01T08:00:00Z", "2026-01-01T09:59:00Z"),
        ];
        assert_eq!(own_comment(&comments, "host-a").map(|c| c.id), Some(2));
        assert_eq!(own_comment(&comments, "host-z"), None);
    }

    #[test]
    #[serial]
    fn settle_secs_is_floored_at_ttl_secs() {
        // The no-overlap argument needs settle >= ttl: a host holding a stale
        // view keeps acting until its OWN record expires (up to ttl after its
        // last read), so nobody may act under a new ring before then.
        let _e = EnvGuard::capture();
        let block = serde_json::json!({
            "enabled": true,
            "issue": "rjwalters/loom#42",
            "ttlSecs": 1800,
            "settleSecs": 60,
        });
        let config = resolve_roster_config_from(Some(&block));
        assert_eq!(config.ttl_secs, 1800);
        assert_eq!(config.settle_secs, 1800);
    }

    // ---- Snapshot cache ----

    #[test]
    #[serial]
    fn snapshot_cache_round_trips() {
        clear_roster_snapshot_for_tests();
        assert!(roster_snapshot().is_none());
        let issue = RosterIssueRef::parse("rjwalters/loom#1234").unwrap();
        set_roster_snapshot(RosterSnapshot {
            issue: issue.clone(),
            host: "host-a".to_string(),
            comments: fixture(),
            ttl_secs: 900,
            settle_secs: 900,
            fetched_at: Utc::now(),
        });
        let snap = roster_snapshot().expect("was just set");
        assert_eq!(snap.issue, issue);
        assert_eq!(snap.comments.len(), 3);
        clear_roster_snapshot_for_tests();
        assert!(roster_snapshot().is_none());
    }
}
