//! Forge-backed role-runner host roster (Issue #7690, Phase A of #6704).
//!
//! **Observational only.** Nothing in this module feeds [`super::decide`] or
//! [`super::resolve_posture`] — the static ring (#6374) is completely
//! unchanged by anything here. This module publishes and expires a roster
//! comment per host, exposes pure `members`/`ring`/`gen` functions over a
//! fixed comment set + instant (the fencing rule Phase B will build on), and
//! feeds `loom-daemon status`'s roster section. See
//! `defaults/docs/role-runner-roster.md` for the full design record this
//! implements — that document is the spec; this module is the
//! implementation of its "Phase A" row.
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
//! property Phase B's fencing rule rests on.

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

/// The pure core of [`resolve_roster_config`], split out so tests can drive
/// it with a synthetic config block.
fn resolve_roster_config_from(block: Option<&serde_json::Value>) -> RosterConfig {
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
    let settle_secs = resolve_u64_knob(ROSTER_SETTLE_SECS_ENV, block, ROSTER_SETTLE_SECS_KEY)
        .filter(|&v| v > 0)
        .unwrap_or(ROSTER_DEFAULT_SETTLE_SECS);

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
         This is host `{host}`'s role-runner **roster record** (Issue #6704 Phase A, #7690) — \
         one comment per host on this designated roster issue. Its liveness signal is this \
         comment's own forge-assigned `updated_at`, never a timestamp embedded in this text. See \
         `defaults/docs/role-runner-roster.md` for the full format contract. **Observational \
         only**: nothing consumes this roster for ownership yet — the role-runner shard ring is \
         still the static one from #6374 until Phase B ships.\n\n\
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
